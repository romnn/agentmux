//! What each account reports, and what agentmux refuses to conclude from it.
//!
//! The dangerous failure here is not a wrong number but a missing one read as a good one: an
//! account that could not be asked must never look like an account with capacity to spare.

use std::collections::BTreeMap;
use std::path::PathBuf;

use agentmux::config::{Account, Config, Defaults};
use agentmux::delegate::{AccountAlias, Vendor};
use agentmux::quota::{
    AccountQuota, Observation, Origin, ProbeRequest, ProbeTarget, QuotaProbe, SystemProbe,
    probe_vendor,
};
// Only the refresh tests read it, and they drive a shell script, so on Windows it would be unused.
#[cfg(unix)]
use agentmux::quota::Refresh;
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
            origin: Origin::Live,
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
/// Claude prefers a key in the environment over a stored login, so the delegate is billed per
/// token, and the profile's subscription figures would be reported under a name that is not
/// spending them.
/// Codex does not, which is why this is a Claude account and not a shared rule.
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

    let reported = probe_vendor(Vendor::Claude, &config, &host_env(), &SystemProbe);

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

/// A Codex `chatgpt` login outranks a key the environment merely happens to export.
///
/// `OPENAI_API_KEY` is exported on plenty of machines by something that is not agentmux, and Codex
/// ignores it in favour of the login its `auth.json` records — measured: a delegate launched with
/// both is refused by the API as a `chatgpt` account.
/// Reading the key alone would report every such account as having no window, which is the one
/// thing `quota` exists to say, withheld because of a variable nobody set for agentmux.
#[gtest]
fn a_codex_chatgpt_login_outranks_a_key_in_the_environment() -> Result<()> {
    let target = codex_target_with(
        Some(r#"{"auth_mode":"chatgpt","tokens":{"access_token":"t"}}"#),
        None,
    )?;

    assert_that!(target, matches_pattern!(ProbeTarget::Window { .. }));
    Ok(())
}

/// A Codex account whose stored login *is* the key is still billed per token.
///
/// The rule reads what the vendor wrote down rather than assuming a precedence in either
/// direction, so `apikey` must still answer `Credential`.
#[gtest]
fn a_codex_apikey_login_still_reports_no_window() -> Result<()> {
    let target = codex_target_with(Some(r#"{"auth_mode":"apikey"}"#), None)?;

    assert_that!(target, eq(&ProbeTarget::Credential));
    Ok(())
}

/// An endpoint of the identity's own has no vendor window whatever the stored login says.
///
/// The login decides *which* identity pays; an endpoint decides that the vendor is not the one
/// being paid, so it answers first and a `chatgpt` login must not talk it round.
#[gtest]
fn a_codex_endpoint_outranks_even_a_stored_login() -> Result<()> {
    let target = codex_target_with(
        Some(r#"{"auth_mode":"chatgpt","tokens":{"access_token":"t"}}"#),
        Some("http://localhost:11434/v1"),
    )?;

    assert_that!(target, eq(&ProbeTarget::Credential));
    Ok(())
}

/// A Codex key with no stored login behind it is billed per token.
///
/// Nothing on disk says the vendor would prefer anything else, and a vendor that says nothing is
/// not guessed at.
#[gtest]
fn a_codex_key_with_no_stored_login_reports_no_window() -> Result<()> {
    let target = codex_target_with(None, None)?;

    assert_that!(target, eq(&ProbeTarget::Credential));
    Ok(())
}

/// Resolve the target of a Codex account that supplies a key, against whatever login is on disk.
///
/// The key is in the host environment as well as on the account, because that is the machine this
/// guards: one where something unrelated exported `OPENAI_API_KEY`.
fn codex_target_with(auth_json: Option<&str>, base_url: Option<&str>) -> Result<ProbeTarget> {
    let home = tempfile::tempdir().or_fail()?;
    if let Some(auth_json) = auth_json {
        std::fs::write(home.path().join("auth.json"), auth_json).or_fail()?;
    }
    let config = config_with(
        "codex",
        &[(
            "personal",
            Account {
                config_dir: Some(home.path().to_path_buf()),
                api_key_env: Some("OPENAI_API_KEY".to_owned()),
                base_url: base_url.map(str::to_owned),
                ..Account::default()
            },
        )],
    );
    let mut env = host_env();
    env.insert(
        "OPENAI_API_KEY".to_owned(),
        "sk-exported-by-something".to_owned(),
    );
    let alias = AccountAlias::parse("personal").or_fail()?;

    ProbeTarget::of(Vendor::Codex, Some(&alias), &config, &env).or_fail()
}

/// The identity an unnamed consultation spends is probed alongside the named accounts.
///
/// With accounts configured but none of them the default, a consultation that names no `account`
/// runs as the CLI's own login.
/// Reporting only the named accounts hides the identity actually paying behind a list of the ones
/// that are not, which is the same mistake as reading a failed probe as spare capacity.
#[gtest]
fn the_cli_login_is_probed_when_no_configured_account_is_the_default() {
    let config = config_with("claude", &[("work", Account::default())]);
    let probe = ScriptedProbe {
        payload: serde_json::json!({"utilization": {"limits": []}}),
    };

    let reported = probe_vendor(Vendor::Claude, &config, &host_env(), &probe);

    // Two entries arrive: the named account, and the unnamed login an omitted `account` would
    // resolve to.
    assert_that!(reported.len(), eq(2));
    assert_that!(
        reported
            .iter()
            .filter(|entry| entry.account.is_none())
            .count(),
        eq(1)
    );
}

/// A configured default leaves the CLI's own login out, because nothing would spend it.
///
/// The unnamed login is reported for being the effective default, not for existing: once an alias
/// holds that role every consultation resolves to a named account, and listing the login as well
/// would offer a window no work is drawn against.
#[gtest]
fn a_configured_default_leaves_the_cli_login_out() {
    let mut config = config_with("claude", &[("work", Account::default())]);
    config.defaults.insert(
        "claude".to_owned(),
        Defaults {
            account: Some("work".to_owned()),
        },
    );
    let probe = ScriptedProbe {
        payload: serde_json::json!({"utilization": {"limits": []}}),
    };

    let reported = probe_vendor(Vendor::Claude, &config, &host_env(), &probe);

    assert_that!(reported.len(), eq(1));
    assert_that!(
        reported[0].account.as_ref().map(ToString::to_string),
        some(eq("work"))
    );
}

/// A default naming an account the file does not define is reported as the launch would refuse it.
///
/// The unnamed entry resolves the same identity a consultation naming no `account` would, so it
/// carries the launch's own refusal rather than standing in for a login nothing would spend.
#[gtest]
fn a_default_naming_an_undefined_account_carries_the_launch_refusal() -> Result<()> {
    let mut config = config_with("claude", &[("work", Account::default())]);
    config.defaults.insert(
        "claude".to_owned(),
        Defaults {
            account: Some("missing".to_owned()),
        },
    );
    let probe = ScriptedProbe {
        payload: serde_json::json!({"utilization": {"limits": []}}),
    };

    let reported = probe_vendor(Vendor::Claude, &config, &host_env(), &probe);

    let Some(unnamed) = reported.iter().find(|entry| entry.account.is_none()) else {
        return fail!("the unnamed login was not reported");
    };
    let Observation::Unavailable { reason } = &unnamed.observation else {
        return fail!("an undefined default cannot report figures");
    };
    assert_that!(reason, contains_substring("missing"));
    Ok(())
}

/// A refresh that rewrote the cache counts as one, however the process ended.
///
/// The CLI writes its figures before it exits, so an unhappy exit after the write would otherwise
/// label current figures as ones that predate the attempt.
#[cfg(unix)]
#[gtest]
fn a_refresh_that_rewrote_the_cache_is_a_refresh_however_the_process_ended() -> Result<()> {
    let observation = probe_claude_with_fake_cli(indoc::indoc! {r#"
        #!/bin/sh
        printf '%s' '{"cachedUsageUtilization":{"fetchedAtMs":2000,"utilization":{"limits":[]}}}' \
            > "$CLAUDE_CONFIG_DIR/.claude.json"
        exit 1
    "#})?;

    let Observation::Reported {
        origin:
            Origin::Cache {
                refresh,
                fetched_at_ms,
                ..
            },
        ..
    } = observation
    else {
        return fail!("a cache the fake CLI rewrote must be reported");
    };
    // The rewritten figures are served, and the refresh is not blamed on the exit status.
    assert_that!(fetched_at_ms, some(eq(2000)));
    assert_that!(refresh, eq(&Refresh::Succeeded));
    Ok(())
}

/// A refresh that left the cache as it was is reported by how its process ended.
#[cfg(unix)]
#[gtest]
fn a_refresh_that_left_the_cache_alone_reports_how_it_ended() -> Result<()> {
    let observation = probe_claude_with_fake_cli(indoc::indoc! {"
        #!/bin/sh
        exit 1
    "})?;

    let Observation::Reported {
        origin:
            Origin::Cache {
                refresh,
                fetched_at_ms,
                ..
            },
        ..
    } = observation
    else {
        return fail!("the stale cache must still be reported");
    };
    // The stale figures are served under a refresh that names the failure.
    assert_that!(fetched_at_ms, some(eq(1000)));
    let Refresh::Failed { reason } = refresh else {
        return fail!("a refresh that exited unhappily without writing must be a failure");
    };
    assert_that!(reason, contains_substring("exit status: 1"));
    Ok(())
}

/// Drive the real Claude probe against a fake `claude` on `PATH`, with a stale cache in place.
///
/// The cache carries a timestamp old enough to force a refresh, and the script decides what that
/// refresh does to the file.
#[cfg(unix)]
fn probe_claude_with_fake_cli(script: &str) -> Result<Observation> {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempfile::tempdir().or_fail()?;
    let config_dir = root.path().join("claude");
    let bin = root.path().join("bin");
    std::fs::create_dir_all(&config_dir).or_fail()?;
    std::fs::create_dir_all(&bin).or_fail()?;
    std::fs::write(
        config_dir.join(".claude.json"),
        r#"{"cachedUsageUtilization":{"fetchedAtMs":1000,"utilization":{"limits":[]}}}"#,
    )
    .or_fail()?;
    let program = bin.join("claude");
    std::fs::write(&program, script).or_fail()?;
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).or_fail()?;

    let mut env = host_env();
    env.insert("PATH".to_owned(), bin.display().to_string());
    Ok(SystemProbe.probe(&ProbeRequest {
        vendor: Vendor::Claude,
        target: ProbeTarget::Window {
            config_dir: Some(config_dir),
        },
        host_env: &env,
    }))
}
