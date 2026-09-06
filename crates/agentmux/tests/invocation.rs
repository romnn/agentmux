//! What agentmux actually hands to a delegate CLI.
//!
//! The value of this crate is that its callers have no flags to forget, because the correct flags
//! are not expressible as anything else.
//! These tests are that claim, mechanised.

use std::collections::BTreeMap;
use std::path::Path;

use agentmux::delegate::{
    ClaudeAccount, CodexSandbox, Delegate, Effort, Invocation, ModelId, SessionRef, TurnPlan,
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
            account: ClaudeAccount::Work,
        },
        Delegate::Claude {
            model: model("claude-fable-5-1")?,
            effort: effort("xhigh")?,
            account: ClaudeAccount::Personal,
        },
        Delegate::Codex {
            model: model("gpt-6-astra")?,
            effort: effort("high")?,
            sandbox: CodexSandbox::ReadOnly,
        },
        Delegate::Codex {
            model: model("gpt-5.6-sol")?,
            effort: effort("xhigh")?,
            sandbox: CodexSandbox::WorkspaceWrite,
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
    let plan = TurnPlan {
        question_path: Path::new("/runs/r/turns/0000/question.md"),
        last_message_path: Path::new("/runs/r/turns/0000/last-message.md"),
        resume,
    };
    delegate.invocation(&plan, &host_session_env()).or_fail()
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
    for delegate in every_delegate()? {
        let invocation = build(&delegate, None)?;
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
    for delegate in every_delegate()? {
        let invocation = build(&delegate, None)?;
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

/// The personal account is the work account minus the API keys, plus its own config directory.
#[gtest]
fn the_personal_account_authenticates_as_itself() -> Result<()> {
    let personal = build(
        &Delegate::Claude {
            model: model("claude-opus-5")?,
            effort: effort("xhigh")?,
            account: ClaudeAccount::Personal,
        },
        None,
    )?;
    assert_that!(personal.env.get("ANTHROPIC_API_KEY"), none());
    assert_that!(personal.env.get("ANTHROPIC_AUTH_TOKEN"), none());
    assert_that!(
        personal.env.get("CLAUDE_CONFIG_DIR").map(String::as_str),
        some(eq("/home/dev/.claude-personal"))
    );

    let work = build(
        &Delegate::Claude {
            model: model("claude-opus-5")?,
            effort: effort("xhigh")?,
            account: ClaudeAccount::Work,
        },
        None,
    )?;
    assert_that!(
        work.env.get("ANTHROPIC_API_KEY").map(String::as_str),
        some(eq("sk-ant-secret"))
    );
    // The host's own CLAUDE_CONFIG_DIR must not select the account.
    assert_that!(work.env.get("CLAUDE_CONFIG_DIR"), none());
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
            account: ClaudeAccount::Work,
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
