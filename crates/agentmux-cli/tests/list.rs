//! `agentmux list` as a shell runs it, against a state directory the scripted launcher filled.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agentmux::delegate::{
    AccountChoice, CodexPermissions, CodexSandbox, Delegate, Effort, ModelId,
};
use agentmux::run::{Retention, RunStatus, RunStore, StartRequest};
use agentmux::testing::{Script, ScriptedLauncher, fixtures};
use googletest::prelude::*;

/// A state directory holding one completed Codex consultation, and its status.
fn one_consultation(state_dir: &Path) -> Result<RunStatus> {
    let store = RunStore::open(
        state_dir,
        Arc::new(ScriptedLauncher::new([Script::completed(
            fixtures::CODEX_HAPPY,
        )])),
        BTreeMap::new(),
    )
    .or_fail()?;
    let started = store
        .start(&StartRequest {
            delegate: Delegate::Codex {
                model: ModelId::parse("gpt-6-astra").or_fail()?,
                effort: Some(Effort::parse("high").or_fail()?),
                permissions: CodexPermissions {
                    sandbox: Some(CodexSandbox::ReadOnly),
                    network_access: None,
                },
                account: AccountChoice::Default,
                isolation: None,
            },
            question: "Review it.".to_owned(),
            cwd: PathBuf::from("/work/project"),
            retention: Retention::Ttl,
            env: BTreeMap::new(),
            billing: None,
            force_codex_exec: false,
            label: None,
        })
        .or_fail()?;
    // Read once in process, so the turn is settled before the command reads it.
    store.status(&started.run_id).or_fail()
}

/// Run `agentmux list` with `args` over `state_dir`.
fn run_list(state_dir: &Path, args: &[&str]) -> Result<std::process::Output> {
    std::process::Command::new(env!("CARGO_BIN_EXE_agentmux"))
        .arg("list")
        .args(args)
        .env("AGENTMUX_STATE_DIR", state_dir)
        .output()
        .or_fail()
}

/// `--since` lists the consultations created at or after it, each with its run total, and none
/// created before it.
#[gtest]
fn since_keeps_the_consultations_created_from_it_on() -> Result<()> {
    let root = tempfile::tempdir().or_fail()?;
    let state_dir = root.path().join("state");
    let status = one_consultation(&state_dir)?;
    let run_id = status.run_id.to_string();

    let from_creation = run_list(&state_dir, &["--since", &status.created_at.to_rfc3339()])?;
    let from_creation_text = String::from_utf8_lossy(&from_creation.stdout);
    assert_that!(from_creation.status.success(), eq(true));
    assert_that!(
        from_creation_text,
        all![
            contains_substring(run_id.as_str()),
            contains_substring("  14k in / 31 out\n")
        ]
    );

    let later = (status.created_at + chrono::Duration::hours(1)).to_rfc3339();
    let from_later = run_list(&state_dir, &["--since", &later])?;
    assert_that!(
        String::from_utf8_lossy(&from_later.stdout),
        all![
            not(contains_substring(run_id.as_str())),
            contains_substring("no consultations match --label, --state and --since")
        ]
    );
    Ok(())
}

/// A `--since` that is not an RFC 3339 instant is refused before anything is listed, naming the
/// format it expects.
#[gtest]
fn a_since_that_is_not_an_instant_is_refused() -> Result<()> {
    let root = tempfile::tempdir().or_fail()?;

    let refused = run_list(&root.path().join("state"), &["--since", "16:00"])?;

    assert_that!(refused.status.success(), eq(false));
    assert_that!(
        String::from_utf8_lossy(&refused.stderr),
        all![
            contains_substring("--since"),
            contains_substring("RFC 3339")
        ]
    );
    Ok(())
}
