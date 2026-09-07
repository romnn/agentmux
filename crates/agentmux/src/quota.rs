//! What each account has left, in the vendor's own words.
//!
//! A rate limit is the one failure whose remedy depends on information the failure does not carry:
//! which *other* account could answer this question now.
//! Both CLIs can be asked, for free and without spending a turn, so this module asks them.
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

use crate::config::{Account, Config, home_dir};
use crate::delegate::{AccountAlias, Vendor, base_env, credential_vars};

/// How long a single account's probe may take before it is abandoned.
///
/// Generous next to the sub-second reads measured from both vendors, because the cost of waiting
/// is one slow answer and the cost of giving up early is no answer at all.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// How old Claude's cached figures may be before the CLI is asked to refresh them.
///
/// The vendor writes that cache at most once every five minutes, so refreshing more eagerly than
/// this cannot produce newer numbers and only costs a process launch.
const MAX_CACHE_AGE: Duration = Duration::from_secs(300);

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
    },
    /// Fetched live from the vendor at the moment of asking.
    Live,
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
/// An account that authenticates with a key has no subscription window at all, and probing it
/// would answer with the *default* account's figures under this account's name — a caller would
/// then route work by another identity's remaining capacity.
/// So the target is decided once, from the same fields the delegate launch reads, and a key-only
/// account is never asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeTarget {
    /// A logged-in profile whose window can be read: the CLI's own, or the directory the
    /// identity's environment names.
    Window {
        /// The configuration directory the identity runs against, when it names one.
        config_dir: Option<PathBuf>,
    },
    /// An identity that supplies a credential or an endpoint, so it is billed per token and has
    /// no window of the vendor's to report.
    Credential,
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
        let env = crate::delegate::resolve_identity(vendor, alias, config, host_env)?.env;
        let vars = credential_vars(vendor);
        // Both CLIs prefer a credential in the environment over a stored login, and an endpoint
        // of its own has no vendor window at all.
        let credentialed = env.contains_key(vars.api_key)
            || env.contains_key(vars.base_url)
            || vars.auth_token.is_some_and(|name| env.contains_key(name));
        if credentialed {
            return Ok(Self::Credential);
        }
        Ok(Self::Window {
            config_dir: env.get(vars.config_dir).map(PathBuf::from),
        })
    }

    /// The configuration directory the probe should point the CLI at, if any.
    #[must_use]
    pub fn config_dir(&self) -> Option<&Path> {
        match self {
            Self::Window { config_dir } => config_dir.as_deref(),
            Self::Credential => None,
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
/// The CLI's own default configuration is included as an unnamed account whenever the vendor has
/// no configured accounts, so a machine with no `agentmux.toml` still gets an answer.
#[must_use]
pub fn probe_vendor(
    vendor: Vendor,
    config: &Config,
    host_env: &BTreeMap<String, String>,
    probe: &dyn QuotaProbe,
) -> Vec<AccountQuota> {
    // A machine with no configuration still has one account: whatever the CLI is logged into.
    // Aliases were validated when the file was read, so every key parses.
    let accounts = config.accounts(vendor);
    let targets: Vec<(Option<AccountAlias>, Option<&Account>)> = if accounts.is_empty() {
        vec![(None, None)]
    } else {
        accounts
            .iter()
            .filter_map(|(alias, account)| {
                AccountAlias::parse(alias)
                    .ok()
                    .map(|alias| (Some(alias), Some(account)))
            })
            .collect()
    };

    std::thread::scope(|scope| {
        let handles: Vec<_> = targets
            .into_iter()
            .map(|(alias, account)| {
                let handle = scope.spawn({
                    let alias = alias.clone();
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
                (handle, alias, account)
            })
            .collect();
        handles
            .into_iter()
            .map(|(handle, alias, account)| AccountQuota {
                vendor,
                account: alias,
                description: account.and_then(|a| a.description.clone()),
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
        if request.target == ProbeTarget::Credential {
            // Reported rather than skipped: a caller comparing accounts needs to see that this one
            // exists and simply has no window, not to find it missing from the list.
            return Observation::Unavailable {
                reason: "this account authenticates with a key rather than a logged-in profile, \
                         so it has no subscription window to report — its usage is billed per \
                         token"
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
            Vendor::Claude => probe_claude(request),
            Vendor::Codex => probe_codex(request),
        }
    }
}

/// The environment a probe child runs with: the delegate's own base, plus the directory that
/// selects the account.
///
/// The same base as a delegate, so a probe on a managed network has the proxy and CA settings a
/// delegate has, and a probe on Windows has what a process there needs to start at all.
fn probe_command(request: &ProbeRequest<'_>, program: &str) -> std::process::Command {
    let mut command = std::process::Command::new(program);
    command.env_clear();
    for (key, value) in base_env(request.host_env) {
        command.env(key, value);
    }
    if let Some(dir) = request.target.config_dir() {
        command.env(credential_vars(request.vendor).config_dir, dir);
    }
    command
}

/// Read Claude's own usage cache.
///
/// No process is spawned: the CLI writes these figures to `.claude.json` inside whichever
/// configuration directory it authenticated from, and reading them costs nothing and cannot fail
/// in a way that spends money.
/// The trade is staleness, which the vendor's own `fetchedAtMs` reports rather than agentmux
/// guessing.
fn probe_claude(request: &ProbeRequest<'_>) -> Observation {
    let home = home_dir(request.host_env);
    // Without `CLAUDE_CONFIG_DIR` the CLI keeps this file at the root of the home directory, not
    // under `~/.claude`.
    let path = match request.target.config_dir() {
        Some(dir) => dir.join(".claude.json"),
        None => match &home {
            Some(home) => home.join(".claude.json"),
            None => {
                return Observation::Unavailable {
                    reason: "neither an account config_dir nor HOME is known".to_owned(),
                };
            }
        },
    };

    // Refreshing first, only when the figures are old enough for it to change anything.
    // A weekly window moves with every other session on the account, so figures from hours ago
    // answer a question nobody asked.
    if is_stale(read_cached_usage(&path).as_ref()) {
        refresh_claude_cache(request);
    }

    let Some(cached) = read_cached_usage(&path) else {
        return Observation::Unavailable {
            reason: format!(
                "{} holds no usage figures. That account may not be logged in on this machine.",
                path.display()
            ),
        };
    };

    Observation::Reported {
        origin: Origin::Cache {
            fetched_at_ms: cached
                .get("fetchedAtMs")
                .and_then(serde_json::Value::as_i64),
            path,
        },
        payload: cached,
    }
}

/// Read the usage figures the Claude CLI caches inside a configuration directory.
fn read_cached_usage(path: &Path) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(path).ok()?;
    let document = serde_json::from_str::<serde_json::Value>(&text).ok()?;
    document.get("cachedUsageUtilization").cloned()
}

/// Whether a set of cached figures is old enough to be worth refreshing.
fn is_stale(cached: Option<&serde_json::Value>) -> bool {
    let Some(fetched_at_ms) = cached
        .and_then(|c| c.get("fetchedAtMs"))
        .and_then(serde_json::Value::as_i64)
    else {
        return true;
    };
    let Some(fetched_at) = chrono::DateTime::from_timestamp_millis(fetched_at_ms) else {
        return true;
    };
    chrono::Utc::now()
        .signed_duration_since(fetched_at)
        .to_std()
        .is_ok_and(|age| age > MAX_CACHE_AGE)
}

/// Ask the Claude CLI to refresh its usage cache.
///
/// `/usage` is answered locally: the run reports `total_cost_usd: 0` and no API duration, so this
/// spends nothing.
/// Its output is discarded because the structured figures land in the cache, and the printed text
/// omits scoped limits that the cache keeps.
fn refresh_claude_cache(request: &ProbeRequest<'_>) {
    use std::process::Stdio;

    let mut command = probe_command(request, Vendor::Claude.program());
    command.args(["--print", "/usage", "--output-format", "text"]);
    let Ok(mut child) = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return;
    };
    // A refresh that does not finish in time is not worth reporting: the cached figures are read
    // either way, and their own timestamp says how old they are.
    let deadline = std::time::Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// Ask Codex over the app-server protocol.
///
/// Codex publishes nothing to disk outside a session's rollout, so this is the only way to read it
/// without spending a turn.
/// It is a short-lived child speaking JSON-RPC on stdio, and it costs no tokens.
fn probe_codex(request: &ProbeRequest<'_>) -> Observation {
    match codex_rate_limits(request) {
        Ok(payload) => Observation::Reported {
            origin: Origin::Live,
            payload,
        },
        Err(reason) => Observation::Unavailable { reason },
    }
}

/// One JSON-RPC exchange with `codex app-server`.
fn codex_rate_limits(request: &ProbeRequest<'_>) -> Result<serde_json::Value, String> {
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::process::Stdio;

    let mut command = probe_command(request, Vendor::Codex.program());
    command.arg("app-server");

    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("cannot run `codex app-server`: {error}"))?;

    let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        let _ = child.kill();
        return Err("codex app-server did not offer stdio".to_owned());
    };

    // The exchange runs on its own thread so a server that never answers cannot hang the caller.
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut write = || -> std::io::Result<()> {
            writeln!(
                stdin,
                r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"clientInfo":{{"name":"agentmux","version":"1"}}}}}}"#
            )?;
            writeln!(
                stdin,
                r#"{{"jsonrpc":"2.0","method":"initialized","params":{{}}}}"#
            )?;
            writeln!(
                stdin,
                r#"{{"jsonrpc":"2.0","id":2,"method":"account/rateLimits/read","params":{{}}}}"#
            )?;
            stdin.flush()
        };
        if write().is_err() {
            let _ = sender.send(Err("codex app-server closed its input".to_owned()));
            return;
        }
        // The server interleaves unsolicited notifications with replies, so the response is found
        // by matching the request id rather than by taking the next line.
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    let _ = sender.send(Err(
                        "codex app-server ended without answering. That account may not be \
                         logged in."
                            .to_owned(),
                    ));
                    return;
                }
                Ok(_) => {}
                Err(error) => {
                    let _ = sender.send(Err(format!("reading codex app-server: {error}")));
                    return;
                }
            }
            let Ok(message) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if message.get("id").and_then(serde_json::Value::as_i64) != Some(2) {
                continue;
            }
            if let Some(error) = message.get("error") {
                let _ = sender.send(Err(format!(
                    "codex app-server refused: {}",
                    error
                        .get("message")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("no reason given")
                )));
                return;
            }
            let _ = sender.send(
                message
                    .get("result")
                    .cloned()
                    .ok_or_else(|| "codex app-server answered without a result".to_owned()),
            );
            return;
        }
    });

    let answer = receiver.recv_timeout(PROBE_TIMEOUT);
    // The child is finished with either way; leaving it running would leak a process per probe.
    let _ = child.kill();
    let _ = child.wait();
    match answer {
        Ok(result) => result,
        Err(_) => Err(format!(
            "codex app-server did not answer within {}s",
            PROBE_TIMEOUT.as_secs()
        )),
    }
}

/// The rate-limit figures Codex wrote while running one turn.
///
/// Codex reports nothing about usage on the stream agentmux captures, but it records it in the
/// session rollout it writes anyway.
/// Reading that afterwards costs nothing and needs no second process, which is the only reason a
/// Codex consultation can report a usage window at all.
///
/// `thread_id` is the identifier the stream already yielded, and the rollout file carrying it is
/// found by name.
#[must_use]
pub fn codex_rollout_rate_limits(codex_home: &Path, thread_id: &str) -> Option<serde_json::Value> {
    let rollout = find_rollout(&codex_home.join("sessions"), thread_id)?;
    let text = std::fs::read_to_string(rollout).ok()?;
    // The last record wins: each is a snapshot, and the newest is the state the turn ended in.
    text.lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find_map(|record| {
            record
                .get("payload")
                .and_then(|p| p.get("rate_limits"))
                .filter(|limits| !limits.is_null())
                .cloned()
        })
}

/// Find the rollout file for `thread_id`.
///
/// Rollouts are filed under `sessions/YYYY/MM/DD/` as `rollout-<timestamp>-<thread_id>.jsonl`,
/// so the search is a bounded walk rather than a glob dependency.
/// The id is matched as the whole tail of the name, not as a substring: a thread id is opaque,
/// and an opaque id that happened to be short would otherwise match every file on the day.
fn find_rollout(sessions: &Path, thread_id: &str) -> Option<PathBuf> {
    fn walk(dir: &Path, suffix: &str, depth: usize) -> Option<PathBuf> {
        if depth > 4 {
            return None;
        }
        let mut directories = Vec::new();
        let mut matches = Vec::new();
        for entry in std::fs::read_dir(dir).ok()?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                directories.push(path);
            } else if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(suffix))
            {
                matches.push(path);
            }
        }
        // Newest first at both levels: names lead with a timestamp, so the last one sorts last,
        // and a repeated thread id resolves to the most recent run.
        if let Some(newest) = matches.into_iter().max() {
            return Some(newest);
        }
        directories.sort_unstable();
        directories
            .into_iter()
            .rev()
            .find_map(|child| walk(&child, suffix, depth + 1))
    }
    walk(sessions, &format!("-{thread_id}.jsonl"), 0)
}
