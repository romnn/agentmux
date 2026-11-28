//! The lifecycle every vendor's consultation goes through, run for each vendor with recorded
//! captures through the scripted launcher and the run store.
//!
//! What only some vendors offer, such as a quota window or a steer, has its own tests; a vendor
//! without it passes here by design.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use agentmux::delegate::{Delegate, PermissionOptions, SessionRef, Vendor};
use agentmux::run::{Retention, RunStore, StartRequest};
use agentmux::testing::{Script, ScriptedLauncher, fixtures};
use agentmux::transcript::Outcome;
use agentmux::vendor::pi::driver::PiPlan;
use googletest::prelude::*;
use strum::VariantArray as _;

/// A vendor's consultation as this suite runs it: two recorded turns of one session, and how the
/// vendor's CLI is told to resume it.
#[derive(Debug, Clone, Copy)]
struct Conformance {
    /// A model the vendor's CLI takes.
    model: &'static str,
    /// A first turn that answered and finished.
    first: &'static str,
    /// A follow-up that answered in the session `first` opened.
    follow_up: &'static str,
    /// The session `first` opened.
    session: &'static str,
    /// The argument the session to resume follows on the CLI's command line, which for pi
    /// names the file the session is in.
    resume_arg: &'static str,
}

/// What this suite runs a `vendor` consultation with.
///
/// Exhaustive, so no vendor joins without passing the suite.
fn conformance(vendor: Vendor) -> Conformance {
    match vendor {
        // Claude's one recorded resume serves as its own first turn, so both turns report the
        // session the follow-up resumes.
        Vendor::Claude => Conformance {
            model: "claude-opus-5",
            first: fixtures::CLAUDE_RESUME,
            follow_up: fixtures::CLAUDE_RESUME,
            session: "850f440b-be0f-411e-aaf3-bc8fdcc6b6b8",
            resume_arg: "--resume",
        },
        Vendor::Codex => Conformance {
            model: "gpt-6-astra",
            first: fixtures::CODEX_HAPPY,
            follow_up: fixtures::CODEX_RESUME_WARNING,
            session: "01a0784a-915b-7d92-a381-b53d765296c4",
            resume_arg: "resume",
        },
        Vendor::Agy => Conformance {
            model: "gemini-3.8-flash-low",
            first: fixtures::AGY_TOOL_USE,
            follow_up: fixtures::AGY_RESUME,
            session: "e4c7ded7-2bf0-480b-9326-24191d51e4ba",
            resume_arg: "--conversation",
        },
        // The one recorded pi turn serves as its own follow-up, as Claude's does.
        Vendor::Pi => Conformance {
            model: "ollama/qwen3.6:27b-64k",
            first: fixtures::PI_TOOL_USE,
            follow_up: fixtures::PI_TOOL_USE,
            session: "01a222df-e34a-76d1-9e3f-380c59ae8f6d",
            resume_arg: "--session",
        },
    }
}

/// A store over a fresh directory, whose launcher plays `scripts`, with a driver for the vendors
/// that are always driven and Codex kept on `codex exec`.
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
    let store = RunStore::open(dir.path(), launcher.clone(), env)
        .or_fail()?
        .with_driver("/opt/agentmux")
        .with_codex_exec();
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
        conformance(vendor).model,
        None,
        None,
        PermissionOptions::default(),
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

/// A turn that answered settles as completed, and its follow-up continues the same consultation:
/// it resumes the session the first turn opened, as the vendor's own CLI is told to, is asked only
/// the new question, and adds a turn to a transcript that only grows.
#[gtest]
fn every_vendor_settles_a_turn_and_resumes_its_session() -> Result<()> {
    for vendor in Vendor::VARIANTS.iter().copied() {
        let case = conformance(vendor);
        let harness = harness([
            Script::completed(case.first),
            Script::completed(case.follow_up),
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
        // A consultation is a conversation: the run id stays, and the turns add up.
        expect_that!(followed.run_id, eq(&started.run_id), "{vendor}");
        expect_that!(followed.turns, eq(2), "{vendor}");

        // The follow-up resumes the delegate's own session rather than restating the brief, so
        // its input is the new question alone; agy reads it inside the record it takes.
        let launches = harness.launcher.launches();
        let resumed = launches.get(1).or_fail()?;
        let resumes = match vendor {
            // pi's command is in its driver's plan, which names the session's file and the
            // session the driver checks the file holds.
            Vendor::Pi => {
                let plan = PiPlan::read(Path::new(resumed.args.get(1).or_fail()?)).or_fail()?;
                let file = plan.session.file.to_string_lossy();
                plan.session.expect_id.as_ref().map(SessionRef::as_str) == Some(case.session)
                    && plan
                        .args
                        .windows(2)
                        .any(|pair| pair == [case.resume_arg, file.as_ref()])
            }
            Vendor::Claude | Vendor::Codex | Vendor::Agy => {
                resumed.has_flag_with(case.resume_arg, case.session)
            }
        };
        expect_that!(
            resumes,
            eq(true),
            "{vendor} did not resume {}",
            case.session
        );
        let asked = match vendor {
            Vendor::Agy => serde_json::from_str::<serde_json::Value>(&resumed.question)
                .or_fail()?["message"]["content"]
                .as_str()
                .or_fail()?
                .to_owned(),
            Vendor::Claude | Vendor::Codex | Vendor::Pi => resumed.question.clone(),
        };
        expect_that!(asked, eq("And the tests?"), "{vendor}");

        // What a reader saw before the follow-up is still the transcript's start.
        let after = harness.store.page(&started.run_id, 0, usize::MAX)?.text;
        expect_that!(after, starts_with(before.as_str()), "{vendor}");
        expect_that!(after, contains_substring("Turn 2 — question"), "{vendor}");
    }
    Ok(())
}

/// A turn cancelled while its delegate runs ends cancelled, its delegate stopped.
///
/// Paused time skips the wait a driven turn's driver is given to leave on its own.
#[gtest]
#[tokio::test(start_paused = true)]
async fn every_vendor_ends_a_cancelled_turn_cancelled() -> Result<()> {
    for vendor in Vendor::VARIANTS.iter().copied() {
        // The delegate has announced its session and is still at work.
        let opening = conformance(vendor).first.lines().next().or_fail()?;
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
        expect_that!(harness.launcher.terminated().len(), eq(1), "{vendor}");
        let transcript = harness.store.page(&started.run_id, 0, usize::MAX)?.text;
        expect_that!(transcript, contains_substring("cancelled"), "{vendor}");
    }
    Ok(())
}

/// A delegate that dies without a terminal event fails, and what it wrote to stderr is the
/// failure's detail and stays in the transcript as the evidence of why.
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
            matches_pattern!(Outcome::Failed {
                detail: contains_substring("the conformance suite pulled the plug"),
                ..
            }),
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
