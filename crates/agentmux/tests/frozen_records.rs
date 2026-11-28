//! Records exactly as an earlier agentmux wrote them, read back through the public entry points.
//!
//! Each consultation here is started through the scripted launcher, and its `meta.json` and first
//! `invocation.json` are then replaced with a pair under `fixtures/records/`.
//! Each pair is one agentmux wrote at commit 944b80d, before any change to the delegate's shape,
//! with the recording machine's paths replaced by placeholders and nothing else changed.
//! Where a test needs an edge none of them has, it edits a copy and says what it changed and why.
//! A change that reads any of them differently fails here rather than on a machine with
//! consultations already on disk.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agentmux::delegate::{
    AccountAlias, AccountChoice, Delegate, Effort, Isolation, ModelId, PermissionOptions,
    Permissions, Vendor,
};
use agentmux::run::{Retention, RunId, RunStore, StartRequest};
use agentmux::testing::{RecordedLaunch, Script, ScriptedLauncher, fixtures};
use agentmux::vendor::claude::{ClaudePermissionMode, ClaudePermissions, ClaudeTool};
use agentmux::vendor::codex::{CodexPermissions, CodexSandbox};
use googletest::prelude::*;
use serde_json::{Value, json};

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
        let env = BTreeMap::from([
            ("PATH".to_owned(), "/usr/bin".to_owned()),
            ("HOME".to_owned(), home.path().display().to_string()),
            (
                "AGENTMUX_CONFIG".to_owned(),
                config_path.display().to_string(),
            ),
        ]);
        let launcher = Arc::new(ScriptedLauncher::new(scripts));
        let store = RunStore::open(state.path(), launcher.clone(), env).or_fail()?;
        Ok(Self {
            store,
            launcher,
            home,
            state,
        })
    }

    /// Start a consultation with `delegate`, and replace its records with `meta` and
    /// `invocation`, `<run_id>`, `<cwd>`, `<home>` and the store's `<root>` filled in.
    fn frozen(&self, delegate: Delegate, meta: &Value, invocation: &Value) -> Result<RunId> {
        let run_id = self
            .store
            .start(&StartRequest {
                delegate,
                question: "q".to_owned(),
                cwd: self.home.path().to_path_buf(),
                retention: Retention::Ttl,
                env: BTreeMap::new(),
                billing: None,
                force_codex_exec: false,
                label: None,
            })?
            .run_id;
        let fill = |record: &Value| -> Result<String> {
            Ok(serde_json::to_string(record)
                .or_fail()?
                .replace("<run_id>", run_id.as_str())
                .replace("<cwd>", &json_path(self.home.path())?)
                .replace("<home>", &json_path(self.home.path())?)
                .replace("<root>", &json_path(self.state.path())?))
        };
        let run = self.state.path().join("runs").join(run_id.as_str());
        std::fs::write(run.join("meta.json"), fill(meta)?).or_fail()?;
        std::fs::write(run.join("turns/0001/invocation.json"), fill(invocation)?).or_fail()?;
        Ok(run_id)
    }

    /// The `index`th launch, counting from zero.
    fn launch(&self, index: usize) -> Result<RecordedLaunch> {
        self.launcher.launches().get(index).cloned().or_fail()
    }
}

/// The `meta.json` and first `invocation.json` agentmux wrote for one consultation of `vendor`,
/// kept under `fixtures/records/` in a directory named after its CLI.
///
/// - Claude: a named account, the user's settings loaded, and every permission chosen.
/// - Codex: a named account, forced onto `codex exec`, read-only and offline.
/// - agy: the CLI's own login, isolated.
fn records(vendor: Vendor) -> Result<(Value, Value)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/records")
        .join(vendor.program());
    let read = |name: &str| -> Result<Value> {
        serde_json::from_str(&std::fs::read_to_string(dir.join(name)).or_fail()?).or_fail()
    };
    Ok((read("meta.json")?, read("invocation.json")?))
}

/// A path as a TOML basic string.
fn toml_string(path: &Path) -> Result<String> {
    Ok(toml::Value::String(path.to_str().or_fail()?.to_owned()).to_string())
}

/// A path as the inside of a JSON string, for a frozen record's `<cwd>`.
fn json_path(path: &Path) -> Result<String> {
    let quoted = serde_json::to_string(path.to_str().or_fail()?).or_fail()?;
    Ok(quoted.trim_matches('"').to_owned())
}

/// A delegate to start a consultation with, whose records [`Machine::frozen`] then replaces.
fn delegate(model: &str, permissions: Permissions) -> Result<Delegate> {
    Ok(Delegate {
        model: ModelId::parse(model).or_fail()?,
        effort: None,
        account: AccountChoice::Default,
        isolation: None,
        permissions,
    })
}

/// A frozen Claude record reads with the flat permission fields, the account and the isolation
/// it was written with, and a follow-up continues it.
#[gtest]
fn a_frozen_claude_record_keeps_every_permission() -> Result<()> {
    let machine = Machine::new([
        Script::completed(fixtures::CLAUDE_HAPPY),
        Script::completed(fixtures::CLAUDE_RESUME),
        Script::completed(fixtures::CLAUDE_HAPPY),
    ])?;
    let start = || delegate("opus", Permissions::Claude(ClaudePermissions::default()));
    let permissions = ClaudePermissions {
        permission_mode: Some(ClaudePermissionMode::AcceptEdits),
        tools: None,
        allowed_tools: Some(ClaudeTool::parse_all(&["WebFetch"]).or_fail()?),
        disallowed_tools: Some(ClaudeTool::parse_all(&["Bash(git push *)"]).or_fail()?),
    };

    let (mut meta, mut invocation) = records(Vendor::Claude)?;
    let run_id = machine.frozen(start()?, &meta, &invocation)?;
    assert_that!(
        machine.store.status(&run_id)?.delegate,
        eq(&Delegate {
            model: ModelId::parse("claude-haiku-5-5").or_fail()?,
            effort: Some(Effort::parse("max").or_fail()?),
            account: AccountChoice::Named(AccountAlias::parse("work").or_fail()?),
            isolation: Some(Isolation::Inherit),
            permissions: Permissions::Claude(permissions.clone()),
        })
    );
    // It continues as well as it reads: the follow-up resumes the session its turn opened.
    machine.store.follow_up(&run_id, "again")?;
    assert_that!(
        machine
            .launch(1)?
            .has_flag_with("--resume", "85ff2d6a-ec24-47cf-a4f8-486a5bb06814"),
        eq(true)
    );

    // Synthetic: agentmux writes an empty tool list as `tools: []`, which no captured record
    // offers, and a newer agentmux may add a key to a delegate that this build has to skip.
    for record in [&mut meta, &mut invocation] {
        record["delegate"]["tools"] = json!([]);
        record["delegate"]["written_by_a_newer_agentmux"] = json!(true);
    }
    let run_id = machine.frozen(start()?, &meta, &invocation)?;
    assert_that!(
        machine.store.status(&run_id)?.delegate.permissions,
        eq(&Permissions::Claude(ClaudePermissions {
            tools: Some(Vec::new()),
            ..permissions
        }))
    );
    Ok(())
}

/// A frozen Codex record continues its session, read-only and offline as it was written, and a
/// follow-up continues with the network its newest turn ran with, though the consultation's
/// record says otherwise, and keeps it switched off when it changes only the sandbox, rather
/// than leaving the network to Codex's own configuration.
#[gtest]
fn a_frozen_codex_record_continues_as_its_newest_turn_ran() -> Result<()> {
    let machine = Machine::new([
        Script::completed(fixtures::CODEX_HAPPY),
        Script::completed(fixtures::CODEX_HAPPY),
        Script::completed(fixtures::CODEX_HAPPY),
        Script::completed(fixtures::CODEX_HAPPY),
    ])?;
    let codex = || delegate("sol", Permissions::Codex(CodexPermissions::default()));

    let (mut meta, invocation) = records(Vendor::Codex)?;
    let run_id = machine.frozen(codex()?, &meta, &invocation)?;
    machine.store.follow_up(&run_id, "again")?;
    let resumed = machine.launch(1)?;
    assert_that!(
        resumed.has_flag_with("resume", "01a0784a-915b-7d92-a381-b53d765296c4"),
        eq(true)
    );
    assert_that!(
        resumed.has_flag_with("--config", "sandbox_mode=\"read-only\""),
        eq(true)
    );

    // Synthetic: a follow-up that switches the network off records that in its turn alone, and
    // none of the captured consultations has one, so the consultation's record is edited to the
    // network it would have started with.
    meta["delegate"]["network_access"] = json!(true);
    let run_id = machine.frozen(codex()?, &meta, &invocation)?;

    machine.store.follow_up_with(
        &run_id,
        "write it out",
        PermissionOptions {
            sandbox: Some(CodexSandbox::WorkspaceWrite),
            ..PermissionOptions::default()
        },
        None,
    )?;

    // The sandbox is the one asked for, and the network the one the newest turn ran with.
    let launch = machine.launch(3)?;
    assert_that!(
        launch.has_flag_with("--config", "sandbox_mode=\"workspace-write\""),
        eq(true)
    );
    assert_that!(
        launch.has_flag_with("--config", "sandbox_workspace_write.network_access=false"),
        eq(true)
    );
    Ok(())
}

/// A frozen agy record continues its conversation isolated, in the home the store keeps, and so
/// does one from before the account and the isolation were pinned, whose turn did not keep the
/// delegate it ran as: it reads as the consultation's record says with the CLI's own login, and
/// resumes isolated, as it could only have run.
#[gtest]
fn a_frozen_agy_record_continues_isolated() -> Result<()> {
    let machine = Machine::new([
        Script::completed(fixtures::AGY_TOOL_USE),
        Script::completed(fixtures::AGY_RESUME),
        Script::completed(fixtures::AGY_TOOL_USE),
        Script::completed(fixtures::AGY_RESUME),
    ])?;
    let agy = || delegate("gemini-3.8-flash-low", Permissions::Agy);
    let agy_home = machine.state.path().join("agy-home");

    let (mut meta, mut invocation) = records(Vendor::Agy)?;
    let run_id = machine.frozen(agy()?, &meta, &invocation)?;
    assert_that!(
        machine.store.status(&run_id)?.delegate,
        eq(&Delegate {
            model: ModelId::parse("gemini-3.8-flash-low").or_fail()?,
            effort: None,
            account: AccountChoice::CliLogin,
            isolation: Some(Isolation::Isolated),
            permissions: Permissions::Agy,
        })
    );
    machine.store.follow_up(&run_id, "again")?;
    let resumed = machine.launch(1)?;
    assert_that!(
        resumed.has_flag_with("--conversation", "e4c7ded7-2bf0-480b-9326-24191d51e4ba"),
        eq(true)
    );
    assert_that!(
        resumed.env.get("HOME").map(PathBuf::from),
        some(eq(&agy_home))
    );

    // Synthetic: an earlier agentmux wrote neither the account nor the isolation, nor the
    // delegate in a turn's invocation, and a newer one may add a key this build has to skip.
    let written = meta["delegate"].as_object_mut().or_fail()?;
    written.remove("account");
    written.remove("isolation");
    written.insert("written_by_a_newer_agentmux".to_owned(), json!("yes"));
    invocation.as_object_mut().or_fail()?.remove("delegate");
    let run_id = machine.frozen(agy()?, &meta, &invocation)?;

    assert_that!(
        machine.store.status(&run_id)?.delegate,
        eq(&Delegate {
            model: ModelId::parse("gemini-3.8-flash-low").or_fail()?,
            effort: None,
            account: AccountChoice::CliLogin,
            isolation: None,
            permissions: Permissions::Agy,
        })
    );

    machine.store.follow_up(&run_id, "again")?;
    assert_that!(
        machine.launch(3)?.env.get("HOME").map(PathBuf::from),
        some(eq(&agy_home))
    );
    Ok(())
}
