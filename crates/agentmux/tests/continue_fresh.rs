//! Continuing a consultation whose session cannot go on in a fresh one, through the scripted
//! launcher.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use agentmux::delegate::{
    AccountChoice, ClaudePermissions, CodexPermissions, CodexSandbox, Delegate, Effort, ModelId,
};
use agentmux::run::{Retention, RunError, RunLabel, RunStore, StartRequest};
use agentmux::testing::{Script, ScriptedLauncher, fixtures};
use googletest::prelude::*;

/// A Codex turn the content filter refused after the delegate had said something.
const FLAGGED: &str = indoc::indoc! {r#"
    {"type":"thread.started","thread_id":"01a0784a-915b-7d92-a381-b53d765296c4"}
    {"type":"item.completed","item":{"id":"i0","type":"agent_message","text":"partial"}}
    {"type":"turn.failed","error":{"message":"This content was flagged for possible cybersecurity risk"}}
"#};

struct Harness {
    store: RunStore,
    launcher: Arc<ScriptedLauncher>,
    _dir: tempfile::TempDir,
}

fn harness(scripts: impl IntoIterator<Item = Script>) -> Result<Harness> {
    let dir = tempfile::tempdir().or_fail()?;
    let launcher = Arc::new(ScriptedLauncher::new(scripts));
    let env = [("PATH", "/usr/bin"), ("HOME", "/home/dev")]
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect();
    let store = RunStore::open(dir.path(), launcher.clone(), env).or_fail()?;
    Ok(Harness {
        store,
        launcher,
        _dir: dir,
    })
}

fn request(delegate: Delegate) -> Result<StartRequest> {
    Ok(StartRequest {
        delegate,
        question: "Rework the YAML parser's alias-expansion limits.".to_owned(),
        cwd: PathBuf::from("/work/project"),
        retention: Retention::Ttl,
        env: BTreeMap::new(),
        billing: None,
        force_codex_exec: false,
        label: Some(RunLabel::parse("BUILD yaml limits").or_fail()?),
    })
}

fn codex() -> Result<Delegate> {
    Ok(Delegate::Codex {
        model: ModelId::parse("gpt-6-astra").or_fail()?,
        effort: Some(Effort::parse("high").or_fail()?),
        permissions: CodexPermissions {
            sandbox: Some(CodexSandbox::WorkspaceWrite),
            network_access: None,
        },
        account: AccountChoice::Default,
        isolation: None,
    })
}

/// Two refused turns in a row point at a fresh session, which takes over everything the old
/// consultation ran with, briefs the delegate on where it left off, and links both ways.
#[gtest]
fn a_session_the_filter_keeps_refusing_continues_afresh() -> Result<()> {
    let harness = harness([
        Script::completed(FLAGGED),
        Script::completed(FLAGGED),
        Script::completed(fixtures::CODEX_HAPPY),
    ])?;
    let old = harness.store.start(&request(codex()?)?)?;
    // One refusal is the wording's, which a rephrased follow-up gets round.
    assert_that!(old.content_flagged_in_a_row, eq(1));
    assert_that!(old.suggests_continue_fresh(), eq(false));
    let refused = harness
        .store
        .follow_up(&old.run_id, "Carefully: only the limit check.")?;
    // A second in a row is the session's own context.
    assert_that!(refused.content_flagged_in_a_row, eq(2));
    assert_that!(refused.suggests_continue_fresh(), eq(true));

    let fresh = harness
        .store
        .continue_fresh(&old.run_id, Some("Finish the limit check."))?;
    assert_that!(fresh.run_id, not(eq(&old.run_id)));
    assert_that!(fresh.continues, some(eq(&old.run_id)));
    assert_that!(
        fresh.label.as_ref().map(RunLabel::as_str),
        some(eq("BUILD yaml limits"))
    );
    assert_that!(fresh.cwd, eq(&old.cwd));
    assert_that!(fresh.delegate.summary(), eq(&refused.delegate.summary()));

    // The brief names the old consultation, its transcript and the working tree, then asks.
    let launches = harness.launcher.launches();
    let brief = launches.last().or_fail()?;
    assert_that!(brief.has_flag("resume"), eq(false));
    assert_that!(
        brief.question,
        all![
            contains_substring(old.run_id.as_str()),
            contains_substring(old.transcript_path.to_string_lossy().as_ref()),
            contains_substring("uncommitted work"),
            contains_substring("a content filter refused its session"),
            ends_with("Finish the limit check.\n"),
        ]
    );
    let header = harness.store.page(&fresh.run_id, 0, 2_000)?.text;
    assert_that!(
        header,
        contains_substring(format!("- continues: {}\n", old.run_id).as_str())
    );

    // The old consultation points forward, and stops suggesting what has been done.
    let old_now = harness.store.status(&old.run_id)?;
    assert_that!(old_now.continued_by, some(eq(&fresh.run_id)));
    assert_that!(old_now.suggests_continue_fresh(), eq(false));
    Ok(())
}

/// With no question, the new session is told to carry on with what the old one was doing.
#[gtest]
fn a_fresh_session_with_no_question_carries_on() -> Result<()> {
    let harness = harness([
        Script::completed(FLAGGED),
        Script::completed(fixtures::CODEX_HAPPY),
    ])?;
    let old = harness.store.start(&request(codex()?)?)?;
    harness.store.continue_fresh(&old.run_id, None)?;
    let launches = harness.launcher.launches();
    let brief = launches.last().or_fail()?;
    assert_that!(
        brief.question,
        contains_substring("Carry on with the work that consultation was doing")
    );
    Ok(())
}

/// A Claude delegate reads only its working directory unless granted more, so it is granted the
/// old consultation's directory, where the transcript its brief names lives.
#[gtest]
fn a_claude_delegate_may_read_the_old_transcript() -> Result<()> {
    let harness = harness([
        Script::completed(fixtures::CLAUDE_HAPPY),
        Script::completed(fixtures::CLAUDE_HAPPY),
    ])?;
    let claude = Delegate::Claude {
        permissions: ClaudePermissions::default(),
        model: ModelId::parse("claude-opus-5").or_fail()?,
        effort: Some(Effort::parse("xhigh").or_fail()?),
        account: AccountChoice::Default,
        isolation: None,
    };
    let old = harness.store.start(&request(claude)?)?;
    harness.store.continue_fresh(&old.run_id, Some("go on"))?;

    let launches = harness.launcher.launches();
    let old_dir = old.transcript_path.parent().or_fail()?;
    assert_that!(launches.first().or_fail()?.has_flag("--add-dir"), eq(false));
    assert_that!(
        launches
            .last()
            .or_fail()?
            .has_flag_with("--add-dir", old_dir.to_string_lossy().as_ref()),
        eq(true)
    );
    Ok(())
}

/// A consultation still running has nothing settled to hand over, and nothing is launched.
#[gtest]
fn a_running_consultation_is_not_continued_afresh() -> Result<()> {
    let harness = harness([Script::running("")])?;
    let old = harness.store.start(&request(codex()?)?)?;
    assert_that!(
        harness.store.continue_fresh(&old.run_id, None),
        err(matches_pattern!(RunError::StillRunning { .. }))
    );
    assert_that!(harness.launcher.launches().len(), eq(1));
    Ok(())
}
