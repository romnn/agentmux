//! Records exactly as an earlier agentmux wrote them, read back through the public entry points.
//!
//! Each consultation here is started through the scripted launcher, and its `meta.json` and first
//! `invocation.json` are then replaced with a frozen text.
//! The texts are what a release that predates every change to the delegate's shape wrote, with
//! the edges a record can have: a network switched off, an empty tool list, permissions and
//! isolation left out, an account spelled as an alias or as the CLI's own login, keys this build
//! does not know, and an invocation record with no delegate in it.
//! A change that reads any of them differently fails here rather than on a machine with
//! consultations already on disk.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agentmux::delegate::{
    AccountChoice, Delegate, ModelId, Overrides, Permissions, SessionRef, Vendor,
};
use agentmux::run::{ListFilter, Retention, RunId, RunStore, StartRequest};
use agentmux::testing::{RecordedLaunch, Script, ScriptedLauncher, fixtures};
use agentmux::vendor::claude::ClaudePermissions;
use agentmux::vendor::codex::{CodexPermissions, CodexSandbox};
use googletest::prelude::*;

/// A machine with one configured Claude account, `work`, and a store that launches every turn
/// directly, so each turn settles when it is next read.
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
        let config_path = home.path().join("agentmux.toml");
        agentmux::testing::write_config_file(
            &config_path,
            format!(
                "[accounts.claude.work]\nconfig_dir = {}\n",
                toml_string(&work)?
            ),
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
    /// texts, `<run_id>` and `<cwd>` filled in.
    fn freeze(&self, run_id: &RunId, meta: &str, invocation: &str) -> Result<()> {
        let fill = |text: &str| -> Result<String> {
            Ok(text
                .replace("<run_id>", run_id.as_str())
                .replace("<cwd>", &json_path(self.home.path())?))
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
/// every permission, with a key in its delegate this build does not read.
const CLAUDE_META: &str = indoc::indoc! {r#"
    {
      "run_id": "<run_id>",
      "delegate": {
        "vendor": "claude",
        "model": "opus",
        "effort": "xhigh",
        "account": "work",
        "isolation": "inherit",
        "permission_mode": "acceptEdits",
        "tools": [],
        "allowed_tools": [
          "WebFetch",
          "Bash(cargo check *)"
        ],
        "disallowed_tools": [
          "Bash(git push *)"
        ],
        "written_by_a_newer_agentmux": true
      },
      "cwd": "<cwd>",
      "retention": "ttl",
      "created_at": "2026-11-20T10:00:00Z",
      "env": {},
      "billing": "subscription"
    }
"#};

/// Its first turn, which ran as the consultation's record says.
const CLAUDE_INVOCATION: &str = indoc::indoc! {r#"
    {
      "program": "claude",
      "args": [],
      "env_keys": [],
      "cwd": "<cwd>",
      "mode": "exec",
      "billing": "subscription",
      "delegate": {
        "vendor": "claude",
        "model": "opus",
        "effort": "xhigh",
        "account": "work",
        "isolation": "inherit",
        "permission_mode": "acceptEdits",
        "tools": [],
        "allowed_tools": [
          "WebFetch",
          "Bash(cargo check *)"
        ],
        "disallowed_tools": [
          "Bash(git push *)"
        ]
      }
    }
"#};

/// A Codex consultation on the CLI's own login, started read-only and isolated, with its network
/// left to Codex.
const CODEX_META: &str = indoc::indoc! {r#"
    {
      "run_id": "<run_id>",
      "delegate": {
        "vendor": "codex",
        "model": "sol",
        "effort": "high",
        "sandbox": "read_only",
        "account": {
          "kind": "cli_login"
        },
        "isolation": "isolated"
      },
      "cwd": "<cwd>",
      "retention": "ttl",
      "created_at": "2026-11-20T10:00:00Z",
      "env": {},
      "billing": "subscription"
    }
"#};

/// Its first turn, which a follow-up moved to a workspace-write sandbox with no network.
const CODEX_INVOCATION: &str = indoc::indoc! {r#"
    {
      "program": "codex",
      "args": [],
      "env_keys": [],
      "cwd": "<cwd>",
      "mode": "exec",
      "billing": "subscription",
      "delegate": {
        "vendor": "codex",
        "model": "sol",
        "effort": "high",
        "sandbox": "workspace_write",
        "network_access": false,
        "account": {
          "kind": "cli_login"
        },
        "isolation": "isolated"
      }
    }
"#};

/// A Codex consultation whose commands may not reach the network, recorded read-only.
const CODEX_OFFLINE_META: &str = indoc::indoc! {r#"
    {
      "run_id": "<run_id>",
      "delegate": {
        "vendor": "codex",
        "model": "sol",
        "sandbox": "read_only",
        "network_access": false,
        "account": {
          "kind": "cli_login"
        },
        "isolation": "isolated"
      },
      "cwd": "<cwd>",
      "retention": "ttl",
      "created_at": "2026-11-20T10:00:00Z",
      "env": {},
      "billing": "subscription"
    }
"#};

/// An agy consultation from before the account and the isolation were pinned, with a key in its
/// delegate this build does not read.
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
      "created_at": "2026-11-20T10:00:00Z",
      "env": {}
    }
"#};

/// An invocation record from before the delegate a turn ran as was kept with it.
const INVOCATION_WITHOUT_DELEGATE: &str = indoc::indoc! {r#"
    {
      "program": "agy",
      "args": [],
      "env_keys": [],
      "cwd": "<cwd>",
      "mode": "exec"
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

    // `status` and `list` describe what the record says, the key it does not know aside.
    let summary = "claude opus effort=xhigh permission_mode=acceptEdits tools= \
                   allowed_tools=WebFetch,Bash(cargo check *) \
                   disallowed_tools=Bash(git push *) account=work settings=inherited";
    assert_that!(
        machine.store.status(&run_id)?.delegate.summary(),
        eq(summary)
    );
    assert_that!(machine.listed(&run_id)?, eq(summary));

    machine.store.follow_up(&run_id, "again")?;

    // The follow-up resumes the session with every recorded permission, and no isolation flag.
    let session = session_of(Vendor::Claude, fixtures::CLAUDE_HAPPY)?;
    let launch = machine.launch(1)?;
    assert_that!(
        launch.args,
        elements_are![
            eq("--print"),
            eq("--resume"),
            eq(session.as_str()),
            eq("--model"),
            eq("opus"),
            eq("--effort"),
            eq("xhigh"),
            eq("--permission-mode"),
            eq("acceptEdits"),
            eq("--tools"),
            eq(""),
            eq("--allowedTools"),
            eq("WebFetch,Bash(cargo check *)"),
            eq("--disallowedTools"),
            eq("Bash(git push *)"),
            eq("--output-format"),
            eq("stream-json"),
            eq("--verbose"),
        ]
    );
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

    let summary = "codex sol effort=high sandbox=workspace-write+no-network account=cli-default";
    assert_that!(
        machine.store.status(&run_id)?.delegate.summary(),
        eq(summary)
    );
    assert_that!(machine.listed(&run_id)?, eq(summary));

    machine.store.follow_up(&run_id, "again")?;

    let session = session_of(Vendor::Codex, fixtures::CODEX_HAPPY)?;
    let last_message = machine
        .run_dir(&run_id)
        .join("turns/0002/last-message.md")
        .to_string_lossy()
        .into_owned();
    assert_that!(
        machine.launch(1)?.args,
        elements_are![
            eq("exec"),
            eq("resume"),
            eq(session.as_str()),
            eq("--json"),
            eq("--skip-git-repo-check"),
            eq("--model"),
            eq("sol"),
            eq("--config"),
            eq("model_reasoning_effort=\"high\""),
            eq("--ignore-user-config"),
            eq("--config"),
            eq("features.hooks=false"),
            eq("--config"),
            eq("sandbox_mode=\"workspace-write\""),
            eq("--config"),
            eq("sandbox_workspace_write.network_access=false"),
            eq("--config"),
            eq("approval_policy=\"never\""),
            eq("--config"),
            eq("approvals_reviewer=\"user\""),
            eq("--output-last-message"),
            eq(last_message.as_str()),
            eq("-"),
        ]
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
    machine.freeze(&run_id, CODEX_OFFLINE_META, INVOCATION_WITHOUT_DELEGATE)?;

    machine.store.follow_up_with(
        &run_id,
        "write it out",
        Overrides {
            permissions: Some(Permissions::Codex(CodexPermissions {
                sandbox: Some(CodexSandbox::WorkspaceWrite),
                ..CodexPermissions::default()
            })),
            ..Overrides::default()
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
        eq("codex sol effort=cli-default sandbox=workspace-write+no-network account=cli-default")
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

    let session = session_of(Vendor::Agy, fixtures::AGY_TOOL_USE)?;
    let launch = machine.launch(1)?;
    assert_that!(
        launch.args,
        elements_are![
            eq("--print="),
            eq("--input-format"),
            eq("stream-json"),
            eq("--output-format"),
            eq("stream-json"),
            eq("--conversation"),
            eq(session.as_str()),
            eq("--model"),
            eq("gemini-3.8-flash-low"),
            eq("--settings"),
            eq(
                r#"{"agentMode":"","permissions":{"ask":[],"deny":["command(*)","write_file(*)","mcp(*)","execute_url(*)","download_url(*)"]},"toolPermission":"request-review","useG1Credits":false}"#
            ),
        ]
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
