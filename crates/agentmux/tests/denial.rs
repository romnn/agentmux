//! What a store told to deny a vendor refuses, and what it deliberately leaves alone.
//!
//! The rule the flag encodes: a harness that is itself the delegate's vendor spawns that subagent
//! natively and supervises it in-process, so agentmux registered inside that harness refuses to be
//! a slower, blinder copy of something it already has.
//! The refusal is worth testing rather than reading, because it has to hold on every path that
//! spawns a child — a follow-up to a consultation a terminal started included — and because a
//! refusal that leaves a run directory behind is worse than no refusal at all: `list` would report
//! a consultation nobody can collect.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use agentmux::delegate::{CodexSandbox, Delegate, Effort, ModelId, Vendor};
use agentmux::run::{Retention, RunError, RunStore, StartRequest};
use agentmux::testing::{Script, ScriptedLauncher, fixtures};
use googletest::prelude::*;

fn env() -> BTreeMap<String, String> {
    [("PATH", "/usr/bin"), ("HOME", "/home/dev")]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

fn claude() -> Result<Delegate> {
    Ok(Delegate::Claude {
        model: ModelId::parse("claude-opus-5").or_fail()?,
        effort: Some(Effort::parse("xhigh").or_fail()?),
        account: None,
        isolation: None,
    })
}

fn codex() -> Result<Delegate> {
    Ok(Delegate::Codex {
        model: ModelId::parse("gpt-6-astra").or_fail()?,
        effort: Some(Effort::parse("high").or_fail()?),
        sandbox: CodexSandbox::ReadOnly,
        account: None,
        isolation: None,
    })
}

fn request(delegate: Delegate, question: &str) -> StartRequest {
    StartRequest {
        delegate,
        question: question.to_owned(),
        cwd: PathBuf::from("/work/project"),
        retention: Retention::Ttl,
        env: BTreeMap::new(),
    }
}

/// A denied vendor is refused, and the refusal costs nothing and leaves nothing.
#[gtest]
fn a_denied_vendor_is_never_spawned() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let launcher = Arc::new(ScriptedLauncher::new([Script::completed(
        fixtures::CLAUDE_HAPPY,
    )]));
    let store = RunStore::open(dir.path(), launcher.clone(), env())
        .or_fail()?
        .with_denied_vendors([Vendor::Claude]);

    let refused = store.start(&request(claude()?, "review this"));

    assert_that!(
        refused.map(|_| ()),
        err(matches_pattern!(RunError::VendorDenied {
            vendor: eq(&Vendor::Claude)
        }))
    );
    // No child, and no half-made consultation for `list` to report as running: a caller that
    // retries elsewhere must not leave a trail of runs nobody will ever collect.
    assert_that!(launcher.launches(), is_empty());
    assert_that!(store.list(20).or_fail()?, is_empty());
    Ok(())
}

/// Denying one vendor is not denying delegation: the cross-vendor direction is the point.
#[gtest]
fn the_vendor_that_was_not_denied_still_runs() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let launcher = Arc::new(ScriptedLauncher::new([Script::completed(
        fixtures::CODEX_HAPPY,
    )]));
    let store = RunStore::open(dir.path(), launcher.clone(), env())
        .or_fail()?
        .with_denied_vendors([Vendor::Claude]);

    let started = store.start(&request(codex()?, "review this"))?;

    assert_that!(started.turns, eq(1));
    assert_that!(launcher.launches().len(), eq(1));
    Ok(())
}

/// A consultation that already exists is not a way around the denial.
///
/// The run here was started from a terminal, which denies nothing, and the follow-up arrives at a
/// server that does — the same store root, a different process.
/// A follow-up spawns the delegate again, so it is refused the same way, before any turn is
/// claimed.
#[gtest]
fn a_follow_up_cannot_reach_a_denied_vendor() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let launcher = Arc::new(ScriptedLauncher::new([
        Script::completed(fixtures::CODEX_HAPPY),
        Script::completed(fixtures::CODEX_HAPPY),
    ]));
    let terminal = RunStore::open(dir.path(), launcher.clone(), env()).or_fail()?;
    let started = terminal.start(&request(codex()?, "review this"))?;
    assert_that!(started.resumable, eq(true));

    let server = RunStore::open(dir.path(), launcher.clone(), env())
        .or_fail()?
        .with_denied_vendors([Vendor::Codex]);
    let refused = server.follow_up(&started.run_id, "and now?");

    assert_that!(
        refused.map(|_| ()),
        err(matches_pattern!(RunError::VendorDenied {
            vendor: eq(&Vendor::Codex)
        }))
    );
    assert_that!(launcher.launches().len(), eq(1));
    // The consultation is untouched: still one turn, and still resumable from a store that would
    // run it.
    assert_that!(terminal.status(&started.run_id).or_fail()?.turns, eq(1));
    assert_that!(
        terminal
            .follow_up(&started.run_id, "and now?")
            .map(|status| status.turns),
        ok(eq(&2))
    );
    Ok(())
}

/// Denying nothing is the default, and what a store was told to deny is readable back.
#[gtest]
fn a_store_denies_nothing_until_it_is_told_to() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let store = RunStore::open(dir.path(), Arc::new(ScriptedLauncher::new([])), env()).or_fail()?;

    assert_that!(store.denied_vendors(), is_empty());
    assert_that!(
        store.with_denied_vendors(Vendor::ALL).denied_vendors(),
        eq(&BTreeSet::from(Vendor::ALL))
    );
    Ok(())
}
