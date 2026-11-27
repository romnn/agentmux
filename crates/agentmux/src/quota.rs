//! What each account has left, in the vendor's own words.
//!
//! A rate limit is the one failure whose remedy depends on information the failure does not carry:
//! which *other* account could answer this question now.
//! Every CLI can be asked, for free and without spending a turn, so this module asks them.
//!
//! # Nothing here is normalised
//!
//! The payloads are handed back exactly as the vendor produced them.
//! That is not laziness: a percentage means nothing without the plan tier behind it, and every
//! absolute figure — `limit_dollars`, `used_dollars`, `individual_limit` — comes back null on a
//! subscription account.
//! Ranking two accounts would require a table of what each tier is worth, which is the roster this
//! crate refuses to keep for model ids and refuses to keep here for the same reason.
//! The caller reads `severity` and decides; agentmux does not decide for it.
//!
//! # A failed probe is never "plenty left"
//!
//! An unauthenticated Claude configuration directory answers `/usage` cheerfully with zeroes.
//! Anything that ranked accounts on "least used" would therefore prefer the broken one, turning a
//! typo in a path into "always route to the account that cannot answer".
//! [`Observation::Unavailable`] exists so that outcome cannot be confused with an idle account.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::{Account, Config};
use crate::delegate::{AccountAlias, Vendor, base_env, credential_vars, scrub_billing_env};
use crate::vendor::{agy, claude, codex};

/// How long a single account's probe may take before it is abandoned.
///
/// Generous next to the sub-second reads measured from both vendors, because the cost of waiting
/// is one slow answer and the cost of giving up early is no answer at all.
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// What one account reported.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountQuota {
    /// Which CLI was asked.
    pub vendor: Vendor,
    /// Which configured account, or `None` for the CLI's own default configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<AccountAlias>,
    /// What the account is for, when the configuration says.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// What came back.
    pub observation: Observation,
}

/// The result of asking one account.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Observation {
    /// The vendor's payload, unmodified.
    Reported {
        /// Where the numbers came from, so a reader can judge how current they are.
        origin: Origin,
        /// The vendor's own JSON.
        ///
        /// Deliberately untyped: a new window, plan tier or limit kind must reach the caller
        /// without an agentmux release, exactly as a new model identifier does.
        payload: serde_json::Value,
    },
    /// The account could not be asked, or answered with something unusable.
    ///
    /// Never collapse this into zero usage.
    Unavailable {
        /// Why, in enough detail to fix it.
        reason: String,
    },
}

/// Where a reported figure came from.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Origin {
    /// Read from the CLI's own on-disk cache.
    ///
    /// The vendor refreshes it at most every five minutes, so `fetched_at_ms` is the only honest
    /// statement about how current the numbers are.
    Cache {
        /// The file read.
        path: PathBuf,
        /// The vendor's own timestamp on the cached figures, in milliseconds.
        #[serde(skip_serializing_if = "Option::is_none")]
        fetched_at_ms: Option<i64>,
        /// What the attempt to refresh the file did before it was read.
        refresh: Refresh,
    },
    /// Fetched live from the vendor at the moment of asking.
    Live,
}

/// What agentmux did about the staleness of a vendor's cache before reading it.
///
/// Reported rather than kept internal because the figures are served either way: a refresh that
/// timed out leaves numbers that look exactly like fresh ones, and a reader deciding whether to
/// trust a window needs to know which it is holding.
/// This is the same rule as [`Observation::Unavailable`] one level down: an answer that could
/// not be obtained must not be indistinguishable from one that was.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Refresh {
    /// The cached figures were recent enough to serve, so no process was spawned.
    Skipped,
    /// The cache was rewritten.
    ///
    /// Judged by the vendor's own timestamp on the figures advancing, not by the exit status
    /// alone: a CLI that wrote the file and then hung or exited unhappily still refreshed it.
    Succeeded,
    /// The CLI was still running at the deadline and was stopped, and the cache did not advance.
    ///
    /// Whatever was already cached is what gets reported.
    TimedOut,
    /// The CLI could not be run, or exited without the cache advancing.
    Failed {
        /// Why, in enough detail to fix it.
        reason: String,
    },
}

/// Asks one account what it has left.
///
/// A trait so the process and filesystem work stays behind a seam, matching
/// [`crate::launch::Launcher`]: tests drive quota rendering without a real CLI on the machine.
pub trait QuotaProbe: Send + Sync + std::fmt::Debug {
    /// Ask one account.
    ///
    /// Returns an [`Observation`] rather than a `Result` because one account failing must not stop
    /// the others being reported, and because the failure itself is the answer worth showing.
    fn probe(&self, request: &ProbeRequest<'_>) -> Observation;
}

/// Which identity a probe is asking about.
///
/// An account that authenticates with a key, or that its environment routes to another backend,
/// has no subscription window at all, and probing it would answer with the *default* account's
/// figures under this account's name — a caller would then route work by another identity's
/// remaining capacity.
/// So the target is decided once, from the same fields the delegate launch reads, and a key-only
/// account is never asked.
///
/// A key in the environment does not by itself make an account key-only: a vendor that records a
/// stored login of its own may prefer that login, and that vendor's own rule is what asks.
/// Getting the precedence backwards costs the window of every account on the machine, so it is
/// decided from what the vendor wrote down rather than from what happens to be exported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeTarget {
    /// A logged-in profile whose window can be read: the CLI's own, or the directory the
    /// identity's environment names.
    Window {
        /// The configuration directory the identity runs against, when it names one.
        config_dir: Option<PathBuf>,
    },
    /// An identity whose environment supplies an API credential, an endpoint, or a switch that
    /// routes it to another backend, so no subscription window of the vendor's describes it.
    NoSubscriptionWindow,
}

impl ProbeTarget {
    /// The target one identity resolves to: a named account, or the CLI's own login when there
    /// is none.
    ///
    /// Decided from the environment the delegate would actually be launched with, so a key the
    /// machine's `[launch]` layer forwards or the host exports classifies the identity exactly as
    /// it would authenticate.
    ///
    /// # Errors
    ///
    /// Returns the account's own launch error when it cannot be resolved, which is also why it
    /// cannot be probed.
    pub fn of(
        vendor: Vendor,
        alias: Option<&AccountAlias>,
        config: &Config,
        host_env: &BTreeMap<String, String>,
    ) -> Result<Self, crate::delegate::DelegateError> {
        let account = crate::delegate::AccountChoice::from(alias.cloned());
        let env = crate::delegate::resolve_identity(vendor, &account, config, host_env, None)?.env;
        Ok(match vendor {
            Vendor::Claude => claude::quota::probe_target(&env),
            Vendor::Codex => codex::quota::probe_target(&env, host_env),
            // agy reads no key or endpoint an account could set, and its gateway variables are
            // withheld from a subscription identity, so its one login is all there is to read.
            Vendor::Agy => Self::Window { config_dir: None },
        })
    }

    /// The configuration directory the probe should point the CLI at, if any.
    #[must_use]
    pub fn config_dir(&self) -> Option<&Path> {
        match self {
            Self::Window { config_dir } => config_dir.as_deref(),
            Self::NoSubscriptionWindow => None,
        }
    }
}

/// Everything a probe needs to reach one account.
#[derive(Debug)]
pub struct ProbeRequest<'a> {
    /// Which CLI to ask.
    pub vendor: Vendor,
    /// Whose figures to read.
    pub target: ProbeTarget,
    /// The host environment, for `HOME` and for anything the CLI needs to run.
    pub host_env: &'a BTreeMap<String, String>,
}

/// A probe that asks nothing and says so.
///
/// The library's default, so embedding agentmux never spawns a process that was not asked for.
/// `agentmux` the binary installs [`SystemProbe`] instead.
#[derive(Debug, Clone, Copy)]
pub struct DisabledProbe;

impl QuotaProbe for DisabledProbe {
    fn probe(&self, _request: &ProbeRequest<'_>) -> Observation {
        Observation::Unavailable {
            reason: "quota probing is not enabled in this process".to_owned(),
        }
    }
}

/// Ask every account this machine defines for one vendor, at once.
///
/// Accounts are probed in parallel because the work is entirely waiting: one Claude probe is a
/// file read and one Codex probe is a short-lived child process, and a machine with four accounts
/// should answer in the time of the slowest, not the sum.
///
/// The CLI's own login is included as an unnamed account whenever no defined account holds the
/// vendor's default, because that is the identity a consultation naming no `account` then spends.
/// A machine with no `agentmux.toml` reaches this the same way, and so still gets an answer.
#[must_use]
pub fn probe_vendor(
    vendor: Vendor,
    config: &Config,
    host_env: &BTreeMap<String, String>,
    probe: &dyn QuotaProbe,
) -> Vec<AccountQuota> {
    enum ProbeAccount<'a> {
        Default,
        Named {
            alias: AccountAlias,
            account: &'a Account,
        },
    }

    impl ProbeAccount<'_> {
        fn alias(&self) -> Option<&AccountAlias> {
            match self {
                Self::Default => None,
                Self::Named { alias, .. } => Some(alias),
            }
        }
    }

    struct PendingProbe<'scope, 'account> {
        handle: std::thread::ScopedJoinHandle<'scope, Observation>,
        account: ProbeAccount<'account>,
    }

    let mut accounts: Vec<ProbeAccount<'_>> = config
        .accounts(vendor)
        .iter()
        .map(|(alias, account)| ProbeAccount::Named {
            alias: alias.clone(),
            account,
        })
        .collect();
    // The unnamed login is asked whenever a consultation naming no `account` would resolve to
    // it, which is when no defined account holds the default: the same condition the launch
    // pins on.
    // A default naming an account the file does not define is reported the way a launch would
    // refuse it, because the unnamed probe resolves the same identity and carries the same error.
    // Listing only the named accounts would otherwise omit the one identity actually paying, and
    // a caller reading the list would choose between the accounts that are not.
    let default_is_defined = config
        .default_account(vendor)
        .is_some_and(|default| config.account(vendor, default.alias).is_some());
    if !default_is_defined {
        accounts.push(ProbeAccount::Default);
    }

    std::thread::scope(|scope| {
        let pending_probes: Vec<_> = accounts
            .into_iter()
            .map(|account| {
                let handle = scope.spawn({
                    let alias = account.alias().cloned();
                    move || match ProbeTarget::of(vendor, alias.as_ref(), config, host_env) {
                        Ok(target) => probe.probe(&ProbeRequest {
                            vendor,
                            target,
                            host_env,
                        }),
                        // An account that cannot be launched cannot be asked either, and the
                        // launch's own reason is the useful one.
                        Err(error) => Observation::Unavailable {
                            reason: error.to_string(),
                        },
                    }
                });
                PendingProbe { handle, account }
            })
            .collect();
        pending_probes
            .into_iter()
            .map(|PendingProbe { handle, account }| AccountQuota {
                vendor,
                description: match &account {
                    ProbeAccount::Default => None,
                    ProbeAccount::Named { account, .. } => account.description.clone(),
                },
                account: match account {
                    ProbeAccount::Default => None,
                    ProbeAccount::Named { alias, .. } => Some(alias),
                },
                // A probe that panicked is reported as unavailable rather than dropped: an
                // account missing from the list reads as "does not exist", which is a different
                // claim.
                observation: handle.join().unwrap_or_else(|_| Observation::Unavailable {
                    reason: "the probe for this account failed inside agentmux".to_owned(),
                }),
            })
            .collect()
    })
}

/// The real probe: reads Claude's cache, and asks Codex over its app-server protocol.
#[derive(Debug, Clone, Copy)]
pub struct SystemProbe;

impl QuotaProbe for SystemProbe {
    fn probe(&self, request: &ProbeRequest<'_>) -> Observation {
        if request.target == ProbeTarget::NoSubscriptionWindow {
            // Reported rather than skipped: a caller comparing accounts needs to see that this one
            // exists and simply has no window, not to find it missing from the list.
            return Observation::Unavailable {
                reason: "subscription quota unavailable for this account's API credential or \
                         routing environment"
                    .to_owned(),
            };
        }
        if let Some(dir) = request.target.config_dir()
            && !dir.is_dir()
        {
            // Checked before anything spawns, because both CLIs *create* the directory they are
            // pointed at and then report "not logged in".
            // A probe that let that happen would leave the account looking configured and disarm
            // the pre-launch check that names the wrong path.
            return Observation::Unavailable {
                reason: format!(
                    "{} does not exist. That account's `config_dir` is wrong, or it has never \
                     been logged in on this machine.",
                    dir.display()
                ),
            };
        }
        match request.vendor {
            Vendor::Claude => claude::quota::probe(request),
            Vendor::Codex => codex::quota::probe(request),
            Vendor::Agy => agy::quota::probe(request),
        }
    }
}

/// The environment a probe child runs with: the delegate's own base, plus the directory that
/// selects the account.
///
/// The same base as a delegate, so a probe on a managed network has the proxy and CA settings a
/// delegate has, and a probe on Windows has what a process there needs to start at all.
///
/// A probe reads a usage window and never needs a key, so its environment is scrubbed of every
/// billing variable whatever the account says: the probe of an account on API pricing is never
/// spawned at all, because [`ProbeTarget::NoSubscriptionWindow`] is answered without one.
pub(crate) fn probe_command(request: &ProbeRequest<'_>, program: &str) -> std::process::Command {
    crate::launch::keep_std_handles_from_children();
    let mut command = std::process::Command::new(program);
    command.env_clear();
    let mut env = base_env(request.host_env);
    scrub_billing_env(&mut env);
    for (key, value) in env {
        command.env(key, value);
    }
    if let (Some(dir), Some(vars)) = (request.target.config_dir(), credential_vars(request.vendor))
    {
        command.env(vars.config_dir, dir);
    }
    command
}
