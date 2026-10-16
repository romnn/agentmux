//! Which pricing a delegate runs on, observed from inside the delegate.
//!
//! Each test launches a fake `claude` through the real launcher; the fake appends the environment
//! it was given to a file, prints a recorded Claude stream so the consultation can be followed up,
//! and exits.
//! What the child process actually received is the only witness that counts.
//! Unix only: the fake is a shell script.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agentmux::delegate::{AccountAlias, Billing, Delegate, Effort, ModelId};
use agentmux::launch::ProcessLauncher;
use agentmux::run::{Retention, RunStatus, RunStore, StartRequest};
use agentmux::testing::fixtures;
use googletest::prelude::*;

/// Keys a launching agent's shell might well export, for both vendors.
const HOST_KEYS: &[(&str, &str)] = &[
    ("ANTHROPIC_API_KEY", "sk-ant-host"),
    ("ANTHROPIC_AUTH_TOKEN", "tok-host"),
    ("ANTHROPIC_BASE_URL", "https://gateway.example"),
    ("OPENAI_API_KEY", "sk-openai-host"),
    ("CODEX_API_KEY", "sk-codex-host"),
];

/// What separates one launch's environment from the next in the dump.
const LAUNCHED: &str = "==== launched";

struct World {
    root: tempfile::TempDir,
    host: BTreeMap<String, String>,
    dump: PathBuf,
    store: Option<RunStore>,
}

/// A fake `claude` on `PATH` that records its environment, a home, and a host environment that
/// carries [`HOST_KEYS`].
fn world() -> Result<World> {
    let root = tempfile::tempdir().or_fail()?;
    let bin = root.path().join("bin");
    let home = root.path().join("home");
    std::fs::create_dir_all(&bin).or_fail()?;
    std::fs::create_dir_all(&home).or_fail()?;
    let dump = root.path().join("claude-env.txt");
    let stream = root.path().join("stream.jsonl");
    std::fs::write(&stream, fixtures::CLAUDE_HAPPY).or_fail()?;
    let fake = bin.join("claude");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\nenv >> '{dump}'\necho '{LAUNCHED}' >> '{dump}'\ncat '{stream}'\nexit 0\n",
            dump = dump.display(),
            stream = stream.display(),
        ),
    )
    .or_fail()?;
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).or_fail()?;

    let mut host: BTreeMap<String, String> = [
        ("PATH", format!("{}:/usr/bin:/bin", bin.display())),
        ("HOME", home.display().to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect();
    host.extend(
        HOST_KEYS
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned())),
    );
    Ok(World {
        root,
        host,
        dump,
        store: None,
    })
}

impl World {
    /// Write a machine configuration and point agentmux at it, readable only by its owner.
    fn configure(&mut self, text: &str) -> Result<()> {
        let path = self.root.path().join("agentmux.toml");
        std::fs::write(&path, text).or_fail()?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).or_fail()?;
        self.host.insert(
            agentmux::config::CONFIG_PATH_ENV.to_owned(),
            path.display().to_string(),
        );
        Ok(())
    }

    /// The store, opened once with the host environment as it now stands.
    fn store(&mut self) -> Result<&RunStore> {
        if self.store.is_none() {
            self.store = Some(
                RunStore::open(
                    self.root.path().join("state"),
                    Arc::new(ProcessLauncher::new()),
                    self.host.clone(),
                )
                .or_fail()?,
            );
        }
        self.store.as_ref().or_fail()
    }

    /// The environment the fake recorded at its `launch`th launch, counting from zero.
    fn seen(&self, launch: usize) -> Result<String> {
        let dump = std::fs::read_to_string(&self.dump).or_fail()?;
        Ok(dump.split(LAUNCHED).nth(launch).or_fail()?.to_owned())
    }

    /// Run one consultation in the world's own directory and return its status and the
    /// environment the fake recorded.
    async fn consult(&mut self, account: Option<&str>) -> Result<(RunStatus, String)> {
        let cwd = self.root.path().to_path_buf();
        let status = self.consult_in(&cwd, account, None).await?;
        Ok((status, self.seen(0)?))
    }

    /// Run one consultation in `cwd`, asking for `billing`, to its end.
    async fn consult_in(
        &mut self,
        cwd: &std::path::Path,
        account: Option<&str>,
        billing: Option<Billing>,
    ) -> Result<RunStatus> {
        let store = self.store()?;
        let started = store.start(&StartRequest {
            delegate: Delegate::Claude {
                model: ModelId::parse("claude-opus-5").or_fail()?,
                effort: Some(Effort::parse("xhigh").or_fail()?),
                account: account
                    .map(AccountAlias::parse)
                    .transpose()
                    .or_fail()?
                    .into(),
                isolation: None,
            },
            question: "q".to_owned(),
            cwd: cwd.to_path_buf(),
            retention: Retention::Ttl,
            env: BTreeMap::new(),
            billing,
            codex_exec: false,
        })?;
        store
            .wait_until_terminal(&started.run_id, Duration::from_secs(20))
            .await?;
        Ok(store.status(&started.run_id)?)
    }
}

/// A delegate whose account did not opt in receives none of the keys the host exported.
#[gtest]
#[tokio::test]
async fn a_delegate_on_a_subscription_receives_no_key_from_the_host() -> Result<()> {
    let mut w = world()?;

    let (status, seen) = w.consult(None).await?;

    for (name, value) in HOST_KEYS {
        assert_that!(
            seen,
            not(contains_substring(*name)),
            "{name} reached the delegate"
        );
        assert_that!(seen, not(contains_substring(*value)));
    }
    // The status says which pricing the turn ran on.
    assert_that!(status.billing, some(eq(Billing::Subscription)));
    Ok(())
}

/// An account the machine file opts into API pricing hands its own key to the delegate, and the
/// transcript says the turn runs on API pricing.
#[gtest]
#[tokio::test]
async fn an_account_opted_into_api_pricing_passes_its_key_and_says_so() -> Result<()> {
    let mut w = world()?;
    w.host
        .insert("CI_ANTHROPIC_KEY".to_owned(), "sk-ci-opted-in".to_owned());
    w.configure(indoc::indoc! {r#"
        [accounts.claude.ci]
        api_key_env = "CI_ANTHROPIC_KEY"
        billing = "api"
    "#})?;

    let (status, seen) = w.consult(Some("ci")).await?;

    assert_that!(seen, contains_substring("ANTHROPIC_API_KEY=sk-ci-opted-in"));
    // The host's own key stays behind: the account chose the identity.
    assert_that!(seen, not(contains_substring("sk-ant-host")));
    assert_that!(status.billing, some(eq(Billing::Api)));
    let transcript = std::fs::read_to_string(&status.transcript_path).or_fail()?;
    assert_that!(
        transcript,
        contains_substring("This turn may be billed at API prices.")
    );
    // And so does every listing of it.
    let listed = w.store()?.list(10)?;
    let summary = listed.first().or_fail()?;
    assert_that!(summary.billing, some(eq(Billing::Api)));
    assert_that!(summary.delegate, ends_with("billing=api"));
    Ok(())
}

/// The same account without the opt-in is refused before anything is launched.
#[gtest]
#[tokio::test]
async fn an_account_with_a_key_but_no_opt_in_launches_nothing() -> Result<()> {
    let mut w = world()?;
    w.host
        .insert("CI_ANTHROPIC_KEY".to_owned(), "sk-ci".to_owned());
    w.configure(indoc::indoc! {r#"
        [accounts.claude.ci]
        api_key_env = "CI_ANTHROPIC_KEY"
    "#})?;

    let refused = w.consult(Some("ci")).await;

    assert_that!(
        refused.map(|_| ()),
        err(displays_as(contains_substring("billing = \"api\"")))
    );
    assert_that!(w.dump.exists(), eq(false));
    Ok(())
}

/// No configuration layer can hand a subscription delegate a key, whatever the variable is
/// called and however it is spelled, while what the layers set for other reasons arrives.
///
/// The names come from a table of this test's own, not from the rule the scrub applies, so a
/// rule that forgot one would fail here.
#[gtest]
#[tokio::test]
async fn no_layer_can_hand_a_subscription_delegate_a_key() -> Result<()> {
    let mut w = world()?;
    let profile = w.root.path().join("home/.claude-personal");
    std::fs::create_dir_all(&profile).or_fail()?;
    for (name, value) in [
        ("MY_PROVIDER_API_KEY", "sk-provider-passthrough"),
        ("openai_api_key_file", "/keys/openai-passthrough"),
        ("AWS_PROFILE", "dev-profile-kept"),
        (
            "CODEX_REFRESH_TOKEN_URL_OVERRIDE",
            "https://refresh-passthrough.example",
        ),
    ] {
        w.host.insert(name.to_owned(), value.to_owned());
    }
    w.configure(&format!(
        indoc::indoc! {r#"
            [launch]
            env = {{ CODEX_API_KEY = "sk-launch-env", Anthropic_Base_Url = "https://launch.example", CODEX_REVOKE_TOKEN_URL_OVERRIDE = "https://revoke-launch.example" }}
            env_passthrough = ["MY_PROVIDER_API_KEY", "openai_api_key_file", "AWS_PROFILE", "CODEX_REFRESH_TOKEN_URL_OVERRIDE"]

            [accounts.claude.personal]
            config_dir = "{profile}"
            env = {{ ANTHROPIC_AUTH_TOKEN = "tok-account-env", CLAUDE_CODE_USE_BEDROCK = "1", AZURE_OPENAI_BASE_URL = "https://azure.example", CODEX_APP_SERVER_LOGIN_ISSUER = "https://issuer-account.example" }}
        "#},
        profile = profile.display()
    ))?;

    let (status, seen) = w.consult(Some("personal")).await?;

    for leaked in [
        "sk-provider-passthrough",
        "/keys/openai-passthrough",
        "sk-launch-env",
        "launch.example",
        "tok-account-env",
        "CLAUDE_CODE_USE_BEDROCK",
        "azure.example",
        "refresh-passthrough.example",
        "revoke-launch.example",
        "issuer-account.example",
    ] {
        assert_that!(
            seen,
            not(contains_substring(leaked)),
            "{leaked} reached the delegate"
        );
    }
    assert_that!(seen, contains_substring("AWS_PROFILE=dev-profile-kept"));
    assert_that!(status.billing, some(eq(Billing::Subscription)));
    Ok(())
}

/// A consultation that starts on a subscription stays on one: an account a checkout's file chose
/// is not moved onto API pricing by a follow-up after that file is gone.
#[gtest]
#[tokio::test]
async fn a_follow_up_keeps_the_pricing_its_consultation_started_on() -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let mut w = world()?;
    let profile = w.root.path().join("home/.claude-work");
    std::fs::create_dir_all(&profile).or_fail()?;
    w.configure(&format!(
        indoc::indoc! {r#"
            [accounts.claude.work]
            config_dir = "{profile}"
            billing = "api"
            env = {{ ANTHROPIC_API_KEY = "sk-work-account" }}
        "#},
        profile = profile.display()
    ))?;
    // A checkout whose own file selects that account.
    let project = w.root.path().join("project");
    std::fs::create_dir_all(project.join(".git")).or_fail()?;
    let selection = project.join("agentmux.toml");
    std::fs::write(&selection, "[defaults.claude]\naccount = \"work\"\n").or_fail()?;
    std::fs::set_permissions(&selection, std::fs::Permissions::from_mode(0o644)).or_fail()?;

    let started = w.consult_in(&project, None, None).await?;
    assert_that!(started.billing, some(eq(Billing::Subscription)));
    assert_that!(w.seen(0)?, not(contains_substring("sk-work-account")));

    // With the checkout's choice gone, the pinned account alone would now be the machine's.
    std::fs::remove_file(&selection).or_fail()?;
    let store = w.store()?;
    store.follow_up(&started.run_id, "and then?")?;
    store
        .wait_until_terminal(&started.run_id, Duration::from_secs(20))
        .await?;

    assert_that!(w.seen(1)?, not(contains_substring("sk-work-account")));
    assert_that!(
        w.store()?.status(&started.run_id)?.billing,
        some(eq(Billing::Subscription))
    );
    Ok(())
}

/// Asking for API pricing where nothing opted in launches nothing.
#[gtest]
#[tokio::test]
async fn asking_for_api_pricing_without_an_opt_in_launches_nothing() -> Result<()> {
    let mut w = world()?;
    let cwd = w.root.path().to_path_buf();

    let refused = w.consult_in(&cwd, None, Some(Billing::Api)).await;

    assert_that!(
        refused.map(|_| ()),
        err(displays_as(contains_substring("asked for API pricing")))
    );
    assert_that!(w.dump.exists(), eq(false));
    Ok(())
}
