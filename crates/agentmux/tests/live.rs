//! Consultations against the real CLIs.
//!
//! These are the only tests that spend money and need credentials, so they are `#[ignore]`d and
//! CI passes without them.
//! Run them by hand after upgrading either CLI:
//!
//! ```text
//! cargo nextest run -p agentmux --run-ignored all -E 'test(live)'
//! ```
//!
//! They exist because the fixtures under `fixtures/` are only as good as the day they were
//! recorded.
//! When one of these fails, re-record the fixtures from the same commands and let
//! `no_committed_fixture_contains_an_unnamed_event_type` tell you what changed.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agentmux::delegate::{CodexSandbox, Delegate, Effort, ModelId};
use agentmux::launch::ProcessLauncher;
use agentmux::run::{Retention, RunStore, StartRequest};
use agentmux::transcript::Outcome;
use googletest::prelude::*;

/// The cheapest model on each side that still exercises the whole path.
const CLAUDE_MODEL: &str = "haiku";
const CODEX_MODEL: &str = "gpt-5.3-codex-spark";

/// Start a consultation against the real CLI and wait for it, in a directory that survives the
/// test so a failure's capture files can be inspected afterwards.
async fn consult(
    delegate: Delegate,
    question: &str,
) -> Result<(RunStore, agentmux::run::RunStatus)> {
    let env: BTreeMap<String, String> = std::env::vars().collect();
    let dir = tempfile::tempdir().or_fail()?;
    let store = RunStore::open(dir.path(), Arc::new(ProcessLauncher::new()), env).or_fail()?;
    let started = store
        .start(&StartRequest {
            delegate,
            question: question.to_owned(),
            cwd: PathBuf::from(env!("CARGO_MANIFEST_DIR")),
            retention: Retention::UntilReleased,
            env: BTreeMap::new(),
        })
        .or_fail()?;
    store
        .wait_until_terminal(&started.run_id, Duration::from_secs(300))
        .await
        .or_fail()?;
    let status = store.status(&started.run_id).or_fail()?;
    let _kept = dir.keep();
    Ok((store, status))
}

/// The flags agentmux builds still work against today's Claude CLI, and nothing was dropped.
#[gtest]
#[tokio::test]
#[ignore = "spends money and needs a logged-in claude CLI"]
async fn a_live_claude_consultation_returns_its_answer() -> Result<()> {
    let (store, status) = consult(
        Delegate::Claude {
            model: ModelId::parse(CLAUDE_MODEL).or_fail()?,
            effort: Effort::parse("low").or_fail()?,
            account: None,
            isolation: None,
        },
        "Reply with exactly LIVE_CLAUDE_OK and nothing else.",
    )
    .await?;

    assert_that!(status.outcome, matches_pattern!(Outcome::Completed { .. }));
    assert_that!(
        status.unrecognised.summary(),
        none(),
        "the Claude event stream has drifted"
    );
    let page = store.page(&status.run_id, 0, usize::MAX).or_fail()?;
    assert_that!(page.text, contains_substring("LIVE_CLAUDE_OK"));
    Ok(())
}

/// The same for Codex, plus the resume path a follow-up depends on.
#[gtest]
#[tokio::test]
#[ignore = "spends money and needs a logged-in codex CLI"]
async fn a_live_codex_consultation_can_be_followed_up() -> Result<()> {
    let (store, status) = consult(
        Delegate::Codex {
            model: ModelId::parse(CODEX_MODEL).or_fail()?,
            effort: Effort::parse("low").or_fail()?,
            sandbox: CodexSandbox::ReadOnly,
            account: None,
            isolation: None,
        },
        "Remember the secret word ZEPPELIN. Reply with exactly LIVE_CODEX_OK and nothing else.",
    )
    .await?;

    assert_that!(status.outcome, matches_pattern!(Outcome::Completed { .. }));
    assert_that!(status.resumable, eq(true));
    assert_that!(
        status.unrecognised.summary(),
        none(),
        "the Codex event stream has drifted"
    );

    let followed = store
        .follow_up(
            &status.run_id,
            "What was the secret word? Reply with only the word.",
        )
        .or_fail()?;
    store
        .wait_until_terminal(&followed.run_id, Duration::from_secs(300))
        .await
        .or_fail()?;
    let followed = store.status(&followed.run_id).or_fail()?;
    assert_that!(followed.turns, eq(2));

    let page = store.page(&status.run_id, 0, usize::MAX).or_fail()?;
    assert_that!(page.text, contains_substring("ZEPPELIN"));
    Ok(())
}

/// A model the vendor does not have must reach the caller as the vendor's own words.
#[gtest]
#[tokio::test]
#[ignore = "needs a logged-in claude CLI"]
async fn a_live_unknown_model_fails_with_the_vendors_own_message() -> Result<()> {
    let (_store, status) = consult(
        Delegate::Claude {
            model: ModelId::parse("does-not-exist-9000").or_fail()?,
            effort: Effort::parse("low").or_fail()?,
            account: None,
            isolation: None,
        },
        "hello",
    )
    .await?;

    assert_that!(
        status.outcome,
        matches_pattern!(Outcome::Failed {
            kind: eq(&agentmux::transcript::FailureKind::ModelUnavailable),
            detail: contains_substring("does-not-exist-9000"),
        })
    );
    Ok(())
}
