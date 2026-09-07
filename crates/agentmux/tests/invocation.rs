//! What agentmux actually hands to a delegate CLI.
//!
//! The value of this crate is that its callers have no flags to forget, because the correct flags
//! are not expressible as anything else.
//! These tests are that claim, mechanised.

use std::collections::BTreeMap;
use std::path::Path;

use agentmux::config::{Account, Config, Secret};
use agentmux::delegate::{
    AccountAlias, CodexSandbox, Delegate, Effort, Invocation, Isolation, ModelId, SessionRef,
    TurnPlan,
};
use googletest::prelude::*;

fn model(id: &str) -> Result<ModelId> {
    ModelId::parse(id).or_fail()
}

fn effort(level: &str) -> Result<Effort> {
    Effort::parse(level).or_fail()
}

/// Every variant of the closed set, so a third vendor cannot be added without extending this.
fn every_delegate() -> Result<Vec<Delegate>> {
    Ok(vec![
        Delegate::Claude {
            model: model("claude-opus-5")?,
            effort: effort("xhigh")?,
            account: None,
            isolation: None,
        },
        Delegate::Claude {
            model: model("claude-fable-5-1")?,
            effort: effort("xhigh")?,
            account: Some(AccountAlias::parse("personal").or_fail()?),
            isolation: None,
        },
        Delegate::Codex {
            model: model("gpt-6-astra")?,
            effort: effort("high")?,
            sandbox: CodexSandbox::ReadOnly,
            account: None,
            isolation: None,
        },
        Delegate::Codex {
            model: model("gpt-5.6-sol")?,
            effort: effort("xhigh")?,
            sandbox: CodexSandbox::WorkspaceWrite,
            account: None,
            isolation: None,
        },
    ])
}

/// An environment as hostile as the real one: a coding-agent session's own identity.
fn host_session_env() -> BTreeMap<String, String> {
    [
        ("PATH", "/usr/bin:/bin"),
        ("HOME", "/home/dev"),
        ("USER", "dev"),
        ("LANG", "en_US.UTF-8"),
        ("ANTHROPIC_API_KEY", "sk-ant-secret"),
        ("OPENAI_API_KEY", "sk-openai-secret"),
        // Everything below is the launching agent's own session, and none of it may be inherited.
        ("CLAUDECODE", "1"),
        (
            "CLAUDE_CODE_SESSION_ID",
            "85ff2d6a-ec24-47cf-a4f8-486a5bb06814",
        ),
        ("CLAUDE_CODE_ENTRYPOINT", "cli"),
        ("CLAUDE_CONFIG_DIR", "/home/dev/.claude-personal"),
        ("CLAUDE_EFFORT", "max"),
        ("CLAUDE_CODE_MESSAGING_SOCKET", "/tmp/socket"),
        ("CLAUDE_CODE_MESSAGING_TOKEN", "token"),
        ("CLAUDE_PID", "27923"),
        ("AGENT_COMMENT_NUDGE_DISABLED", "1"),
        ("TERM", "xterm-256color"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect()
}

fn build(delegate: &Delegate, resume: Option<&SessionRef>) -> Result<Invocation> {
    build_with(delegate, resume, &Config::default())
}

/// Build against a specific machine configuration.
///
/// Separate from [`build`] because most tests care only about the argument vector, which no
/// configuration affects; the account tests are the ones that need a populated alias map.
fn build_with(
    delegate: &Delegate,
    resume: Option<&SessionRef>,
    config: &Config,
) -> Result<Invocation> {
    let plan = TurnPlan {
        question_path: Path::new("/runs/r/turns/0000/question.md"),
        last_message_path: Path::new("/runs/r/turns/0000/last-message.md"),
        resume,
        extra_env: &BTreeMap::new(),
    };
    delegate
        .invocation(&plan, &host_session_env(), config)
        .or_fail()
}

/// A configuration whose `personal` alias points at a directory that exists.
///
/// The directory has to be real: agentmux refuses an account whose config directory is missing,
/// which is the whole point of that check.
fn config_with_personal(dir: &Path) -> Config {
    let mut config = Config::default();
    let account = Account {
        config_dir: Some(dir.to_path_buf()),
        ..Account::default()
    };
    for vendor in ["claude", "codex"] {
        config
            .accounts
            .entry(vendor.to_owned())
            .or_default()
            .insert("personal".to_owned(), account.clone());
    }
    config
}

/// Whether `args` contains `flag` immediately followed by `value`.
///
/// Shares its shape with [`agentmux::testing::RecordedLaunch::has_flag_with`], which asks the same
/// question of a launch that happened rather than of an argument vector that was built.
fn has_pair(args: &[String], flag: &str, value: &str) -> bool {
    args.windows(2)
        .any(|w| w.first().is_some_and(|f| f == flag) && w.get(1).is_some_and(|v| v == value))
}

/// The "never forgets" claim: no delegate can be launched without full capture.
#[gtest]
fn every_delegate_captures_its_whole_stream() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let config = config_with_personal(dir.path());
    for delegate in every_delegate()? {
        let invocation = build_with(&delegate, None, &config)?;
        let args = &invocation.args;
        let label = delegate.summary();

        match delegate.vendor() {
            agentmux::delegate::Vendor::Claude => {
                // Streaming, or only the last assistant message comes back.
                assert_that!(
                    has_pair(args, "--output-format", "stream-json"),
                    eq(true),
                    "{label}"
                );
                // Mandatory alongside stream-json under --print.
                assert_that!(args.contains(&"--verbose".to_owned()), eq(true), "{label}");
                // The structural hook defence: no settings source can register a Stop hook.
                assert_that!(has_pair(args, "--setting-sources", ""), eq(true), "{label}");
                // No MCP re-entry.
                assert_that!(
                    args.contains(&"--strict-mcp-config".to_owned()),
                    eq(true),
                    "{label}"
                );
                assert_that!(
                    has_pair(args, "--mcp-config", r#"{"mcpServers":{}}"#),
                    eq(true),
                    "{label}"
                );
                // Read-only: no edit tool is offered at all.
                assert_that!(
                    has_pair(args, "--permission-mode", "plan"),
                    eq(true),
                    "{label}"
                );
                let tools = args
                    .windows(2)
                    .find(|w| w.first().is_some_and(|f| f == "--tools"))
                    .and_then(|w| w.get(1).cloned())
                    .unwrap_or_default();
                assert_that!(tools, not(contains_substring("Edit")), "{label}");
                assert_that!(tools, not(contains_substring("Write")), "{label}");
                // Would defeat --resume, and a consultation must stay open to a follow-up.
                assert_that!(
                    args.contains(&"--no-session-persistence".to_owned()),
                    eq(false),
                    "{label}"
                );
            }
            agentmux::delegate::Vendor::Codex => {
                // Without --json stdout carries only the final answer and stays empty until exit.
                assert_that!(args.contains(&"--json".to_owned()), eq(true), "{label}");
                assert_that!(
                    has_pair(args, "--config", "features.hooks=false"),
                    eq(true),
                    "{label}"
                );
                // Measured: `-c mcp_servers={}` does not detach the machine's MCP servers, and
                // a delegate launched with it still called a configured server's tool.
                // Only dropping the user config does, and without that a codex delegate could
                // reach agentmux itself and recurse.
                assert_that!(
                    args.contains(&"--ignore-user-config".to_owned()),
                    eq(true),
                    "{label} must not inherit the machine's MCP servers"
                );
                // Does not persist the session, so it would make a follow-up impossible.
                assert_that!(
                    args.contains(&"--ephemeral".to_owned()),
                    eq(false),
                    "{label}"
                );
                assert_that!(args.contains(&"-".to_owned()), eq(true), "{label}");
            }
        }

        // Model and effort are always pinned, never inherited from the host's defaults.
        // Codex carries the effort inside a `-c model_reasoning_effort="…"` override rather than
        // as its own argument, so this checks presence rather than an exact argument.
        assert_that!(
            args.contains(&delegate.model().as_str().to_owned()),
            eq(true),
            "{label}"
        );
        assert_that!(
            args.iter()
                .any(|arg| arg.contains(delegate.effort().as_str())),
            eq(true),
            "{label} did not pin its reasoning effort"
        );
    }
    Ok(())
}

/// The delegate must not be a nested session wearing its parent's identity.
#[gtest]
fn the_child_does_not_inherit_the_host_session() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let config = config_with_personal(dir.path());
    for delegate in every_delegate()? {
        let invocation = build_with(&delegate, None, &config)?;
        let label = delegate.summary();

        for key in invocation.env.keys() {
            assert_that!(
                key.starts_with("CLAUDE_CODE_"),
                eq(false),
                "{label} leaked host session variable {key}"
            );
            assert_that!(key.as_str(), not(eq("CLAUDECODE")), "{label}");
            assert_that!(key.as_str(), not(eq("CLAUDE_PID")), "{label}");
            assert_that!(key.as_str(), not(eq("CLAUDE_EFFORT")), "{label}");
            assert_that!(
                key.as_str(),
                not(eq("AGENT_COMMENT_NUDGE_DISABLED")),
                "{label}"
            );
        }

        // The allowlist is closed: every key is one agentmux chose, not one it failed to remove.
        let expected: &[&str] = &[
            "PATH",
            "HOME",
            "USER",
            "LANG",
            "TERM",
            "NO_COLOR",
            "CI",
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_BASE_URL",
            "OPENAI_API_KEY",
            "OPENAI_BASE_URL",
            "CODEX_HOME",
            "CLAUDE_CONFIG_DIR",
            // The recursion guard, which is agentmux's own and carries nothing of the host.
            "AGENTMUX_DELEGATE",
        ];
        for key in invocation.env.keys() {
            assert_that!(
                expected.contains(&key.as_str()),
                eq(true),
                "{label} set unexpected {key}"
            );
        }

        // The host's terminal type would make a CLI emit cursor sequences into the capture file.
        assert_that!(
            invocation.env.get("TERM").map(String::as_str),
            some(eq("dumb")),
            "{label}"
        );
    }
    Ok(())
}

/// A named account is the default account minus the host's API keys, plus its own config
/// directory.
///
/// The withholding is the half that is easy to miss: an `ANTHROPIC_API_KEY` exported in the
/// launching agent's shell would otherwise outrank the config directory, so a caller that asked
/// for one identity would silently spend another.
#[gtest]
fn a_named_account_authenticates_as_itself() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let config = config_with_personal(dir.path());

    let personal = build_with(
        &Delegate::Claude {
            model: model("claude-opus-5")?,
            effort: effort("xhigh")?,
            account: Some(AccountAlias::parse("personal").or_fail()?),
            isolation: None,
        },
        None,
        &config,
    )?;
    assert_that!(personal.env.get("ANTHROPIC_API_KEY"), none());
    assert_that!(personal.env.get("ANTHROPIC_AUTH_TOKEN"), none());
    assert_that!(
        personal.env.get("CLAUDE_CONFIG_DIR").map(String::as_str),
        some(eq(dir.path().to_string_lossy().as_ref()))
    );

    let default = build_with(
        &Delegate::Claude {
            model: model("claude-opus-5")?,
            effort: effort("xhigh")?,
            account: None,
            isolation: None,
        },
        None,
        &config,
    )?;
    assert_that!(
        default.env.get("ANTHROPIC_API_KEY").map(String::as_str),
        some(eq("sk-ant-secret"))
    );
    // The host's own CLAUDE_CONFIG_DIR must not select the account.
    assert_that!(default.env.get("CLAUDE_CONFIG_DIR"), none());
    Ok(())
}

/// An alias the machine does not define must name the ones it does.
///
/// The caller is usually a model that guessed a plausible name from the user's prose, and it
/// cannot see the file.
/// An error that only says "unknown" leaves it guessing again.
#[gtest]
fn an_unknown_alias_lists_the_configured_ones() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let config = config_with_personal(dir.path());
    let plan = TurnPlan {
        question_path: Path::new("/runs/r/turns/0000/question.md"),
        last_message_path: Path::new("/runs/r/turns/0000/last-message.md"),
        resume: None,
        extra_env: &BTreeMap::new(),
    };

    let error = Delegate::Claude {
        model: model("claude-opus-5")?,
        effort: effort("xhigh")?,
        account: Some(AccountAlias::parse("work").or_fail()?),
        isolation: None,
    }
    .invocation(&plan, &host_session_env(), &config)
    .expect_err("`work` is not configured");

    let message = error.to_string();
    assert_that!(
        message,
        contains_substring("no claude account named `work`")
    );
    assert_that!(message, contains_substring("personal"));
    Ok(())
}

/// An account pointing at a directory that is not there must say so before launching.
///
/// Left to the CLI this is silent: `claude` creates the directory, reports "Not logged in", and
/// sends the reader to look at their credentials rather than at the path that was wrong.
#[gtest]
fn an_account_directory_that_is_missing_is_refused_before_launch() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let missing = dir.path().join("never-created");
    let config = config_with_personal(&missing);
    let plan = TurnPlan {
        question_path: Path::new("/runs/r/turns/0000/question.md"),
        last_message_path: Path::new("/runs/r/turns/0000/last-message.md"),
        resume: None,
        extra_env: &BTreeMap::new(),
    };

    let error = Delegate::Claude {
        model: model("claude-opus-5")?,
        effort: effort("xhigh")?,
        account: Some(AccountAlias::parse("personal").or_fail()?),
        isolation: None,
    }
    .invocation(&plan, &host_session_env(), &config)
    .expect_err("the directory does not exist");

    assert_that!(error.to_string(), contains_substring("does not exist"));
    assert_that!(
        error.to_string(),
        contains_substring(missing.to_string_lossy().as_ref())
    );
    Ok(())
}

/// An account may carry credentials directly, which is how a local endpoint is reached.
///
/// A locally served model has no config directory to log into; it is a base URL and a token the
/// server ignores.
/// The same shape covers a gateway and a CI key.
#[gtest]
fn an_account_can_supply_credentials_instead_of_a_directory() -> Result<()> {
    let mut config = Config::default();
    config
        .accounts
        .entry("codex".to_owned())
        .or_default()
        .insert(
            "local".to_owned(),
            Account {
                base_url: Some("http://localhost:11434/v1".to_owned()),
                api_key_env: Some("LOCAL_TOKEN".to_owned()),
                ..Account::default()
            },
        );

    let plan = TurnPlan {
        question_path: Path::new("/runs/r/turns/0000/question.md"),
        last_message_path: Path::new("/runs/r/turns/0000/last-message.md"),
        resume: None,
        extra_env: &BTreeMap::new(),
    };
    let mut host = host_session_env();
    host.insert("LOCAL_TOKEN".to_owned(), "not-a-real-key".to_owned());

    let invocation = Delegate::Codex {
        model: model("qwen3-coder")?,
        effort: effort("high")?,
        sandbox: CodexSandbox::ReadOnly,
        account: Some(AccountAlias::parse("local").or_fail()?),
        isolation: None,
    }
    .invocation(&plan, &host, &config)
    .or_fail()?;

    assert_that!(
        invocation.env.get("OPENAI_BASE_URL").map(String::as_str),
        some(eq("http://localhost:11434/v1"))
    );
    assert_that!(
        invocation.env.get("OPENAI_API_KEY").map(String::as_str),
        some(eq("not-a-real-key"))
    );
    // The host's own Codex credentials stay behind: the alias chose the identity.
    assert_that!(invocation.env.get("CODEX_HOME"), none());
    Ok(())
}

/// `codex exec resume` rejects `--sandbox` outright and exits 2.
#[gtest]
fn a_codex_resume_passes_its_sandbox_as_config_not_as_a_flag() -> Result<()> {
    let session = SessionRef::parse("01a0784a-915b-7d92-a381-b53d765296c4").or_fail()?;
    let delegate = Delegate::Codex {
        model: model("gpt-6-astra")?,
        effort: effort("high")?,
        sandbox: CodexSandbox::ReadOnly,
        account: None,
        isolation: None,
    };

    let fresh = build(&delegate, None)?;
    assert_that!(has_pair(&fresh.args, "--sandbox", "read-only"), eq(true));

    let resumed = build(&delegate, Some(&session))?;
    assert_that!(resumed.args.contains(&"--sandbox".to_owned()), eq(false));
    assert_that!(
        has_pair(&resumed.args, "--config", r#"sandbox_mode="read-only""#),
        eq(true)
    );
    assert_that!(resumed.args.contains(&"resume".to_owned()), eq(true));
    // Resume does not inherit the recorded model, so it must be re-pinned every time.
    assert_that!(has_pair(&resumed.args, "--model", "gpt-6-astra"), eq(true));
    Ok(())
}

/// A Claude follow-up resumes the delegate's own session rather than restating the brief.
#[gtest]
fn a_claude_resume_reopens_the_delegates_session() -> Result<()> {
    let session = SessionRef::parse("85ff2d6a-ec24-47cf-a4f8-486a5bb06814").or_fail()?;
    let resumed = build(
        &Delegate::Claude {
            model: model("claude-opus-5")?,
            effort: effort("xhigh")?,
            account: None,
            isolation: None,
        },
        Some(&session),
    )?;
    assert_that!(
        has_pair(&resumed.args, "--resume", session.as_str()),
        eq(true)
    );
    Ok(())
}

/// Model identifiers and efforts are forwarded, not vetted against a roster.
#[gtest]
fn an_unreleased_model_identifier_is_accepted_verbatim() {
    // Whatever ships next year must work without an agentmux release.
    for id in [
        "gpt-7-nebula",
        "claude-opus-6",
        "claude-opus-5[1m]",
        "openai/gpt-6-astra",
    ] {
        assert_that!(
            ModelId::parse(id).map(|m| m.as_str().to_owned()),
            ok(eq(id))
        );
    }
    for level in ["ultra", "none", "minimal", "some-future-level"] {
        assert_that!(
            Effort::parse(level).map(|e| e.as_str().to_owned()),
            ok(eq(level))
        );
    }
}

/// Argv safety is the only thing agentmux enforces about a model identifier.
#[gtest]
fn an_argv_unsafe_model_identifier_is_rejected() {
    for id in [
        "",
        "--dangerously-skip-permissions",
        "a b",
        "a\nb",
        "a\0b",
        "a;rm -rf /",
    ] {
        assert_that!(
            ModelId::parse(id),
            err(anything()),
            "{id:?} must be rejected"
        );
    }
    assert_that!(
        ModelId::parse(&"x".repeat(ModelId::MAX_LEN + 1)),
        err(anything())
    );
}

/// A per-request environment may set nothing the machine's configuration has not listed.
///
/// The caller is frequently another model acting on text it was handed, so this is the boundary
/// between "switch off a hook for this review" and "run a program of my choosing with the
/// operator's subscription credentials in its environment".
/// No list of names to refuse stays complete against that: `PATH` chooses which binary executes,
/// `NODE_OPTIONS` runs code inside the CLI before it reads a setting, `LD_PRELOAD` does the same
/// to the loader, and the next runtime will read one more.
/// So a request may only set what the operator listed, and by default that is nothing.
#[gtest]
fn a_request_may_not_set_anything_that_redirects_the_consultation() -> Result<()> {
    let refused = [
        // Chooses which binary runs.
        "PATH",
        // Windows reads names without regard to case, so this spells the same variable there.
        "path",
        // Relocates the default account's identity.
        "HOME",
        // Runs the caller's code inside the CLI before it reads a single setting.
        "NODE_OPTIONS",
        "LD_PRELOAD",
        "DYLD_INSERT_LIBRARIES",
        "BASH_ENV",
        // Routes every request through a host of the caller's choosing, and makes its certificate
        // trusted; together these capture credentials without naming one.
        "HTTPS_PROXY",
        "NODE_EXTRA_CA_CERTS",
        // The credential and account-selection names themselves, for both vendors at once.
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_BASE_URL",
        "OPENAI_API_KEY",
        "CLAUDE_CONFIG_DIR",
        "CODEX_HOME",
        // A vendor namespace that reroutes the consultation without naming a credential.
        "CLAUDE_CODE_USE_BEDROCK",
        // agentmux's own namespace: the recursion marker, and the configuration a nested agentmux
        // would read once a delegate inherits its account's MCP servers.
        "AGENTMUX_DELEGATE",
        "AGENTMUX_CONFIG",
        "AWS_BEARER_TOKEN_BEDROCK",
        "http_proxy",
        // Something entirely innocuous, because the rule is an allowlist and not a judgement.
        "DISABLE_HOOKS",
    ];
    // The one name the operator did list must not widen what the caller may set.
    let mut config = Config::default();
    config.launch.request_env = vec!["REVIEW_MODE".to_owned()];

    for name in refused {
        let requested = [((name).to_owned(), "attacker".to_owned())]
            .into_iter()
            .collect();
        let plan = TurnPlan {
            question_path: Path::new("/runs/r/turns/0000/question.md"),
            last_message_path: Path::new("/runs/r/turns/0000/last-message.md"),
            resume: None,
            extra_env: &requested,
        };
        let result = Delegate::Claude {
            model: model("claude-opus-5")?,
            effort: effort("xhigh")?,
            account: None,
            isolation: None,
        }
        .invocation(&plan, &host_session_env(), &config);

        assert_that!(
            result.is_err(),
            eq(true),
            "a request was allowed to set {name}"
        );
    }
    Ok(())
}

/// A per-request environment reaches the delegate for the names the operator listed.
///
/// The point of the guard is to keep identity and routing out of a caller's hands, not to make the
/// parameter useless: switching off a hook inside a review is exactly what it is for.
#[gtest]
fn a_request_environment_reaches_the_delegate_when_listed() -> Result<()> {
    let mut config = Config::default();
    config.launch.request_env = vec!["DISABLE_HOOKS".to_owned()];
    let requested = [("DISABLE_HOOKS".to_owned(), "true".to_owned())]
        .into_iter()
        .collect();
    let plan = TurnPlan {
        question_path: Path::new("/runs/r/turns/0000/question.md"),
        last_message_path: Path::new("/runs/r/turns/0000/last-message.md"),
        resume: None,
        extra_env: &requested,
    };

    let invocation = Delegate::Claude {
        model: model("claude-opus-5")?,
        effort: effort("xhigh")?,
        account: None,
        isolation: None,
    }
    .invocation(&plan, &host_session_env(), &config)
    .or_fail()?;

    assert_that!(
        invocation.env.get("DISABLE_HOOKS").map(String::as_str),
        some(eq("true"))
    );
    Ok(())
}

/// A value no environment can carry is refused at the request, not blamed on the CLI.
///
/// `Command::spawn` refuses a NUL byte with an error that the launcher would report as "cannot
/// run `claude` … is it installed?", sending the caller to reinstall a working tool.
#[gtest]
fn a_request_value_with_a_nul_byte_is_refused_as_an_argument() -> Result<()> {
    let mut config = Config::default();
    config.launch.request_env = vec!["DISABLE_HOOKS".to_owned()];
    let requested = [("DISABLE_HOOKS".to_owned(), "a\0b".to_owned())]
        .into_iter()
        .collect();
    let plan = TurnPlan {
        question_path: Path::new("/runs/r/turns/0000/question.md"),
        last_message_path: Path::new("/runs/r/turns/0000/last-message.md"),
        resume: None,
        extra_env: &requested,
    };

    let error = Delegate::Claude {
        model: model("claude-opus-5")?,
        effort: effort("xhigh")?,
        account: None,
        isolation: None,
    }
    .invocation(&plan, &host_session_env(), &config)
    .expect_err("a NUL byte cannot reach the child");
    assert_that!(
        error,
        matches_pattern!(agentmux::delegate::DelegateError::Argument { .. })
    );
    Ok(())
}

/// Choosing an account withholds the host's credentials even when the machine layer forwarded
/// them.
///
/// `[launch] env_passthrough = ["ANTHROPIC_API_KEY"]` is a reasonable line on a machine that was
/// key-only when it was written; an account added later must still authenticate as itself, or
/// the caller believes it switched to a subscription while billing the key.
#[gtest]
fn an_account_withholds_a_credential_the_machine_layer_forwarded() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let mut config = config_with_personal(dir.path());
    config.launch.env_passthrough = vec!["ANTHROPIC_API_KEY".to_owned()];
    config.launch.env.insert(
        "ANTHROPIC_BASE_URL".to_owned(),
        Secret::new("http://gateway.internal"),
    );

    let invocation = build_with(
        &Delegate::Claude {
            model: model("claude-opus-5")?,
            effort: effort("xhigh")?,
            account: Some(AccountAlias::parse("personal").or_fail()?),
            isolation: None,
        },
        None,
        &config,
    )?;

    assert_that!(invocation.env.get("ANTHROPIC_API_KEY"), none());
    assert_that!(invocation.env.get("ANTHROPIC_BASE_URL"), none());
    assert_that!(
        invocation.env.get("CLAUDE_CONFIG_DIR").map(String::as_str),
        some(eq(dir.path().to_string_lossy().as_ref()))
    );
    Ok(())
}

/// The three environment layers apply in order, each overriding the one before.
///
/// Stated as one test because the ordering is the contract: a machine-wide setting is a default, an
/// account's setting is more specific, and one call's setting is the most specific of all.
#[gtest]
fn the_machine_account_and_request_layers_apply_in_that_order() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let mut config = config_with_personal(dir.path());
    config.launch.env.insert(
        "DELEGATE_TEST_LAYER".to_owned(),
        Secret::new("from-machine"),
    );
    config
        .launch
        .env
        .insert("MACHINE_ONLY".to_owned(), Secret::new("yes"));
    config.launch.request_env = vec!["DELEGATE_TEST_LAYER".to_owned()];
    if let Some(account) = config
        .accounts
        .get_mut("claude")
        .and_then(|table| table.get_mut("personal"))
    {
        account.launch.env.insert(
            "DELEGATE_TEST_LAYER".to_owned(),
            Secret::new("from-account"),
        );
    }

    let requested = [("DELEGATE_TEST_LAYER".to_owned(), "from-request".to_owned())]
        .into_iter()
        .collect();
    let plan = TurnPlan {
        question_path: Path::new("/runs/r/turns/0000/question.md"),
        last_message_path: Path::new("/runs/r/turns/0000/last-message.md"),
        resume: None,
        extra_env: &requested,
    };

    let invocation = Delegate::Claude {
        model: model("claude-opus-5")?,
        effort: effort("xhigh")?,
        account: Some(AccountAlias::parse("personal").or_fail()?),
        isolation: None,
    }
    .invocation(&plan, &host_session_env(), &config)
    .or_fail()?;

    // The most specific layer wins.
    assert_that!(
        invocation
            .env
            .get("DELEGATE_TEST_LAYER")
            .map(String::as_str),
        some(eq("from-request"))
    );
    // A layer that nothing overrides still reaches the delegate.
    assert_that!(
        invocation.env.get("MACHINE_ONLY").map(String::as_str),
        some(eq("yes"))
    );
    Ok(())
}

/// A configured default account applies when the caller names none.
///
/// Without this the machine and project `[defaults]` tables are decoration: a checkout that pins
/// its client's identity would authenticate as whatever the machine's default login happens to be,
/// which is the silently-wrong-identity failure the whole account mechanism exists to prevent.
#[gtest]
fn a_configured_default_applies_when_the_caller_names_none() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let mut config = config_with_personal(dir.path());
    config.defaults.insert(
        "claude".to_owned(),
        agentmux::config::Defaults {
            account: Some("personal".to_owned()),
        },
    );

    let invocation = build_with(
        &Delegate::Claude {
            model: model("claude-opus-5")?,
            effort: effort("xhigh")?,
            account: None,
            isolation: None,
        },
        None,
        &config,
    )?;

    assert_that!(
        invocation.env.get("CLAUDE_CONFIG_DIR").map(String::as_str),
        some(eq(dir.path().to_string_lossy().as_ref()))
    );
    // The default is a real account selection, so the host's key is withheld exactly as if the
    // caller had named it.
    assert_that!(invocation.env.get("ANTHROPIC_API_KEY"), none());
    Ok(())
}

/// A default naming an account nobody defined must point at the file that chose it.
///
/// The caller named nothing, so an error blaming its arguments sends it looking in the wrong
/// place entirely.
#[gtest]
fn a_default_that_cannot_be_resolved_names_the_file_that_selected_it() -> Result<()> {
    let mut config = Config {
        project_source: Some(std::path::PathBuf::from("/work/client/agentmux.toml")),
        project_defaults: ["claude".to_owned()].into_iter().collect(),
        ..Config::default()
    };
    config.defaults.insert(
        "claude".to_owned(),
        agentmux::config::Defaults {
            account: Some("clientx".to_owned()),
        },
    );
    let plan = TurnPlan {
        question_path: Path::new("/runs/r/turns/0000/question.md"),
        last_message_path: Path::new("/runs/r/turns/0000/last-message.md"),
        resume: None,
        extra_env: &BTreeMap::new(),
    };

    let error = Delegate::Claude {
        model: model("claude-opus-5")?,
        effort: effort("xhigh")?,
        account: None,
        isolation: None,
    }
    .invocation(&plan, &host_session_env(), &config)
    .expect_err("`clientx` is not defined");

    let message = error.to_string();
    assert_that!(message, contains_substring("clientx"));
    assert_that!(message, contains_substring("/work/client/agentmux.toml"));
    Ok(())
}

/// Inheriting drops exactly the flags that impose isolation, and nothing else.
///
/// The opt-out exists because the default is sometimes wrong: a reviewer meant to exercise the
/// project's own tooling needs that tooling loaded.
/// What it must not do is quietly widen what the delegate may change: plan mode and the
/// read-only tool list are a different question from whose configuration is loaded.
#[gtest]
fn inheriting_settings_drops_only_the_isolation_flags() -> Result<()> {
    let inherited = build(
        &Delegate::Claude {
            model: model("claude-opus-5")?,
            effort: effort("xhigh")?,
            account: None,
            isolation: Some(Isolation::Inherit),
        },
        None,
    )?;

    assert_that!(
        inherited.args.contains(&"--setting-sources".to_owned()),
        eq(false)
    );
    assert_that!(
        inherited.args.contains(&"--strict-mcp-config".to_owned()),
        eq(false)
    );
    // Still a second opinion rather than an edit.
    assert_that!(
        has_pair(&inherited.args, "--permission-mode", "plan"),
        eq(true)
    );
    assert_that!(inherited.args.contains(&"--tools".to_owned()), eq(true));

    let isolated = build(
        &Delegate::Claude {
            model: model("claude-opus-5")?,
            effort: effort("xhigh")?,
            account: None,
            isolation: None,
        },
        None,
    )?;
    assert_that!(
        isolated.args.contains(&"--setting-sources".to_owned()),
        eq(true)
    );
    assert_that!(
        isolated.args.contains(&"--strict-mcp-config".to_owned()),
        eq(true)
    );
    Ok(())
}

/// The same opt-out reaches Codex, whose isolation is one flag over the whole config file.
#[gtest]
fn a_codex_delegate_can_inherit_its_account_configuration() -> Result<()> {
    let (model, effort) = (model("gpt-6-astra")?, effort("high")?);
    let delegate = |isolation| Delegate::Codex {
        model: model.clone(),
        effort: effort.clone(),
        sandbox: CodexSandbox::ReadOnly,
        account: None,
        isolation,
    };

    let inherited = build(&delegate(Some(Isolation::Inherit)), None)?;
    assert_that!(
        inherited.args.contains(&"--ignore-user-config".to_owned()),
        eq(false)
    );
    assert_that!(
        inherited.args.contains(&"features.hooks=false".to_owned()),
        eq(false)
    );

    let isolated = build(&delegate(None), None)?;
    assert_that!(
        isolated.args.contains(&"--ignore-user-config".to_owned()),
        eq(true)
    );
    Ok(())
}

/// An account may ask for its own settings, so the choice sits next to the identity it belongs to.
#[gtest]
fn an_account_can_ask_for_its_own_settings() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let mut config = config_with_personal(dir.path());
    if let Some(account) = config
        .accounts
        .get_mut("claude")
        .and_then(|table| table.get_mut("personal"))
    {
        account.inherit_settings = Some(true);
    }

    let invocation = build_with(
        &Delegate::Claude {
            model: model("claude-opus-5")?,
            effort: effort("xhigh")?,
            account: Some(AccountAlias::parse("personal").or_fail()?),
            isolation: None,
        },
        None,
        &config,
    )?;

    assert_that!(
        invocation.args.contains(&"--setting-sources".to_owned()),
        eq(false)
    );

    // An explicit request still overrides the account's preference, in both directions.
    let forced = build_with(
        &Delegate::Claude {
            model: model("claude-opus-5")?,
            effort: effort("xhigh")?,
            account: Some(AccountAlias::parse("personal").or_fail()?),
            isolation: Some(Isolation::Isolated),
        },
        None,
        &config,
    )?;
    assert_that!(
        forced.args.contains(&"--setting-sources".to_owned()),
        eq(true)
    );
    Ok(())
}

/// Every delegate carries the marker that stops agentmux consulting itself.
///
/// An isolated delegate cannot reach agentmux at all, but an inheriting one loads the operator's
/// own MCP servers — and agentmux may be among them.
/// The marker is what makes the opt-out safe to offer rather than a recursion waiting to happen.
#[gtest]
fn every_delegate_is_marked_so_agentmux_cannot_consult_itself() -> Result<()> {
    for delegate in every_delegate()? {
        let dir = tempfile::tempdir().or_fail()?;
        let invocation = build_with(&delegate, None, &config_with_personal(dir.path()))?;
        assert_that!(
            invocation.env.get("AGENTMUX_DELEGATE").map(String::as_str),
            some(eq("1")),
            "{}",
            delegate.summary()
        );
    }
    Ok(())
}

/// An account a project file selects does not bring its own `request_env` with it.
///
/// A checkout may say which of the operator's accounts pays, and nothing more.
/// Its instructions can also tell the calling agent what `env` to pass, so an account whose
/// `request_env` names something the machine layer would refuse must not become reachable just
/// because the checkout chose it — while the same account, chosen by the machine file, keeps it.
#[gtest]
fn a_project_selected_account_does_not_widen_request_env() -> Result<()> {
    let mut config = Config::default();
    config
        .accounts
        .entry("claude".to_owned())
        .or_default()
        .insert(
            "debug".to_owned(),
            Account {
                api_key_env: Some("ANTHROPIC_API_KEY".to_owned()),
                launch: agentmux::config::LaunchEnv {
                    request_env: vec!["NODE_OPTIONS".to_owned()],
                    ..agentmux::config::LaunchEnv::default()
                },
                ..Account::default()
            },
        );
    config.defaults.insert(
        "claude".to_owned(),
        agentmux::config::Defaults {
            account: Some("debug".to_owned()),
        },
    );
    let requested = [("NODE_OPTIONS".to_owned(), "--require /tmp/x.js".to_owned())]
        .into_iter()
        .collect();
    let plan = TurnPlan {
        question_path: Path::new("/runs/r/turns/0000/question.md"),
        last_message_path: Path::new("/runs/r/turns/0000/last-message.md"),
        resume: None,
        extra_env: &requested,
    };
    let delegate = Delegate::Claude {
        model: model("claude-opus-5")?,
        effort: effort("xhigh")?,
        account: None,
        isolation: None,
    };

    // The machine's own default: the account's request_env applies.
    let machine = delegate.invocation(&plan, &host_session_env(), &config);
    assert_that!(machine.is_ok(), eq(true), "{machine:?}");

    // The same account chosen by the checkout: it does not.
    config.project_source = Some(std::path::PathBuf::from("/work/client/agentmux.toml"));
    config.project_defaults = ["claude".to_owned()].into_iter().collect();
    let project = delegate.invocation(&plan, &host_session_env(), &config);
    assert_that!(
        project.map(|_| ()),
        err(displays_as(contains_substring("cannot be set per request")))
    );
    Ok(())
}
