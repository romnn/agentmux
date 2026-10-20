//! What a delegate loads and may do: the machine file's defaults and limits, how a request is held
//! to them, and what each CLI is then told.
//!
//! A limit is the owner's ceiling and a request beyond it is refused, never lowered to fit, so
//! these tests pin both halves: what a request that fits launches with, and that one that does not
//! fit launches nothing at all.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agentmux::config::{Config, ConfigError, PermissionRefusal};
use agentmux::delegate::{
    AccountAlias, AccountChoice, ClaudePermissionMode, ClaudePermissions, ClaudeTool,
    CodexPermissions, CodexSandbox, Delegate, DelegateError, Effort, Invocation, Isolation,
    ModelId, PermissionOptions, SessionRef, TurnPlan, Vendor, codex_supports_permission_profiles,
};
use agentmux::run::{Retention, RunError, RunStore, StartRequest};
use agentmux::testing::{Script, ScriptedLauncher, fixtures};
use googletest::prelude::*;

/// Where every machine file in these tests claims to live, so a refusal can be checked to name it.
const MACHINE_FILE: &str = "/home/dev/.config/agentmux/agentmux.toml";

/// The owner's own machine file, as issue #9 proposes it.
const OWNER_FILE: &str = indoc::indoc! {r#"
    [codex.defaults]
    ignore_user_config = false
    sandbox = "read_only"
    network_access = true

    [codex.limits]
    allow_user_config = true
    max_sandbox = "workspace_write"
    allow_network = true

    [claude.defaults]
    ignore_user_config = true
    permission_mode = "plan"

    [claude.limits]
    allowed_permission_modes = ["plan", "default", "acceptEdits", "dontAsk", "auto"]
"#};

/// A configuration parsed from `text`, as the machine file at [`MACHINE_FILE`].
fn machine(text: &str) -> Result<Config> {
    let mut config: Config = toml::from_str(text).or_fail()?;
    config.source = Some(PathBuf::from(MACHINE_FILE));
    Ok(config)
}

fn codex(sandbox: Option<CodexSandbox>, network_access: Option<bool>) -> Result<Delegate> {
    Ok(Delegate::Codex {
        model: ModelId::parse("gpt-6.1-sol").or_fail()?,
        effort: Some(Effort::parse("high").or_fail()?),
        permissions: CodexPermissions {
            sandbox,
            network_access,
        },
        account: AccountChoice::Default,
        isolation: None,
    })
}

fn claude(permissions: ClaudePermissions) -> Result<Delegate> {
    Ok(Delegate::Claude {
        model: ModelId::parse("claude-opus-5").or_fail()?,
        effort: Some(Effort::parse("xhigh").or_fail()?),
        account: AccountChoice::Default,
        isolation: None,
        permissions,
    })
}

fn tools(names: &[&str]) -> Result<Vec<ClaudeTool>> {
    ClaudeTool::parse_all(names).or_fail()
}

fn host_env() -> BTreeMap<String, String> {
    [("PATH", "/usr/bin"), ("HOME", "/home/dev")]
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect()
}

/// Build one turn of `delegate` under `config`, resuming `resume` when given.
fn build(
    delegate: &Delegate,
    config: &Config,
    resume: Option<&SessionRef>,
) -> std::result::Result<Invocation, DelegateError> {
    let extra_env = BTreeMap::new();
    let plan = TurnPlan {
        question_path: Path::new("/runs/r/turns/0000/question.md"),
        last_message_path: Path::new("/runs/r/turns/0000/last-message.md"),
        resume,
        extra_env: &extra_env,
        billing: None,
    };
    delegate.invocation(&plan, &host_env(), config)
}

/// Whether `args` holds `flag` followed by exactly `value`.
fn has_pair(args: &[String], flag: &str, value: &str) -> bool {
    args.windows(2).any(|pair| {
        pair.first().is_some_and(|f| f == flag) && pair.get(1).is_some_and(|v| v == value)
    })
}

/// The name of the permission profile `args` select, when they select one.
fn profile_of(args: &[String]) -> Option<String> {
    args.iter().find_map(|arg| {
        arg.strip_prefix("default_permissions=\"")
            .and_then(|rest| rest.strip_suffix('"'))
            .map(ToOwned::to_owned)
    })
}

/// The limit a refused launch names, when a limit refused it.
fn refusal_of(error: &DelegateError) -> Option<&PermissionRefusal> {
    match error {
        DelegateError::Permission(refusal) => Some(refusal),
        _ => None,
    }
}

/// The limit a refused consultation names, when a limit refused it.
fn run_refusal_of(error: &RunError) -> Option<&PermissionRefusal> {
    match error {
        RunError::Delegate(error) => refusal_of(error),
        _ => None,
    }
}

fn has(args: &[String], arg: &str) -> bool {
    args.iter().any(|candidate| candidate == arg)
}

/// The owner's file reads as written, says nothing this build does not read, and lists what it
/// allows in the one line every report prints.
#[gtest]
fn the_owners_file_reads_as_written() -> Result<()> {
    let home = tempfile::tempdir().or_fail()?;
    let path = home.path().join("agentmux.toml");
    agentmux::testing::write_config_file(&path, OWNER_FILE).or_fail()?;
    let env: BTreeMap<String, String> = [(
        "HOME".to_owned(),
        home.path().to_string_lossy().into_owned(),
    )]
    .into_iter()
    .collect();

    let config = Config::load(&env, home.path()).or_fail()?;

    assert_that!(config.unknown, is_empty());
    assert_that!(config.codex.defaults.network_access, some(eq(true)));
    // `default` is Claude Code's older name for its manual mode.
    assert_that!(
        config.claude.limits.allowed_permission_modes,
        some(contains(eq(&ClaudePermissionMode::Manual)))
    );
    assert_that!(
        config.describe_permissions(Vendor::Codex),
        eq(
            "ignore_user_config=false allow_user_config=true sandbox=read_only \
            network_access=true max_sandbox=workspace_write allow_network=true"
        )
    );
    assert_that!(
        config.describe_permissions(Vendor::Claude),
        eq(
            "ignore_user_config=true allow_user_config=true permission_mode=plan \
            allowed_permission_modes=manual,acceptEdits,plan,dontAsk,auto"
        )
    );
    Ok(())
}

/// Without a machine file, both vendors load the user's configuration, Codex runs read-only and
/// leaves its network to Codex, Claude runs in plan mode, and neither full access nor bypassing
/// Claude's permissions can be asked for.
#[gtest]
fn without_a_file_the_built_in_defaults_and_limits_apply() {
    let config = Config::default();
    assert_that!(
        config.describe_permissions(Vendor::Codex),
        eq(
            "ignore_user_config=false allow_user_config=true sandbox=read_only \
            network_access=unset max_sandbox=workspace_write allow_network=true"
        )
    );
    assert_that!(
        config.describe_permissions(Vendor::Claude),
        eq(
            "ignore_user_config=false allow_user_config=true permission_mode=plan \
            allowed_permission_modes=manual,acceptEdits,plan,dontAsk,auto"
        )
    );
}

/// A misspelled permission key is reported like any other, and the file still loads.
#[gtest]
fn a_misspelled_permission_key_is_reported() -> Result<()> {
    let home = tempfile::tempdir().or_fail()?;
    agentmux::testing::write_config_file(
        home.path().join("agentmux.toml"),
        indoc::indoc! {r#"
            [codex.limits]
            max_sandox = "read_only"
        "#},
    )
    .or_fail()?;
    let env: BTreeMap<String, String> = [(
        "HOME".to_owned(),
        home.path().to_string_lossy().into_owned(),
    )]
    .into_iter()
    .collect();

    let config = Config::load(&env, home.path()).or_fail()?;

    let keys: Vec<&str> = config.unknown.iter().map(|key| key.key.as_str()).collect();
    assert_that!(keys, elements_are![eq(&"codex.limits.max_sandox")]);
    Ok(())
}

/// A checked-out file may not decide what a delegate may do, in either direction: neither its
/// defaults nor its limits.
#[gtest]
fn a_project_file_that_sets_permissions_is_refused() -> Result<()> {
    for table in [
        indoc::indoc! {r#"
            [codex.defaults]
            sandbox = "workspace_write"
        "#},
        indoc::indoc! {"
            [codex.limits]
            allow_network = true
        "},
        indoc::indoc! {r#"
            [claude.defaults]
            permission_mode = "acceptEdits"
        "#},
        indoc::indoc! {r#"
            [claude.limits]
            allowed_permission_modes = ["bypassPermissions"]
        "#},
    ] {
        let home = tempfile::tempdir().or_fail()?;
        let repo = home.path().join("work/cloned");
        std::fs::create_dir_all(repo.join(".git")).or_fail()?;
        agentmux::testing::write_config_file(repo.join("agentmux.toml"), table).or_fail()?;
        let env: BTreeMap<String, String> = [(
            "HOME".to_owned(),
            home.path().to_string_lossy().into_owned(),
        )]
        .into_iter()
        .collect();

        let error = Config::load(&env, &repo).err().or_fail()?;

        assert_that!(
            error,
            matches_pattern!(ConfigError::ProjectFileOversteps { .. }),
            "{table}"
        );
    }
    Ok(())
}

/// Defaults the same file's limits refuse are refused when the file is read, because every
/// consultation relying on them would be.
#[gtest]
fn defaults_their_own_limits_refuse_are_refused_at_load() -> Result<()> {
    let cases = [
        (
            indoc::indoc! {r#"
                [codex.defaults]
                sandbox = "danger_full_access"
            "#},
            "codex.defaults",
        ),
        (
            indoc::indoc! {"
                [codex.defaults]
                network_access = true
                [codex.limits]
                allow_network = false
            "},
            "codex.defaults",
        ),
        (
            indoc::indoc! {"
                [codex.defaults]
                ignore_user_config = false
                [codex.limits]
                allow_user_config = false
            "},
            "codex.defaults.ignore_user_config",
        ),
        // Plan mode is agentmux's own default, and this list leaves it out.
        (
            indoc::indoc! {r#"
                [claude.limits]
                allowed_permission_modes = ["acceptEdits"]
            "#},
            "claude.defaults",
        ),
        (
            indoc::indoc! {r#"
                [accounts.codex.personal]
                config_dir = "/tmp"
                inherit_settings = true
                [codex.limits]
                allow_user_config = false
            "#},
            "accounts.codex.personal.inherit_settings",
        ),
    ];
    for (text, setting) in cases {
        let home = tempfile::tempdir().or_fail()?;
        agentmux::testing::write_config_file(home.path().join("agentmux.toml"), text).or_fail()?;
        let env: BTreeMap<String, String> = [(
            "HOME".to_owned(),
            home.path().to_string_lossy().into_owned(),
        )]
        .into_iter()
        .collect();

        let error = Config::load(&env, home.path()).err().or_fail()?;

        assert_that!(
            error,
            matches_pattern!(ConfigError::DefaultBeyondLimit {
                setting: contains_substring(setting),
                ..
            }),
            "{text}"
        );
    }
    // The limit that rules inheriting out makes isolation what nobody choosing means, which is
    // no contradiction.
    let config = machine(indoc::indoc! {"
        [codex.limits]
        allow_user_config = false
    "})?;
    assert_that!(
        codex(None, None)?.resolved_isolation(&config),
        eq(Isolation::Isolated)
    );
    Ok(())
}

/// `ignore_user_config` and its inverse `inherit_settings` name one choice, and a request that
/// spells it both ways must agree with itself.
#[gtest]
fn the_two_isolation_spellings_must_agree() -> Result<()> {
    assert_that!(
        Isolation::from_spellings(Some(true), None),
        ok(some(eq(&Isolation::Isolated)))
    );
    assert_that!(
        Isolation::from_spellings(None, Some(true)),
        ok(some(eq(&Isolation::Inherit)))
    );
    // Matching inverse values are one choice spelled twice.
    assert_that!(
        Isolation::from_spellings(Some(false), Some(true)),
        ok(some(eq(&Isolation::Inherit)))
    );
    assert_that!(Isolation::from_spellings(None, None), ok(none()));
    let conflict = Isolation::from_spellings(Some(true), Some(true))
        .err()
        .or_fail()?;
    assert_that!(
        conflict.to_string(),
        eq(
            "Conflicting settings: ignore_user_config=true and inherit_settings=true. Supply only \
            one spelling, or matching inverse values."
        )
    );
    Ok(())
}

/// The caller decides first, then the account, then the vendor's default, and loading the user's
/// configuration is what is left.
#[gtest]
fn isolation_is_decided_by_the_caller_then_the_account_then_the_defaults() -> Result<()> {
    let personal = AccountAlias::parse("personal").or_fail()?;
    let named = |delegate: Delegate| delegate.with_account(AccountChoice::Named(personal.clone()));
    let config = machine(indoc::indoc! {r#"
        [accounts.codex.personal]
        config_dir = "/tmp"
        inherit_settings = true

        [codex.defaults]
        ignore_user_config = true
    "#})?;

    // The vendor's default, for a delegate whose account says nothing.
    assert_that!(
        codex(None, None)?.resolved_isolation(&config),
        eq(Isolation::Isolated)
    );
    // The account's own preference wins over the vendor's default.
    assert_that!(
        named(codex(None, None)?).resolved_isolation(&config),
        eq(Isolation::Inherit)
    );
    // The caller wins over both.
    assert_that!(
        named(codex(None, None)?)
            .with_isolation(Isolation::Isolated)
            .resolved_isolation(&config),
        eq(Isolation::Isolated)
    );
    // With nothing configured, the user's configuration loads.
    assert_that!(
        codex(None, None)?.resolved_isolation(&Config::default()),
        eq(Isolation::Inherit)
    );
    Ok(())
}

/// Each Codex sandbox and network choice reaches `codex exec` in the spelling that release takes,
/// on a new session and on a resume, which refuses `--sandbox`.
#[gtest]
fn codex_sandbox_and_network_reach_exec_on_start_and_resume() -> Result<()> {
    let config = Config::default();
    let session = SessionRef::parse("01a0784a-915b-7d92-a381-b53d765296c4").or_fail()?;

    for resume in [None, Some(&session)] {
        let label = if resume.is_some() { "resume" } else { "start" };
        let sandbox_flag = |args: &[String], mode: &str| {
            if resume.is_some() {
                has_pair(args, "--config", &format!("sandbox_mode=\"{mode}\""))
            } else {
                has_pair(args, "--sandbox", mode)
            }
        };

        // Read-only, network left to Codex: the legacy mode and nothing else.
        let read_only =
            build(&codex(Some(CodexSandbox::ReadOnly), None)?, &config, resume).or_fail()?;
        let args = read_only.args();
        assert_that!(sandbox_flag(args, "read-only"), eq(true), "{label}");
        assert_that!(
            args.iter().any(|arg| arg.contains("network")),
            eq(false),
            "{label}"
        );

        // Workspace-write with a network choice either way.
        for network in [false, true] {
            let invocation = build(
                &codex(Some(CodexSandbox::WorkspaceWrite), Some(network))?,
                &config,
                resume,
            )
            .or_fail()?;
            let args = invocation.args();
            assert_that!(sandbox_flag(args, "workspace-write"), eq(true), "{label}");
            assert_that!(
                has_pair(
                    args,
                    "--config",
                    &format!("sandbox_workspace_write.network_access={network}")
                ),
                eq(true),
                "{label} {network}"
            );
        }

        // Read-only with network: a profile of this launch's own, defined and selected, and no
        // legacy sandbox flag beside it, which would drop the profile.
        let networked = build(
            &codex(Some(CodexSandbox::ReadOnly), Some(true))?,
            &config,
            resume,
        )
        .or_fail()?;
        let args = networked.args();
        let profile = profile_of(args).or_fail()?;
        assert_that!(
            has_pair(
                args,
                "--config",
                &format!(
                    "permissions.{profile}={{extends=\":read-only\",network={{enabled=true}}}}"
                )
            ),
            eq(true),
            "{label}"
        );
        assert_that!(
            has_pair(
                args,
                "--config",
                &format!("default_permissions=\"{profile}\"")
            ),
            eq(true),
            "{label}"
        );
        assert_that!(has(args, "--sandbox"), eq(false), "{label}");
        assert_that!(
            args.iter().any(|arg| arg.starts_with("sandbox_mode=")),
            eq(false),
            "{label}"
        );

        // Read-only without network is the legacy read-only mode, which has none.
        let offline = build(
            &codex(Some(CodexSandbox::ReadOnly), Some(false))?,
            &config,
            resume,
        )
        .or_fail()?;
        assert_that!(
            sandbox_flag(offline.args(), "read-only"),
            eq(true),
            "{label}"
        );
        assert_that!(
            offline
                .args()
                .iter()
                .any(|arg| arg.contains("default_permissions")),
            eq(false),
            "{label}"
        );
    }
    Ok(())
}

/// The profile a read-only turn with network runs under is named afresh for every launch, so no
/// configuration file can define one of the same name in advance and have codex merge its own
/// entries, such as a filesystem write grant, into it.
#[gtest]
fn every_launch_names_its_own_permission_profile() -> Result<()> {
    let delegate = codex(Some(CodexSandbox::ReadOnly), Some(true))?;
    let session = SessionRef::parse("01a0784a-915b-7d92-a381-b53d765296c4").or_fail()?;
    let names = [
        profile_of(build(&delegate, &Config::default(), None).or_fail()?.args()).or_fail()?,
        profile_of(build(&delegate, &Config::default(), None).or_fail()?.args()).or_fail()?,
        profile_of(
            build(&delegate, &Config::default(), Some(&session))
                .or_fail()?
                .args(),
        )
        .or_fail()?,
    ];
    let [first, second, resumed] = &names;
    assert_that!(first, starts_with("agentmux_ro_"));
    // A name a user's `[permissions.<name>.filesystem]` could have been written for is never
    // reused: not by another start, and not by a resume of the same session.
    assert_that!(first, not(eq(second)));
    assert_that!(first, not(eq(resumed)));
    assert_that!(second, not(eq(resumed)));
    Ok(())
}

/// Every `codex exec` turn pins the approval policy and the reviewer, new or resumed, isolated or
/// not, because codex drops its own headless `never` when the loaded configuration names the
/// `auto_review` reviewer, and an approval would let a command out of the sandbox.
#[gtest]
fn every_exec_turn_pins_unattended_approval() -> Result<()> {
    let session = SessionRef::parse("01a0784a-915b-7d92-a381-b53d765296c4").or_fail()?;
    for resume in [None, Some(&session)] {
        for isolation in [Isolation::Inherit, Isolation::Isolated] {
            for network in [None, Some(true)] {
                let delegate =
                    codex(Some(CodexSandbox::ReadOnly), network)?.with_isolation(isolation);
                let invocation = build(&delegate, &Config::default(), resume).or_fail()?;
                let args = invocation.args();
                let label = format!("{resume:?} {isolation:?} {network:?}");
                assert_that!(
                    has_pair(args, "--config", "approval_policy=\"never\""),
                    eq(true),
                    "{label}"
                );
                assert_that!(
                    has_pair(args, "--config", "approvals_reviewer=\"user\""),
                    eq(true),
                    "{label}"
                );
            }
        }
    }
    Ok(())
}

/// Loading the user's configuration and skipping it differ in exactly the isolation flags, and
/// the sandbox is passed either way, so the user's own `sandbox_mode` never decides it.
#[gtest]
fn a_codex_delegate_that_loads_the_user_config_still_gets_its_sandbox() -> Result<()> {
    let config = Config::default();
    let inheriting = build(&codex(None, None)?, &config, None).or_fail()?;
    let isolated = build(
        &codex(None, None)?.with_isolation(Isolation::Isolated),
        &config,
        None,
    )
    .or_fail()?;

    assert_that!(has(inheriting.args(), "--ignore-user-config"), eq(false));
    assert_that!(has(inheriting.args(), "features.hooks=false"), eq(false));
    assert_that!(
        has_pair(inheriting.args(), "--sandbox", "read-only"),
        eq(true)
    );
    assert_that!(has(isolated.args(), "--ignore-user-config"), eq(true));
    assert_that!(has(isolated.args(), "features.hooks=false"), eq(true));
    assert_that!(
        has_pair(isolated.args(), "--sandbox", "read-only"),
        eq(true)
    );
    Ok(())
}

/// A limit that forbids network switches off a network nobody chose, rather than leaving it to
/// the user's own configuration.
#[gtest]
fn a_network_ban_switches_off_a_network_nobody_chose() -> Result<()> {
    let config = machine(indoc::indoc! {"
        [codex.limits]
        allow_network = false
    "})?;
    let invocation = build(
        &codex(Some(CodexSandbox::WorkspaceWrite), None)?,
        &config,
        None,
    )
    .or_fail()?;
    assert_that!(
        has_pair(
            invocation.args(),
            "--config",
            "sandbox_workspace_write.network_access=false"
        ),
        eq(true)
    );
    Ok(())
}

/// A request beyond a limit is refused in the words the caller retries from, naming the limit and
/// the file, and is never lowered to fit.
#[gtest]
fn a_request_beyond_a_limit_is_refused_naming_the_limit_and_the_file() -> Result<()> {
    let config = machine(indoc::indoc! {r#"
        [codex.limits]
        max_sandbox = "workspace_write"
        allow_network = false
        allow_user_config = false

        [codex.defaults]
        ignore_user_config = true
    "#})?;
    let refusal = |delegate: Delegate| -> Result<String> {
        Ok(build(&delegate, &config, None).err().or_fail()?.to_string())
    };

    assert_that!(
        refusal(codex(Some(CodexSandbox::DangerFullAccess), None)?)?,
        eq(&format!(
            "Cannot launch codex: sandbox=\"danger_full_access\" exceeds \
             codex.limits.max_sandbox=\"workspace_write\" in {MACHINE_FILE}. Request \
             sandbox=\"workspace_write\" or \"read_only\". Only the owner can raise this limit."
        ))
    );
    assert_that!(
        refusal(codex(None, Some(true))?)?,
        eq(&format!(
            "Cannot launch codex: network_access=true is forbidden by \
             codex.limits.allow_network=false in {MACHINE_FILE}. Request network_access=false. \
             Only the owner can change this limit."
        ))
    );
    assert_that!(
        refusal(codex(None, None)?.with_isolation(Isolation::Inherit))?,
        eq(&format!(
            "Cannot launch codex: ignore_user_config=false is forbidden by \
             codex.limits.allow_user_config=false in {MACHINE_FILE}. Request \
             ignore_user_config=true."
        ))
    );
    Ok(())
}

/// Full access and bypassing Claude's permissions are the owner's to allow: refused by default,
/// launched as asked once the machine file allows them.
#[gtest]
fn the_dangerous_modes_need_the_owner() -> Result<()> {
    let danger = codex(Some(CodexSandbox::DangerFullAccess), None)?;
    let bypass = claude(ClaudePermissions {
        permission_mode: Some(ClaudePermissionMode::BypassPermissions),
        ..ClaudePermissions::default()
    })?;

    // Refused by agentmux's own limits, saying where another can be set.
    let refused = build(&danger, &Config::default(), None).err().or_fail()?;
    assert_that!(
        refusal_of(&refused),
        some(matches_pattern!(
            PermissionRefusal::SandboxBeyondLimit { .. }
        ))
    );
    assert_that!(
        refused.to_string(),
        contains_substring("agentmux's built-in limit")
    );
    let refused = build(&bypass, &Config::default(), None).err().or_fail()?;
    assert_that!(
        refused.to_string(),
        contains_substring(
            "Cannot launch claude: permission_mode=\"bypassPermissions\" is not in \
             claude.limits.allowed_permission_modes"
        )
    );

    // Allowed by the owner, launched exactly as asked.
    let allowed = machine(indoc::indoc! {r#"
        [codex.limits]
        max_sandbox = "danger_full_access"

        [claude.limits]
        allowed_permission_modes = ["plan", "bypassPermissions"]
    "#})?;
    let launched = build(&danger, &allowed, None).or_fail()?;
    assert_that!(
        has_pair(launched.args(), "--sandbox", "danger-full-access"),
        eq(true)
    );
    let launched = build(&bypass, &allowed, None).or_fail()?;
    assert_that!(
        has_pair(launched.args(), "--permission-mode", "bypassPermissions"),
        eq(true)
    );
    // Full access has no network switch, so a request to switch it off is refused.
    let offline_danger = build(
        &codex(Some(CodexSandbox::DangerFullAccess), Some(false))?,
        &allowed,
        None,
    )
    .err()
    .or_fail()?;
    assert_that!(
        refusal_of(&offline_danger),
        some(eq(&PermissionRefusal::FullAccessCannotDenyNetwork))
    );
    Ok(())
}

/// A Claude delegate's permission mode and tool lists reach the CLI as chosen, and only the lists
/// that were chosen are passed, so a delegate that loads the user's settings keeps the user's
/// own rules otherwise.
#[gtest]
fn claude_permissions_reach_the_cli() -> Result<()> {
    let config = Config::default();
    let chosen = claude(ClaudePermissions {
        permission_mode: Some(ClaudePermissionMode::DontAsk),
        tools: Some(tools(&["Read", "Grep", "WebFetch"])?),
        allowed_tools: Some(tools(&["WebFetch", "Bash(cargo check *)"])?),
        disallowed_tools: Some(tools(&["Bash(git push *)"])?),
    })?;
    let invocation = build(&chosen, &config, None).or_fail()?;
    let args = invocation.args();
    assert_that!(has_pair(args, "--permission-mode", "dontAsk"), eq(true));
    assert_that!(has_pair(args, "--tools", "Read,Grep,WebFetch"), eq(true));
    assert_that!(
        has_pair(args, "--allowedTools", "WebFetch,Bash(cargo check *)"),
        eq(true)
    );
    assert_that!(
        has_pair(args, "--disallowedTools", "Bash(git push *)"),
        eq(true)
    );

    // Nothing chosen: plan mode and the read-only list, and no rule lists at all.
    let defaulted = build(&claude(ClaudePermissions::default())?, &config, None).or_fail()?;
    let args = defaulted.args();
    assert_that!(has_pair(args, "--permission-mode", "plan"), eq(true));
    assert_that!(
        has_pair(args, "--tools", "Bash,Read,Grep,Glob,WebFetch,WebSearch"),
        eq(true)
    );
    assert_that!(has(args, "--allowedTools"), eq(false));
    assert_that!(has(args, "--disallowedTools"), eq(false));

    // An empty tool list offers none.
    let none_offered = claude(ClaudePermissions {
        tools: Some(Vec::new()),
        ..ClaudePermissions::default()
    })?;
    let invocation = build(&none_offered, &config, None).or_fail()?;
    assert_that!(has_pair(invocation.args(), "--tools", ""), eq(true));

    // A rule cannot smuggle a second entry past the comma that separates them.
    assert_that!(ClaudeTool::parse("Read,Write"), err(anything()));
    Ok(())
}

/// An option of the other vendor is refused rather than ignored, at the start and on a follow-up.
#[gtest]
fn an_option_for_the_other_vendor_is_refused() -> Result<()> {
    let network = PermissionOptions {
        network_access: Some(true),
        ..PermissionOptions::default()
    };
    let mode = PermissionOptions {
        permission_mode: Some(ClaudePermissionMode::Plan),
        ..PermissionOptions::default()
    };

    assert_that!(
        Delegate::from_parts(Vendor::Claude, "opus", None, None, network.clone()),
        err(matches_pattern!(DelegateError::NotForVendor {
            option: eq(&"network_access"),
            ..
        }))
    );
    assert_that!(
        Delegate::from_parts(Vendor::Codex, "sol", None, None, mode.clone()),
        err(matches_pattern!(DelegateError::NotForVendor {
            option: eq(&"permission_mode"),
            ..
        }))
    );
    assert_that!(
        codex(None, None)?.with_options(mode),
        err(matches_pattern!(DelegateError::NotForVendor { .. }))
    );
    assert_that!(
        claude(ClaudePermissions::default())?.with_options(network),
        err(matches_pattern!(DelegateError::NotForVendor { .. }))
    );
    Ok(())
}

/// A driven Codex turn is given the same network as `exec`: a read-only one's as the turn's
/// sandbox policy, which the driver sends, and a workspace-write one's as a server override.
#[gtest]
fn the_app_server_gets_the_same_network_as_exec() -> Result<()> {
    let config = Config::default();
    let read_only = codex(Some(CodexSandbox::ReadOnly), Some(true))?
        .app_server_settings(&config)
        .or_fail()?;
    assert_that!(read_only.sandbox, eq(CodexSandbox::ReadOnly));
    assert_that!(read_only.network_access, some(eq(true)));
    assert_that!(
        read_only
            .args
            .iter()
            .any(|arg| arg.contains("default_permissions")),
        eq(false)
    );

    let workspace = codex(Some(CodexSandbox::WorkspaceWrite), Some(false))?
        .app_server_settings(&config)
        .or_fail()?;
    assert_that!(
        has_pair(
            &workspace.args,
            "--config",
            "sandbox_workspace_write.network_access=false"
        ),
        eq(true)
    );
    Ok(())
}

/// Read-only with network over `exec` needs a codex whose profile support is verified.
#[gtest]
fn permission_profiles_need_a_verified_codex() {
    assert_that!(
        codex_supports_permission_profiles(Some("codex-cli 0.163.0")),
        eq(true)
    );
    assert_that!(
        codex_supports_permission_profiles(Some("codex-cli 0.162.1")),
        eq(true)
    );
    assert_that!(
        codex_supports_permission_profiles(Some("codex-cli 0.162.0")),
        eq(false)
    );
    assert_that!(
        codex_supports_permission_profiles(Some("codex-cli 1.0.0-alpha.2")),
        eq(true)
    );
    assert_that!(
        codex_supports_permission_profiles(Some("codex (scripted)")),
        eq(false)
    );
    assert_that!(codex_supports_permission_profiles(None), eq(false));
}

/// A run store over a machine file the test can rewrite between turns.
struct Machine {
    store: RunStore,
    launcher: Arc<ScriptedLauncher>,
    config_path: PathBuf,
    home: tempfile::TempDir,
    _state: tempfile::TempDir,
}

impl Machine {
    fn new(text: &str, launcher: ScriptedLauncher) -> Result<Self> {
        let home = tempfile::tempdir().or_fail()?;
        let state = tempfile::tempdir().or_fail()?;
        let config_path = home.path().join("agentmux.toml");
        agentmux::testing::write_config_file(&config_path, text).or_fail()?;
        let env: BTreeMap<String, String> = [
            ("PATH".to_owned(), "/usr/bin".to_owned()),
            (
                "HOME".to_owned(),
                home.path().to_string_lossy().into_owned(),
            ),
            (
                "AGENTMUX_CONFIG".to_owned(),
                config_path.to_string_lossy().into_owned(),
            ),
        ]
        .into_iter()
        .collect();
        let launcher = Arc::new(launcher);
        let store = RunStore::open(state.path(), launcher.clone(), env).or_fail()?;
        Ok(Self {
            store,
            launcher,
            config_path,
            home,
            _state: state,
        })
    }

    fn rewrite(&self, text: &str) -> Result<()> {
        agentmux::testing::write_config_file(&self.config_path, text).or_fail()?;
        Ok(())
    }

    fn request(&self, delegate: Delegate) -> StartRequest {
        StartRequest {
            delegate,
            question: "q".to_owned(),
            cwd: self.home.path().to_path_buf(),
            retention: Retention::Ttl,
            env: BTreeMap::new(),
            billing: None,
            force_codex_exec: false,
        }
    }

    /// The arguments of the `index`th launch.
    fn args(&self, index: usize) -> Result<Vec<String>> {
        Ok(self.launcher.launches().get(index).or_fail()?.args.clone())
    }
}

/// A second Codex turn, resuming the session `fixtures::CODEX_HAPPY` opened.
const CODEX_SECOND_TURN: &str = indoc::indoc! {r#"
    {"type":"thread.started","thread_id":"01a0784a-915b-7d92-a381-b53d765296c4"}
    {"type":"turn.started"}
    {"type":"item.completed","item":{"id":"i0","type":"agent_message","text":"AGAIN"}}
    {"type":"turn.completed","usage":{"input_tokens":19000,"output_tokens":8}}
"#};

fn codex_turns(count: usize) -> Vec<Script> {
    std::iter::once(Script::completed(fixtures::CODEX_HAPPY))
        .chain(std::iter::repeat_with(|| {
            Script::completed(CODEX_SECOND_TURN)
        }))
        .take(count)
        .collect()
}

/// A consultation records what the machine file's defaults gave it, says so in its summary, and
/// keeps it when the defaults change afterwards.
#[gtest]
fn a_consultation_pins_the_defaults_it_started_with() -> Result<()> {
    let machine = Machine::new(
        OWNER_FILE,
        ScriptedLauncher::new(codex_turns(2)).with_cli_version("codex-cli 0.163.0"),
    )?;

    let started = machine.store.start(&machine.request(codex(None, None)?))?;
    assert_that!(
        started.delegate.codex_permissions().copied(),
        some(eq(CodexPermissions {
            sandbox: Some(CodexSandbox::ReadOnly),
            network_access: Some(true),
        }))
    );
    assert_that!(
        started.delegate.summary(),
        contains_substring("sandbox=read-only+network")
    );
    assert_that!(started.delegate.isolation(), some(eq(Isolation::Inherit)));
    assert_that!(
        machine
            .args(0)?
            .iter()
            .any(|arg| arg.starts_with("default_permissions=")),
        eq(true)
    );

    // A default edited later does not move a consultation already running.
    machine.rewrite(indoc::indoc! {"
        [codex.defaults]
        network_access = false
    "})?;
    machine.store.follow_up(&started.run_id, "again")?;
    assert_that!(
        machine
            .args(1)?
            .iter()
            .any(|arg| arg.starts_with("default_permissions=")),
        eq(true)
    );
    Ok(())
}

/// A follow-up may change the recorded choices; what it changes is what the later turns keep.
#[gtest]
fn a_follow_up_override_becomes_the_recorded_choice() -> Result<()> {
    let machine = Machine::new("", ScriptedLauncher::new(codex_turns(3)))?;
    let started = machine.store.start(&machine.request(codex(None, None)?))?;

    machine.store.follow_up_with(
        &started.run_id,
        "write it out",
        PermissionOptions {
            sandbox: Some(CodexSandbox::WorkspaceWrite),
            network_access: Some(false),
            isolation: Some(Isolation::Isolated),
            ..PermissionOptions::default()
        },
    )?;
    machine.store.follow_up(&started.run_id, "and again")?;

    for index in [1, 2] {
        let args = machine.args(index)?;
        assert_that!(
            has_pair(&args, "--config", "sandbox_mode=\"workspace-write\""),
            eq(true),
            "{index}"
        );
        assert_that!(
            has_pair(
                &args,
                "--config",
                "sandbox_workspace_write.network_access=false"
            ),
            eq(true),
            "{index}"
        );
        assert_that!(has(&args, "--ignore-user-config"), eq(true), "{index}");
    }
    let status = machine.store.status(&started.run_id)?;
    assert_that!(
        status.delegate.summary(),
        contains_substring("sandbox=workspace-write+no-network")
    );
    Ok(())
}

/// A limit the owner tightens after a consultation began refuses its next turn, before the turn
/// is claimed, and a follow-up that asks for less is accepted.
#[gtest]
fn a_tightened_limit_refuses_the_next_turn() -> Result<()> {
    let machine = Machine::new(
        indoc::indoc! {"
            [codex.defaults]
            network_access = true
        "},
        ScriptedLauncher::new(codex_turns(2)).with_cli_version("codex-cli 0.163.0"),
    )?;
    let started = machine.store.start(&machine.request(codex(None, None)?))?;

    machine.rewrite(indoc::indoc! {"
        [codex.limits]
        allow_network = false
    "})?;
    let refused = machine
        .store
        .follow_up(&started.run_id, "again")
        .err()
        .or_fail()?;
    assert_that!(
        run_refusal_of(&refused),
        some(matches_pattern!(PermissionRefusal::NetworkForbidden { .. }))
    );
    assert_that!(
        refused.to_string(),
        contains_substring(machine.config_path.display().to_string())
    );
    assert_that!(machine.launcher.launches().len(), eq(1));
    assert_that!(machine.store.status(&started.run_id)?.turns, eq(1));

    // Asking for what the new limit allows continues the same consultation.
    machine.store.follow_up_with(
        &started.run_id,
        "again, offline",
        PermissionOptions {
            network_access: Some(false),
            ..PermissionOptions::default()
        },
    )?;
    assert_that!(
        has_pair(&machine.args(1)?, "--config", "sandbox_mode=\"read-only\""),
        eq(true)
    );
    Ok(())
}

/// A request beyond a limit launches nothing and leaves no consultation behind.
#[gtest]
fn a_refused_start_leaves_no_run() -> Result<()> {
    let machine = Machine::new("", ScriptedLauncher::new(codex_turns(1)))?;

    let refused = machine
        .store
        .start(&machine.request(codex(Some(CodexSandbox::DangerFullAccess), None)?));

    let refused = refused.err().or_fail()?;
    assert_that!(
        run_refusal_of(&refused),
        some(matches_pattern!(
            PermissionRefusal::SandboxBeyondLimit { .. }
        ))
    );
    assert_that!(machine.store.list(10)?, is_empty());
    assert_that!(machine.launcher.launches(), is_empty());
    Ok(())
}

/// Read-only with network over `exec` is refused on a codex whose profile support is not
/// verified, rather than run under whatever its configuration selects.
#[gtest]
fn an_unverified_codex_cannot_run_read_only_with_network_over_exec() -> Result<()> {
    let machine = Machine::new(
        "",
        ScriptedLauncher::new(codex_turns(1)).with_cli_version("codex-cli 0.150.0"),
    )?;

    let refused = machine
        .store
        .start(&machine.request(codex(Some(CodexSandbox::ReadOnly), Some(true))?))
        .err()
        .or_fail()?;

    assert_that!(
        refused.to_string(),
        contains_substring(
            "Cannot launch codex exec with sandbox=\"read_only\" and network_access=true: this \
             Codex installation lacks verified permission-profile support."
        )
    );
    assert_that!(refused.to_string(), contains_substring("codex-cli 0.150.0"));
    assert_that!(machine.store.list(10)?, is_empty());
    assert_that!(machine.launcher.launches(), is_empty());
    Ok(())
}

/// A consultation recorded before isolation and permissions were kept follows up as it ran:
/// isolated, in plan mode, with the read-only tool list.
#[gtest]
fn a_record_without_the_new_fields_follows_up_as_it_ran() -> Result<()> {
    let machine = Machine::new(
        "",
        ScriptedLauncher::new([
            Script::completed(fixtures::CLAUDE_HAPPY),
            Script::completed(fixtures::CLAUDE_RESUME),
        ]),
    )?;
    let started = machine
        .store
        .start(&machine.request(claude(ClaudePermissions::default())?))?;

    // Strip what an older agentmux never wrote.
    let meta_path = machine
        .store
        .root()
        .join("runs")
        .join(started.run_id.as_str())
        .join("meta.json");
    let mut meta: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&meta_path).or_fail()?).or_fail()?;
    let delegate = meta
        .get_mut("delegate")
        .and_then(serde_json::Value::as_object_mut)
        .or_fail()?;
    delegate.remove("isolation");
    delegate.remove("permission_mode");
    std::fs::write(&meta_path, serde_json::to_string(&meta).or_fail()?).or_fail()?;
    let invocation_path = meta_path
        .with_file_name("turns")
        .join("0000")
        .join("invocation.json");
    let mut invocation: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&invocation_path).or_fail()?).or_fail()?;
    invocation
        .as_object_mut()
        .or_fail()?
        .remove("delegate")
        .or_fail()?;
    std::fs::write(
        &invocation_path,
        serde_json::to_string(&invocation).or_fail()?,
    )
    .or_fail()?;

    machine.store.follow_up(&started.run_id, "again")?;

    let args = machine.args(1)?;
    assert_that!(has_pair(&args, "--setting-sources", ""), eq(true));
    assert_that!(has_pair(&args, "--permission-mode", "plan"), eq(true));
    assert_that!(
        has_pair(&args, "--tools", "Bash,Read,Grep,Glob,WebFetch,WebSearch"),
        eq(true)
    );
    Ok(())
}

/// A follow-up that changes the permissions records them with its turn and never rewrites what is
/// already rendered: the transcript before the follow-up is a prefix of the one after it, so every
/// cursor handed out stays good, and the new turn says what it ran with.
#[gtest]
fn a_permission_change_leaves_the_rendered_transcript_in_place() -> Result<()> {
    let machine = Machine::new("", ScriptedLauncher::new(codex_turns(2)))?;
    let started = machine
        .store
        .start(&machine.request(codex(Some(CodexSandbox::WorkspaceWrite), None)?))?;
    let meta_path = machine
        .store
        .root()
        .join("runs")
        .join(started.run_id.as_str())
        .join("meta.json");
    let meta_before = std::fs::read_to_string(&meta_path).or_fail()?;
    let before = machine.store.page(&started.run_id, 0, usize::MAX)?;

    machine.store.follow_up_with(
        &started.run_id,
        "again, read-only",
        PermissionOptions {
            sandbox: Some(CodexSandbox::ReadOnly),
            ..PermissionOptions::default()
        },
    )?;

    let after = machine.store.page(&started.run_id, 0, usize::MAX)?;
    assert_that!(after.text, starts_with(before.text.as_str()));
    assert_that!(
        std::fs::read_to_string(&meta_path).or_fail()?,
        eq(&meta_before)
    );
    let (_, new_turn) = after.text.split_at(before.text.len());
    assert_that!(
        new_turn,
        contains_substring("ran with other settings than the header names")
    );
    assert_that!(new_turn, contains_substring("sandbox=read-only"));
    // The status, and so the next follow-up, starts from what the newest turn ran with.
    assert_that!(
        machine.store.status(&started.run_id)?.delegate.summary(),
        contains_substring("sandbox=read-only")
    );
    Ok(())
}

/// A hook that reopened a turn which loaded the user's settings stays expected after a later turn
/// runs isolated: each reopening is judged by its own turn.
#[gtest]
fn a_reopened_turn_is_judged_by_its_own_isolation() -> Result<()> {
    let machine = Machine::new(
        "",
        ScriptedLauncher::new([
            Script::completed(fixtures::CLAUDE_STOP_HOOK),
            Script::completed(fixtures::CLAUDE_RESUME),
        ]),
    )?;
    let started = machine
        .store
        .start(&machine.request(claude(ClaudePermissions::default())?))?;
    assert_that!(
        started.hook_reopening,
        some(eq(agentmux::run::HookReopening::Expected))
    );

    machine.store.follow_up_with(
        &started.run_id,
        "again, isolated",
        PermissionOptions {
            isolation: Some(Isolation::Isolated),
            ..PermissionOptions::default()
        },
    )?;

    let status = machine.store.status(&started.run_id)?;
    assert_that!(
        status.hook_reopening,
        some(eq(agentmux::run::HookReopening::Expected))
    );
    assert_that!(status.delegate.isolation(), some(eq(Isolation::Isolated)));
    assert_that!(
        has_pair(&machine.args(1)?, "--setting-sources", ""),
        eq(true)
    );
    Ok(())
}

/// `list` describes a consultation as `status` does, by what its newest turn ran with, so the two
/// agree after a follow-up changed the permissions.
#[gtest]
fn list_and_status_agree_after_an_override() -> Result<()> {
    let machine = Machine::new("", ScriptedLauncher::new(codex_turns(2)))?;
    let started = machine
        .store
        .start(&machine.request(codex(Some(CodexSandbox::WorkspaceWrite), None)?))?;
    machine.store.follow_up_with(
        &started.run_id,
        "again, offline and read-only",
        PermissionOptions {
            sandbox: Some(CodexSandbox::ReadOnly),
            network_access: Some(false),
            ..PermissionOptions::default()
        },
    )?;

    let status = machine.store.status(&started.run_id)?;
    let listed = machine.store.list(10)?;
    let summary = status.delegate.summary();
    assert_that!(summary, contains_substring("sandbox=read-only+no-network"));
    assert_that!(
        listed.first().map(|entry| entry.delegate.clone()),
        some(starts_with(summary.as_str()))
    );
    Ok(())
}

/// A turn that cannot be steered says why by its own isolation, which a follow-up may have
/// changed from the one the consultation started with.
#[gtest]
#[tokio::test]
async fn a_steer_refusal_names_the_running_turns_isolation() -> Result<()> {
    let machine = Machine::new(
        "",
        ScriptedLauncher::new([
            Script::completed(fixtures::CODEX_HAPPY),
            Script::running(String::new()),
        ]),
    )?;
    // Loads the user's configuration, but this store drives no app-server, so it runs over
    // `exec`.
    let started = machine.store.start(&machine.request(codex(None, None)?))?;
    machine.store.follow_up_with(
        &started.run_id,
        "again, isolated",
        PermissionOptions {
            isolation: Some(Isolation::Isolated),
            ..PermissionOptions::default()
        },
    )?;

    let refused = machine.store.steer(&started.run_id, "x").await;

    assert_that!(
        refused.map_err(|err| err.to_string()),
        err(contains_substring("it runs isolated"))
    );
    Ok(())
}
