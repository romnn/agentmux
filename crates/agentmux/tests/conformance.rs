//! The lifecycle every vendor's consultation goes through, run for each vendor with recorded
//! captures through the scripted launcher and the run store.
//!
//! What only some vendors offer, such as a quota window or a steer, has its own tests; a vendor
//! without it passes here by design.

use std::collections::BTreeMap;
use std::path::PathBuf;

use agentmux::delegate::{Delegate, Overrides, Vendor};
use agentmux::run::{Retention, RunStore, StartRequest};
use agentmux::testing::{Script, ScriptedLauncher, fixtures};
use agentmux::transcript::Outcome;
use googletest::prelude::*;
use strum::VariantArray as _;

/// A store over a fresh directory, whose launcher plays `scripts`.
struct Harness {
    store: RunStore,
    launcher: std::sync::Arc<ScriptedLauncher>,
    _dir: tempfile::TempDir,
}

fn harness(scripts: impl IntoIterator<Item = Script>) -> Result<Harness> {
    let dir = tempfile::tempdir().or_fail()?;
    let launcher = std::sync::Arc::new(ScriptedLauncher::new(scripts));
    let env = BTreeMap::from([
        ("PATH".to_owned(), "/usr/bin".to_owned()),
        ("HOME".to_owned(), "/home/dev".to_owned()),
    ]);
    let store = RunStore::open(dir.path(), launcher.clone(), env).or_fail()?;
    Ok(Harness {
        store,
        launcher,
        _dir: dir,
    })
}

/// A new consultation of `vendor`, as a caller that names only the vendor and the model starts
/// one.
fn request(vendor: Vendor) -> Result<StartRequest> {
    let delegate = Delegate::from_parts(
        vendor,
        fixtures::conformance(vendor).model,
        None,
        None,
        Overrides::default(),
    )
    .or_fail()?;
    Ok(StartRequest {
        delegate,
        question: "Review the auth module.".to_owned(),
        cwd: PathBuf::from("/work/project"),
        retention: Retention::Ttl,
        env: BTreeMap::new(),
        billing: None,
        force_codex_exec: false,
        label: None,
    })
}

/// A turn that answered settles as completed, and its follow-up resumes the session it opened,
/// as the vendor's own CLI is told to, into a transcript that only grows.
#[gtest]
fn every_vendor_settles_a_turn_and_resumes_its_session() -> Result<()> {
    for vendor in Vendor::VARIANTS.iter().copied() {
        let conformance = fixtures::conformance(vendor);
        let harness = harness([
            Script::completed(conformance.first),
            Script::completed(conformance.follow_up),
        ])?;

        let started = harness.store.start(&request(vendor)?)?;
        expect_that!(
            started.outcome,
            matches_pattern!(Outcome::Completed { .. }),
            "{vendor}"
        );
        let before = harness.store.page(&started.run_id, 0, usize::MAX)?.text;

        let followed = harness.store.follow_up(&started.run_id, "And the tests?")?;
        expect_that!(
            followed.outcome,
            matches_pattern!(Outcome::Completed { .. }),
            "{vendor}"
        );
        let launches = harness.launcher.launches();
        expect_that!(
            launches
                .get(1)
                .map(|launch| launch.has_flag_with(conformance.resume_arg, conformance.session)),
            some(eq(true)),
            "{vendor} did not resume {}",
            conformance.session
        );

        // What a reader saw before the follow-up is still the transcript's start.
        let after = harness.store.page(&started.run_id, 0, usize::MAX)?.text;
        expect_that!(after, starts_with(before.as_str()), "{vendor}");
        expect_that!(after.len(), gt(before.len()), "{vendor}");
    }
    Ok(())
}

/// A turn cancelled while its delegate runs ends cancelled.
#[gtest]
#[tokio::test]
async fn every_vendor_ends_a_cancelled_turn_cancelled() -> Result<()> {
    for vendor in Vendor::VARIANTS.iter().copied() {
        // The delegate has announced its session and is still at work.
        let opening = fixtures::conformance(vendor)
            .first
            .lines()
            .next()
            .or_fail()?;
        let harness = harness([Script::running(format!("{opening}\n"))])?;

        let started = harness.store.start(&request(vendor)?)?;
        expect_that!(
            started.outcome,
            matches_pattern!(Outcome::Running),
            "{vendor}"
        );
        let cancelled = harness.store.cancel(&started.run_id).await?;
        expect_that!(
            cancelled.outcome,
            matches_pattern!(Outcome::Cancelled),
            "{vendor}"
        );
    }
    Ok(())
}

/// A delegate that dies without a terminal event fails, and what it wrote to stderr is kept as
/// the evidence of why.
#[gtest]
fn every_vendor_keeps_the_stderr_of_a_turn_that_died() -> Result<()> {
    for vendor in Vendor::VARIANTS.iter().copied() {
        let harness = harness([Script::exited(
            "",
            "fatal: the conformance suite pulled the plug\n",
            2,
        )])?;

        let started = harness.store.start(&request(vendor)?)?;
        expect_that!(
            started.outcome,
            matches_pattern!(Outcome::Failed { .. }),
            "{vendor}"
        );
        let transcript = harness.store.page(&started.run_id, 0, usize::MAX)?.text;
        expect_that!(
            transcript,
            contains_substring("the conformance suite pulled the plug"),
            "{vendor}"
        );
    }
    Ok(())
}
