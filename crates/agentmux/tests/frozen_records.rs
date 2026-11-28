//! Records exactly as an earlier agentmux wrote them, read back through the public entry points.
//!
//! Each consultation here is started through the scripted launcher, and its `meta.json` and first
//! `invocation.json` are then replaced with a frozen text.
//! The texts are records agentmux wrote at commit 944b80d, before any change to the delegate's
//! shape, with the recording machine's paths replaced by placeholders.
//! Where a test needs an edge none of those records has, its constant is one of them with a
//! single edit, and the constant's doc names it: an empty tool list, a network a follow-up
//! switched off, keys this build does not know, and records from before the isolation, the
//! account or the delegate were kept.
//! A change that reads any of them differently fails here rather than on a machine with
//! consultations already on disk.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agentmux::delegate::{
    AccountChoice, Delegate, ModelId, PermissionOptions, Permissions, SessionRef, Vendor,
};
use agentmux::run::{ListFilter, Retention, RunId, RunStore, StartRequest};
use agentmux::testing::{RecordedLaunch, Script, ScriptedLauncher, fixtures};
use agentmux::vendor::claude::ClaudePermissions;
use agentmux::vendor::codex::{CodexPermissions, CodexSandbox};
use googletest::prelude::*;

/// A machine with the accounts the frozen records name, Claude's `work` and Codex's `personal`,
/// and a store that launches every turn directly, so each turn settles when it is next read.
struct Machine {
    store: RunStore,
    launcher: Arc<ScriptedLauncher>,
    home: tempfile::TempDir,
    state: tempfile::TempDir,
}

impl Machine {
    fn new(scripts: impl IntoIterator<Item = Script>) -> Result<Self> {
        let home = tempfile::tempdir().or_fail()?;
        let state = tempfile::tempdir().or_fail()?;
        let work = home.path().join("claude-work");
        std::fs::create_dir(&work).or_fail()?;
        let personal = home.path().join("codex-personal");
        std::fs::create_dir(&personal).or_fail()?;
        let config_path = home.path().join("agentmux.toml");
        agentmux::testing::write_config_file(
            &config_path,
            indoc::formatdoc! {"
                [accounts.claude.work]
                config_dir = {work}

                [accounts.codex.personal]
                config_dir = {personal}
                ",
                work = toml_string(&work)?,
                personal = toml_string(&personal)?,
            },
        )
        .or_fail()?;
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
        let launcher = Arc::new(ScriptedLauncher::new(scripts));
        let store = RunStore::open(state.path(), launcher.clone(), env).or_fail()?;
        Ok(Self {
            store,
            launcher,
            home,
            state,
        })
    }

    /// Start a consultation with `delegate`, whose records [`Machine::freeze`] then replaces.
    fn start(&self, delegate: Delegate) -> Result<RunId> {
        let status = self.store.start(&StartRequest {
            delegate,
            question: "q".to_owned(),
            cwd: self.home.path().to_path_buf(),
            retention: Retention::Ttl,
            env: BTreeMap::new(),
            billing: None,
            force_codex_exec: false,
            label: None,
        })?;
        Ok(status.run_id)
    }

    fn run_dir(&self, run_id: &RunId) -> PathBuf {
        self.state.path().join("runs").join(run_id.as_str())
    }

    /// Replace the consultation's `meta.json` and its first turn's `invocation.json` with frozen
    /// texts, `<run_id>`, `<cwd>`, `<home>` and the store's `<root>` filled in.
    fn freeze(&self, run_id: &RunId, meta: &str, invocation: &str) -> Result<()> {
        let fill = |text: &str| -> Result<String> {
            Ok(text
                .replace("<run_id>", run_id.as_str())
                .replace("<cwd>", &json_path(self.home.path())?)
                .replace("<home>", &json_path(self.home.path())?)
                .replace("<root>", &json_path(self.state.path())?))
        };
        let run = self.run_dir(run_id);
        std::fs::write(run.join("meta.json"), fill(meta)?).or_fail()?;
        std::fs::write(run.join("turns/0001/invocation.json"), fill(invocation)?).or_fail()?;
        Ok(())
    }

    /// The `index`th launch, counting from zero.
    fn launch(&self, index: usize) -> Result<RecordedLaunch> {
        self.launcher.launches().get(index).cloned().or_fail()
    }

    /// The one-line description `list` gives the consultation.
    fn listed(&self, run_id: &RunId) -> Result<String> {
        let listed = self.store.list(10, &ListFilter::default())?;
        Ok(listed
            .into_iter()
            .find(|summary| summary.run_id == *run_id)
            .or_fail()?
            .delegate)
    }
}

/// A path as a TOML basic string.
fn toml_string(path: &Path) -> Result<String> {
    Ok(toml::Value::String(path.to_str().or_fail()?.to_owned()).to_string())
}

/// A path as the inside of a JSON string, for a frozen text's `<cwd>`.
fn json_path(path: &Path) -> Result<String> {
    let quoted = serde_json::to_string(path.to_str().or_fail()?).or_fail()?;
    Ok(quoted.trim_matches('"').to_owned())
}

/// The session the first turn's capture announced, which a follow-up resumes.
fn session_of(vendor: Vendor, capture: &str) -> Result<SessionRef> {
    agentmux::stream::fold(vendor, 0, "q", capture)
        .session
        .or_fail()
}

fn claude() -> Result<Delegate> {
    Ok(Delegate {
        model: ModelId::parse("opus").or_fail()?,
        effort: None,
        account: AccountChoice::Default,
        isolation: None,
        permissions: Permissions::Claude(ClaudePermissions::default()),
    })
}

fn codex() -> Result<Delegate> {
    Ok(Delegate {
        model: ModelId::parse("sol").or_fail()?,
        effort: None,
        account: AccountChoice::Default,
        isolation: None,
        permissions: Permissions::Codex(CodexPermissions::default()),
    })
}

fn agy() -> Result<Delegate> {
    Ok(Delegate {
        model: ModelId::parse("gemini-3.8-flash-low").or_fail()?,
        effort: None,
        account: AccountChoice::Default,
        isolation: None,
        permissions: Permissions::Agy,
    })
}

/// A Claude consultation that runs as a named account, loads the user's settings, and chose
/// every permission.
///
/// Edited from a record agentmux wrote so that its delegate offers no tools at all, which
/// agentmux records as an empty list.
/// Its delegate also carries a synthetic key this build does not read, as a newer agentmux could
/// write.
const CLAUDE_META: &str = indoc::indoc! {r#"
    {
      "run_id": "<run_id>",
      "delegate": {
        "vendor": "claude",
        "model": "claude-haiku-5-5",
        "effort": "max",
        "account": "work",
        "isolation": "inherit",
        "permission_mode": "acceptEdits",
        "tools": [],
        "allowed_tools": [
          "WebFetch"
        ],
        "disallowed_tools": [
          "Bash(git push *)"
        ],
        "written_by_a_newer_agentmux": true
      },
      "cwd": "<cwd>",
      "retention": "ttl",
      "created_at": "2026-11-28T01:47:11.660880512Z",
      "env": {},
      "rewritten_from": "haiku",
      "billing": "subscription"
    }
"#};

/// Its first turn, which ran as the consultation's record says.
///
/// Edited from what agentmux wrote only to offer no tools, in its delegate and its `--tools`.
const CLAUDE_INVOCATION: &str = indoc::indoc! {r#"
    {
      "program": "claude",
      "args": [
        "--print",
        "--model",
        "claude-haiku-5-5",
        "--effort",
        "max",
        "--permission-mode",
        "acceptEdits",
        "--tools",
        "",
        "--allowedTools",
        "WebFetch",
        "--disallowedTools",
        "Bash(git push *)",
        "--output-format",
        "stream-json",
        "--verbose",
        "--input-format",
        "stream-json",
        "--replay-user-messages",
        "--permission-prompts",
        "none"
      ],
      "env_keys": [
        "AGENTMUX_DELEGATE",
        "AGENTMUX_STATE_DIR",
        "CI",
        "HOME",
        "LANG",
        "LC_ALL",
        "LOGNAME",
        "NO_COLOR",
        "PATH",
        "SHELL",
        "TERM",
        "USER"
      ],
      "cwd": "<cwd>",
      "mode": "claude_stream_input",
      "billing": "subscription",
      "config_dir": "<home>/claude-work",
      "cli_version": "2.1.277 (Claude Code)",
      "agentmux_version": "0.0.13",
      "delegate": {
        "vendor": "claude",
        "model": "claude-haiku-5-5",
        "effort": "max",
        "account": "work",
        "isolation": "inherit",
        "permission_mode": "acceptEdits",
        "tools": [],
        "allowed_tools": [
          "WebFetch"
        ],
        "disallowed_tools": [
          "Bash(git push *)"
        ]
      },
      "label": "claude-inherit"
    }
"#};

/// A Codex consultation as a named account, started isolated in a workspace-write sandbox whose
/// network is on.
const CODEX_META: &str = indoc::indoc! {r#"
    {
      "run_id": "<run_id>",
      "delegate": {
        "vendor": "codex",
        "model": "gpt-6-luna",
        "effort": "medium",
        "sandbox": "workspace_write",
        "network_access": true,
        "account": "personal",
        "isolation": "isolated"
      },
      "cwd": "<cwd>",
      "retention": "ttl",
      "created_at": "2026-11-28T01:47:13.310575724Z",
      "env": {},
      "rewritten_from": "luna",
      "billing": "subscription"
    }
"#};

/// Its newest turn, run by a follow-up that switched the network off.
///
/// Edited from a first turn agentmux wrote, which kept the network on, in its delegate's
/// `network_access` and the `--config` that sets it.
/// None of the recorded consultations has a follow-up that changed the network.
const CODEX_INVOCATION: &str = indoc::indoc! {r#"
    {
      "program": "codex",
      "args": [
        "exec",
        "--json",
        "--skip-git-repo-check",
        "--model",
        "gpt-6-luna",
        "--config",
        "model_reasoning_effort=\"medium\"",
        "--ignore-user-config",
        "--config",
        "features.hooks=false",
        "--sandbox",
        "workspace-write",
        "--config",
        "sandbox_workspace_write.network_access=false",
        "--config",
        "approval_policy=\"never\"",
        "--config",
        "approvals_reviewer=\"user\"",
        "--output-last-message",
        "<root>/runs/<run_id>/turns/0001/last-message.md",
        "-"
      ],
      "env_keys": [
        "AGENTMUX_DELEGATE",
        "AGENTMUX_STATE_DIR",
        "CI",
        "HOME",
        "LANG",
        "LC_ALL",
        "LOGNAME",
        "NO_COLOR",
        "PATH",
        "SHELL",
        "TERM",
        "USER"
      ],
      "cwd": "<cwd>",
      "mode": "exec",
      "billing": "subscription",
      "config_dir": "<home>/codex-personal",
      "cli_version": "codex-cli 0.163.0",
      "agentmux_version": "0.0.13",
      "delegate": {
        "vendor": "codex",
        "model": "gpt-6-luna",
        "effort": "medium",
        "sandbox": "workspace_write",
        "network_access": false,
        "account": "personal",
        "isolation": "isolated"
      },
      "label": "codex-isolated"
    }
"#};

/// A Codex consultation forced onto `codex exec`, whose commands may not reach the network,
/// recorded read-only.
const CODEX_OFFLINE_META: &str = indoc::indoc! {r#"
    {
      "run_id": "<run_id>",
      "delegate": {
        "vendor": "codex",
        "model": "gpt-6-luna",
        "effort": "medium",
        "sandbox": "read_only",
        "network_access": false,
        "account": "personal",
        "isolation": "inherit"
      },
      "cwd": "<cwd>",
      "retention": "ttl",
      "created_at": "2026-11-28T01:47:13.000746167Z",
      "env": {},
      "rewritten_from": "luna",
      "billing": "subscription",
      "codex_exec": true
    }
"#};

/// Its first turn, which ran as the consultation's record says.
const CODEX_OFFLINE_INVOCATION: &str = indoc::indoc! {r#"
    {
      "program": "codex",
      "args": [
        "exec",
        "--json",
        "--skip-git-repo-check",
        "--model",
        "gpt-6-luna",
        "--config",
        "model_reasoning_effort=\"medium\"",
        "--sandbox",
        "read-only",
        "--config",
        "approval_policy=\"never\"",
        "--config",
        "approvals_reviewer=\"user\"",
        "--output-last-message",
        "<root>/runs/<run_id>/turns/0001/last-message.md",
        "-"
      ],
      "env_keys": [
        "AGENTMUX_DELEGATE",
        "AGENTMUX_STATE_DIR",
        "CI",
        "HOME",
        "LANG",
        "LC_ALL",
        "LOGNAME",
        "NO_COLOR",
        "PATH",
        "SHELL",
        "TERM",
        "USER"
      ],
      "cwd": "<cwd>",
      "mode": "exec",
      "billing": "subscription",
      "config_dir": "<home>/codex-personal",
      "cli_version": "codex-cli 0.163.0",
      "agentmux_version": "0.0.13",
      "delegate": {
        "vendor": "codex",
        "model": "gpt-6-luna",
        "effort": "medium",
        "sandbox": "read_only",
        "network_access": false,
        "account": "personal",
        "isolation": "inherit"
      },
      "label": "codex-exec"
    }
"#};

/// A synthetic agy consultation from before the account and the isolation were pinned, with a
/// key in its delegate this build does not read.
///
/// It is an isolated agy record agentmux wrote, with the delegate's `account` and `isolation`
/// taken out and the unknown key put in.
const AGY_META: &str = indoc::indoc! {r#"
    {
      "run_id": "<run_id>",
      "delegate": {
        "vendor": "agy",
        "model": "gemini-3.8-flash-low",
        "written_by_a_newer_agentmux": "yes"
      },
      "cwd": "<cwd>",
      "retention": "ttl",
      "created_at": "2026-11-28T01:47:13.767901124Z",
      "env": {},
      "billing": "subscription"
    }
"#};

/// A synthetic invocation record from before the delegate a turn ran as was kept with it.
///
/// It is the first turn of an isolated agy consultation agentmux wrote, with its `delegate`
/// taken out.
const INVOCATION_WITHOUT_DELEGATE: &str = indoc::indoc! {r#"
    {
      "program": "agy",
      "args": [
        "--print=",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--model",
        "gemini-3.8-flash-low",
        "--settings",
        "{\"agentMode\":\"\",\"permissions\":{\"ask\":[],\"deny\":[\"command(*)\",\"write_file(*)\",\"mcp(*)\",\"execute_url(*)\",\"download_url(*)\"]},\"toolPermission\":\"request-review\",\"useG1Credits\":false}"
      ],
      "env_keys": [
        "AGENTMUX_DELEGATE",
        "AGENTMUX_STATE_DIR",
        "CI",
        "HOME",
        "LANG",
        "LC_ALL",
        "LOGNAME",
        "NO_COLOR",
        "PATH",
        "SHELL",
        "TERM",
        "USER",
        "USERPROFILE"
      ],
      "cwd": "<cwd>",
      "mode": "exec",
      "billing": "subscription",
      "config_dir": "<root>/agy-home/.gemini/antigravity-cli",
      "cli_version": "1.5.18",
      "agentmux_version": "0.0.13",
      "label": "agy-isolated"
    }
"#};

/// A frozen Claude record reads with every permission it chose, and its follow-up launches with
/// all of them, as the account it names, loading the user's settings.
#[gtest]
fn a_frozen_claude_record_keeps_every_permission() -> Result<()> {
    let machine = Machine::new([
        Script::completed(fixtures::CLAUDE_HAPPY),
        Script::completed(fixtures::CLAUDE_RESUME),
    ])?;
    let run_id = machine.start(claude()?)?;
    machine.freeze(&run_id, CLAUDE_META, CLAUDE_INVOCATION)?;

    // `status` and `list` describe what the record says, the key it does not know aside, and
    // `list` names the model the caller asked for, which the record keeps.
    let summary = "claude claude-haiku-5-5 effort=max permission_mode=acceptEdits tools= \
                   allowed_tools=WebFetch disallowed_tools=Bash(git push *) account=work \
                   settings=inherited";
    assert_that!(
        machine.store.status(&run_id)?.delegate.summary(),
        eq(summary)
    );
    assert_that!(
        machine.listed(&run_id)?,
        eq(&format!("{summary} (asked for haiku)"))
    );

    machine.store.follow_up(&run_id, "again")?;

    // The follow-up resumes the session with every recorded permission, and loads the user's
    // settings, as the record's isolation says.
    let session = session_of(Vendor::Claude, fixtures::CLAUDE_HAPPY)?;
    let launch = machine.launch(1)?;
    assert_that!(launch.has_flag_with("--resume", session.as_str()), eq(true));
    assert_that!(launch.has_flag_with("--effort", "max"), eq(true));
    assert_that!(
        launch.has_flag_with("--permission-mode", "acceptEdits"),
        eq(true)
    );
    assert_that!(launch.has_flag_with("--tools", ""), eq(true));
    assert_that!(launch.has_flag_with("--allowedTools", "WebFetch"), eq(true));
    assert_that!(
        launch.has_flag_with("--disallowedTools", "Bash(git push *)"),
        eq(true)
    );
    assert_that!(launch.has_flag("--setting-sources"), eq(false));
    // It runs as the account the record names.
    assert_that!(
        launch.env.get("CLAUDE_CONFIG_DIR"),
        some(eq(&machine
            .home
            .path()
            .join("claude-work")
            .to_string_lossy()
            .into_owned()))
    );
    Ok(())
}

/// A frozen Codex record reads with what its newest turn ran with, and its follow-up launches
/// with that turn's sandbox and its network switched off.
#[gtest]
fn a_frozen_codex_record_continues_as_its_newest_turn_ran() -> Result<()> {
    let machine = Machine::new([
        Script::completed(fixtures::CODEX_HAPPY),
        Script::completed(fixtures::CODEX_HAPPY),
    ])?;
    let run_id = machine.start(codex()?)?;
    machine.freeze(&run_id, CODEX_META, CODEX_INVOCATION)?;

    let summary =
        "codex gpt-6-luna effort=medium sandbox=workspace-write+no-network account=personal";
    assert_that!(
        machine.store.status(&run_id)?.delegate.summary(),
        eq(summary)
    );
    assert_that!(
        machine.listed(&run_id)?,
        eq(&format!("{summary} (asked for luna)"))
    );

    machine.store.follow_up(&run_id, "again")?;

    // The follow-up resumes the session isolated, as the consultation's record says, with the
    // newest turn's sandbox and its network switched off.
    let session = session_of(Vendor::Codex, fixtures::CODEX_HAPPY)?;
    let launch = machine.launch(1)?;
    assert_that!(launch.has_flag_with("resume", session.as_str()), eq(true));
    assert_that!(
        launch.has_flag_with("--config", "model_reasoning_effort=\"medium\""),
        eq(true)
    );
    assert_that!(launch.has_flag("--ignore-user-config"), eq(true));
    assert_that!(
        launch.has_flag_with("--config", "sandbox_mode=\"workspace-write\""),
        eq(true)
    );
    assert_that!(
        launch.has_flag_with("--config", "sandbox_workspace_write.network_access=false"),
        eq(true)
    );
    // It runs as the account the record names.
    assert_that!(
        launch.env.get("CODEX_HOME"),
        some(eq(&machine
            .home
            .path()
            .join("codex-personal")
            .to_string_lossy()
            .into_owned()))
    );
    Ok(())
}

/// A follow-up that changes only the sandbox of a frozen Codex record keeps its network switched
/// off, rather than leaving the network to Codex's own configuration.
#[gtest]
fn a_partial_override_keeps_a_frozen_network_choice() -> Result<()> {
    let machine = Machine::new([
        Script::completed(fixtures::CODEX_HAPPY),
        Script::completed(fixtures::CODEX_HAPPY),
    ])?;
    let run_id = machine.start(codex()?)?;
    machine.freeze(&run_id, CODEX_OFFLINE_META, CODEX_OFFLINE_INVOCATION)?;

    machine.store.follow_up_with(
        &run_id,
        "write it out",
        PermissionOptions {
            sandbox: Some(CodexSandbox::WorkspaceWrite),
            ..PermissionOptions::default()
        },
        None,
    )?;

    let launch = machine.launch(1)?;
    assert_that!(
        launch.has_flag_with("--config", "sandbox_mode=\"workspace-write\""),
        eq(true)
    );
    assert_that!(
        launch.has_flag_with("--config", "sandbox_workspace_write.network_access=false"),
        eq(true)
    );
    assert_that!(
        machine.store.status(&run_id)?.delegate.summary(),
        eq(
            "codex gpt-6-luna effort=medium sandbox=workspace-write+no-network account=personal \
            settings=inherited"
        )
    );
    Ok(())
}

/// A frozen agy record from before isolation was pinned reads as isolated, as it could only have
/// run, and its follow-up resumes the conversation in the home the store keeps, reading its
/// question as the record agy takes.
#[gtest]
fn a_frozen_agy_record_continues_isolated() -> Result<()> {
    let machine = Machine::new([
        Script::completed(fixtures::AGY_TOOL_USE),
        Script::completed(fixtures::AGY_RESUME),
    ])?;
    let run_id = machine.start(agy()?)?;
    machine.freeze(&run_id, AGY_META, INVOCATION_WITHOUT_DELEGATE)?;

    let summary = "agy gemini-3.8-flash-low effort=cli-default account=cli-default";
    assert_that!(
        machine.store.status(&run_id)?.delegate.summary(),
        eq(summary)
    );
    assert_that!(machine.listed(&run_id)?, eq(summary));

    machine.store.follow_up(&run_id, "again")?;

    // The follow-up resumes the conversation in the home the store keeps for an isolated agy.
    let session = session_of(Vendor::Agy, fixtures::AGY_TOOL_USE)?;
    let launch = machine.launch(1)?;
    assert_that!(
        launch.has_flag_with("--conversation", session.as_str()),
        eq(true)
    );
    let agy_home = machine
        .state
        .path()
        .join("agy-home")
        .to_string_lossy()
        .into_owned();
    assert_that!(launch.env.get("HOME"), some(eq(&agy_home)));
    // The question reached agy's stdin as the one stream-json record it reads.
    assert_that!(
        launch.question,
        eq("{\"event\":\"user\",\"message\":{\"content\":\"again\"}}\n")
    );
    Ok(())
}
