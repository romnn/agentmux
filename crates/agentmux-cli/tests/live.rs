//! A steered consultation against the real `codex app-server`.
//!
//! The only test of this crate that spends a turn and needs a logged-in `codex`, so it is
//! `#[ignore]`d and CI passes without it.
//! Run it by hand after upgrading codex, outside any sandbox, because the delegate saves its
//! session where the sandbox would not let it:
//!
//! ```text
//! AGENTMUX_LIVE_CODEX_ACCOUNT=personal \
//!   cargo nextest run -p agentmux-cli --run-ignored all -E 'test(live)' --no-capture
//! ```
//!
//! `AGENTMUX_LIVE_CODEX_ACCOUNT` names the account from the machine's `agentmux.toml`; without it
//! the file's default, or the CLI's own login, is used.
//! The driver is the `agentmux` built with this test, never an installed one.
//! The store is kept, and its path printed, so a failure's capture can be read afterwards.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agentmux::delegate::{AccountAlias, CodexSandbox, Delegate, Effort, Isolation, ModelId};
use agentmux::launch::ProcessLauncher;
use agentmux::run::{Retention, RunStore, StartRequest};
use agentmux::transcript::Outcome;
use googletest::prelude::*;

/// The cheapest model in codex-cli 0.159.3's own list that still runs a command and follows an
/// instruction.
const MODEL: &str = "gpt-6-luna";

/// Start, steer, finish, follow up, and cancel one consultation over the real app-server.
#[gtest]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "spends a Codex turn and needs a logged-in codex CLI"]
async fn a_live_codex_turn_is_steered_followed_up_and_cancelled() -> Result<()> {
    let env: BTreeMap<String, String> = std::env::vars().collect();
    let root = tempfile::tempdir().or_fail()?.keep();
    let project = root.join("project");
    std::fs::create_dir_all(&project).or_fail()?;
    let store = RunStore::open(root.join("state"), Arc::new(ProcessLauncher::new()), env)
        .or_fail()?
        .with_codex_app_server(env!("CARGO_BIN_EXE_agentmux"));
    let account = std::env::var("AGENTMUX_LIVE_CODEX_ACCOUNT")
        .ok()
        .map(|alias| AccountAlias::parse(&alias))
        .transpose()
        .or_fail()?;

    let started = store.start(&StartRequest {
        delegate: Delegate::Codex {
            model: ModelId::parse(MODEL).or_fail()?,
            effort: Some(Effort::parse("low").or_fail()?),
            sandbox: CodexSandbox::ReadOnly,
            account,
            isolation: Some(Isolation::Inherit),
        },
        question: "Run the shell command `sleep 20` and wait for it to finish. Then reply with \
                   one line: LIVE_DONE, followed by anything else you have been asked to add."
            .to_owned(),
        cwd: project,
        retention: Retention::UntilReleased,
        env: BTreeMap::new(),
        billing: None,
        codex_exec: false,
    })?;
    let id = started.run_id.clone();
    eprintln!(
        "live run {id}, transcript {}",
        started.transcript_path.display()
    );
    // The turn takes a steer once the driver has announced it.
    let deadline = Instant::now() + Duration::from_secs(60);
    while !store.status(&id)?.steerable {
        assert_that!(
            Instant::now() < deadline,
            eq(true),
            "the turn never started"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Steered while the command runs; it reaches the model before its next call.
    store
        .steer(&id, "Also put the word STEERED at the end of that line.")
        .await?;
    store
        .wait_until_terminal(&id, Duration::from_secs(300))
        .await?;
    let (status, page) = store.view(&id, 0, usize::MAX)?;
    assert_that!(status.outcome, matches_pattern!(Outcome::Completed { .. }));
    assert_that!(page.text, contains_substring("### user (steer)"));
    // The closing reply comes after the steer, and so after everything that asked for it.
    let (_, reply) = page.text.rsplit_once("### assistant").or_fail()?;
    assert_that!(reply, not(contains_substring("### user")));
    assert_that!(reply, contains_substring("LIVE_DONE"));
    assert_that!(reply, contains_substring("STEERED"));

    // A follow-up resumes the same thread; its replies are read from below its question.
    store.follow_up(&id, "Reply with exactly LIVE_AGAIN and nothing else.")?;
    store
        .wait_until_terminal(&id, Duration::from_secs(300))
        .await?;
    let (followed, page) = store.view(&id, status.transcript_bytes, usize::MAX)?;
    assert_that!(
        followed.outcome,
        matches_pattern!(Outcome::Completed { .. })
    );
    assert_that!(followed.broke_continuity, eq(false));
    let (_, replies) = page.text.split_once("### assistant").or_fail()?;
    assert_that!(replies, contains_substring("LIVE_AGAIN"));

    // A long turn is cancelled over the protocol once it is running, and the store records it.
    store.follow_up(
        &id,
        "Run the shell command `sleep 120`, then reply with exactly LIVE_LATE.",
    )?;
    let deadline = Instant::now() + Duration::from_secs(60);
    while !store.status(&id)?.steerable {
        assert_that!(
            Instant::now() < deadline,
            eq(true),
            "the turn never started"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // Time for the model to start the command, which the transcript does not show running.
    tokio::time::sleep(Duration::from_secs(10)).await;
    let asked = Instant::now();
    let cancelled = store.cancel(&id).await?;
    eprintln!("cancel took {:?}", asked.elapsed());
    assert_that!(cancelled.outcome, eq(&Outcome::Cancelled));
    let page = store.page(&id, followed.transcript_bytes, usize::MAX)?;
    assert_that!(page.text, contains_substring("_Turn 3 cancelled."));
    // The interrupted turn never got to its answer.
    let replies = page
        .text
        .split_once("### assistant")
        .map_or("", |(_, replies)| replies);
    assert_that!(replies, not(contains_substring("LIVE_LATE")));
    // Nothing codex said in any of the three turns went unread.
    assert_that!(
        cancelled.unrecognised.summary(),
        none(),
        "the app-server protocol has drifted"
    );
    Ok(())
}
