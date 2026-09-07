//! What each account reports, and what agentmux refuses to conclude from it.
//!
//! The dangerous failure here is not a wrong number but a missing one read as a good one: an
//! account that could not be asked must never look like an account with capacity to spare.

use std::collections::BTreeMap;
use std::path::PathBuf;

use agentmux::config::{Account, Config};
use agentmux::delegate::Vendor;
use agentmux::quota::{
    AccountQuota, Observation, ProbeRequest, ProbeTarget, QuotaProbe, probe_vendor,
};
use googletest::prelude::*;

/// A probe that answers from a script instead of a machine.
#[derive(Debug)]
struct ScriptedProbe {
    payload: serde_json::Value,
}

impl QuotaProbe for ScriptedProbe {
    fn probe(&self, request: &ProbeRequest<'_>) -> Observation {
        if request.target == ProbeTarget::Credential {
            return Observation::Unavailable {
                reason: "custom endpoint".to_owned(),
            };
        }
        Observation::Reported {
            origin: agentmux::quota::Origin::Live,
            payload: self.payload.clone(),
        }
    }
}

fn host_env() -> BTreeMap<String, String> {
    [("HOME".to_owned(), "/home/dev".to_owned())]
        .into_iter()
        .collect()
}

fn config_with(vendor: &str, accounts: &[(&str, Account)]) -> Config {
    let mut config = Config::default();
    let table = config.accounts.entry(vendor.to_owned()).or_default();
    for (alias, account) in accounts {
        table.insert((*alias).to_owned(), account.clone());
    }
    config
}

/// Every configured account is asked, and each answer is labelled with the account it came from.
///
/// A caller choosing where to send work needs the mapping; a bare list of percentages is unusable.
#[gtest]
fn every_configured_account_is_reported_by_name() {
    let config = config_with(
        "claude",
        &[
            (
                "work",
                Account {
                    config_dir: Some(PathBuf::from("/home/dev/.claude")),
                    ..Account::default()
                },
            ),
            (
                "personal",
                Account {
                    config_dir: Some(PathBuf::from("/home/dev/.claude-personal")),
                    ..Account::default()
                },
            ),
        ],
    );
    let probe = ScriptedProbe {
        payload: serde_json::json!({"utilization": {"limits": []}}),
    };

    let reported = probe_vendor(Vendor::Claude, &config, &host_env(), &probe);

    let names: Vec<String> = reported
        .iter()
        .filter_map(|entry| entry.account.as_ref().map(ToString::to_string))
        .collect();
    assert_that!(names, unordered_elements_are![eq("work"), eq("personal")]);
}

/// A machine with no configuration still gets an answer, for the CLI's own default account.
#[gtest]
fn a_machine_without_configuration_still_reports_its_default_account() {
    let probe = ScriptedProbe {
        payload: serde_json::json!({"utilization": {"limits": []}}),
    };

    let reported = probe_vendor(Vendor::Claude, &Config::default(), &host_env(), &probe);

    assert_that!(reported.len(), eq(1));
    assert_that!(reported[0].account.is_none(), eq(true));
}

/// An account that authenticates against its own endpoint has no vendor window, and says so.
///
/// Probing it would otherwise answer with the *default* account's figures under this account's
/// name, which is worse than no answer.
#[gtest]
fn an_endpoint_account_reports_no_window_rather_than_someone_elses() {
    let config = config_with(
        "codex",
        &[(
            "local",
            Account {
                base_url: Some("http://localhost:11434/v1".to_owned()),
                ..Account::default()
            },
        )],
    );
    let probe = ScriptedProbe {
        payload: serde_json::json!({"rateLimitsByLimitId": {}}),
    };

    let reported = probe_vendor(Vendor::Codex, &config, &host_env(), &probe);

    assert_that!(
        reported[0].observation,
        matches_pattern!(Observation::Unavailable { .. })
    );
}

/// An account that names a profile directory but authenticates with a key has no window either.
///
/// Both CLIs prefer a key in the environment over a stored login, so the delegate is billed per
/// token, and the profile's subscription figures would be reported under a name that is not
/// spending them.
#[gtest]
fn a_profile_that_also_supplies_a_key_reports_no_window() {
    let config = config_with(
        "claude",
        &[(
            "ci",
            Account {
                config_dir: Some(PathBuf::from("/home/dev/.claude-ci")),
                api_key_env: Some("CI_ANTHROPIC_KEY".to_owned()),
                ..Account::default()
            },
        )],
    );
    let probe = ScriptedProbe {
        payload: serde_json::json!({"utilization": {"limits": []}}),
    };

    let reported = probe_vendor(Vendor::Claude, &config, &host_env(), &probe);

    assert_that!(
        reported[0].observation,
        matches_pattern!(Observation::Unavailable { .. })
    );
}

/// With no account named, the probe reads exactly the login a delegate without an account runs
/// as.
///
/// A no-account Codex delegate is handed the host's `CODEX_HOME`; a no-account Claude delegate is
/// deliberately not handed the host's `CLAUDE_CONFIG_DIR`, which under Claude Code names the
/// launching session's own identity.
/// The probe follows the same rule, or `quota` would report one login's window for a delegate
/// that spends another's.
#[gtest]
fn the_default_target_reads_the_login_a_delegate_would_run_as() {
    let mut env = host_env();
    env.insert(
        "CLAUDE_CONFIG_DIR".to_owned(),
        "/home/dev/.claude-work".to_owned(),
    );
    env.insert("CODEX_HOME".to_owned(), "/home/dev/.codex-work".to_owned());
    let config = Config::default();
    assert_that!(
        ProbeTarget::of(Vendor::Claude, None, &config, &env),
        ok(eq(&ProbeTarget::Window { config_dir: None }))
    );
    assert_that!(
        ProbeTarget::of(Vendor::Codex, None, &config, &env),
        ok(eq(&ProbeTarget::Window {
            config_dir: Some(PathBuf::from("/home/dev/.codex-work"))
        }))
    );
}

/// A key the host exports, or the machine's launch layer forwards, is what a no-account delegate
/// authenticates with — so the probe must not report the subscription window it would not spend.
#[gtest]
fn a_forwarded_key_makes_the_default_identity_a_credential() {
    let mut env = host_env();
    env.insert("ANTHROPIC_API_KEY".to_owned(), "sk-host".to_owned());
    assert_that!(
        ProbeTarget::of(Vendor::Claude, None, &Config::default(), &env),
        ok(eq(&ProbeTarget::Credential))
    );

    let mut config = Config::default();
    config.launch.env.insert(
        "OPENAI_API_KEY".to_owned(),
        agentmux::config::Secret::new("sk-launch"),
    );
    assert_that!(
        ProbeTarget::of(Vendor::Codex, None, &config, &host_env()),
        ok(eq(&ProbeTarget::Credential))
    );
}

/// A vendor field this build has never seen still reaches the caller.
///
/// The payload is passed through whole, so a new window kind or plan tier needs no release here —
/// the same rule that keeps model identifiers opaque.
#[gtest]
fn an_unknown_vendor_field_survives_the_round_trip() -> Result<()> {
    let entry = AccountQuota {
        vendor: Vendor::Claude,
        account: None,
        description: None,
        observation: Observation::Reported {
            origin: agentmux::quota::Origin::Live,
            payload: serde_json::json!({"some_window_invented_next_year": {"left": 12}}),
        },
    };

    let json = serde_json::to_string(&entry).or_fail()?;

    assert_that!(json, contains_substring("some_window_invented_next_year"));
    Ok(())
}

/// Codex's usage window is recovered from the rollout it writes while running a turn.
///
/// Nothing about rate limits reaches the stream agentmux captures, so without this a Codex
/// consultation could never report a window at all.
/// The rollout is written anyway, so reading it afterwards costs neither a process nor a token.
#[gtest]
fn a_codex_rollout_yields_the_window_the_stream_omitted() -> Result<()> {
    let home = tempfile::tempdir().or_fail()?;
    let day = home.path().join("sessions/2026/09/07");
    std::fs::create_dir_all(&day).or_fail()?;
    let thread = "01a0784a-915b-7d92-a381-b53d765296c4";
    std::fs::write(
        day.join(format!("rollout-2026-09-07T01-53-34-{thread}.jsonl")),
        indoc::indoc! {r#"
            {"type":"event_msg","payload":{"type":"token_count","rate_limits":{"limit_id":"codex","primary":{"used_percent":11.0,"window_minutes":10080,"resets_at":1789303382}}}}
            {"type":"event_msg","payload":{"type":"token_count","rate_limits":{"limit_id":"codex","primary":{"used_percent":53.0,"window_minutes":10080,"resets_at":1789303382}}}}
        "#},
    )
    .or_fail()?;

    let limits = agentmux::quota::codex_rollout_rate_limits(home.path(), thread)
        .expect("the rollout carries rate limits");

    // The last record wins: each is a snapshot and the newest is where the turn ended.
    assert_that!(
        limits["primary"]["used_percent"].as_f64(),
        some(approx_eq(53.0))
    );
    assert_that!(limits["limit_id"].as_str(), some(eq("codex")));
    Ok(())
}

/// A thread with no rollout is not an error, and not a zero.
#[gtest]
fn a_missing_rollout_yields_nothing_rather_than_a_default() -> Result<()> {
    let home = tempfile::tempdir().or_fail()?;
    std::fs::create_dir_all(home.path().join("sessions/2026/09/07")).or_fail()?;

    let limits = agentmux::quota::codex_rollout_rate_limits(home.path(), "no-such-thread");

    assert_that!(limits.is_none(), eq(true));
    Ok(())
}

/// Probing must not create the directory it is pointed at.
///
/// Both CLIs create a missing config directory and then report "not logged in".
/// A probe that let that happen would leave a wrong `config_dir` looking configured, and disarm
/// the pre-launch check whose whole purpose is to name the path rather than the credentials.
#[gtest]
fn probing_a_missing_config_dir_creates_nothing() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let missing = dir.path().join("never-created");
    let config = config_with(
        "claude",
        &[(
            "gone",
            Account {
                config_dir: Some(missing.clone()),
                ..Account::default()
            },
        )],
    );

    let reported = probe_vendor(
        Vendor::Claude,
        &config,
        &host_env(),
        &agentmux::quota::SystemProbe,
    );

    // Asserting the reason, not merely that something was unavailable: without the guard the probe
    // falls through to reading the cache and reports a missing *file*, which is the same verdict
    // reached for the wrong reason and after the CLI has been given a chance to create the
    // directory.
    let reason = match &reported[0].observation {
        Observation::Unavailable { reason } => reason.clone(),
        Observation::Reported { .. } => panic!("a missing directory cannot report figures"),
    };
    assert_that!(reason, contains_substring("does not exist"));
    assert_that!(reason, contains_substring("not been logged in"));
    assert_that!(
        missing.exists(),
        eq(false),
        "the probe created the account's config directory"
    );
    Ok(())
}
