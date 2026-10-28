//! An agentmux process that outlives an upgrade of its own executable, through the scripted
//! launcher.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agentmux::delegate::{
    AccountChoice, CodexPermissions, CodexSandbox, Delegate, Effort, Invocation, ModelId,
};
use agentmux::launch::{ExitStatus, LaunchError, LaunchSpec, Launched, Launcher, Liveness};
use agentmux::run::{ListFilter, Retention, RunError, RunStore, StartRequest};
use agentmux::testing::{Script, ScriptedLauncher};
use googletest::prelude::*;

fn env() -> BTreeMap<String, String> {
    [("PATH", "/usr/bin"), ("HOME", "/home/dev")]
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect()
}

fn request() -> Result<StartRequest> {
    Ok(StartRequest {
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
        question: "review this".to_owned(),
        cwd: PathBuf::from("/work/project"),
        retention: Retention::Ttl,
        env: BTreeMap::new(),
        billing: None,
        force_codex_exec: false,
        label: None,
    })
}

/// Once the executable a store supervises its turns with is replaced, as an upgrade replaces it
/// under a running MCP server, a new consultation is refused at once rather than launched into a
/// supervisor of another build that fails it later.
#[gtest]
fn a_replaced_executable_launches_nothing() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let executable = dir.path().join("agentmux");
    std::fs::write(&executable, "agentmux 0.0.9").or_fail()?;
    let launcher = Arc::new(ScriptedLauncher::new([Script::running("")]));
    let store = RunStore::open(dir.path().join("state"), launcher.clone(), env())
        .or_fail()?
        .with_supervisor(&executable)
        .with_codex_app_server(&executable);
    store.start(&request()?)?;
    assert_that!(launcher.launches().len(), eq(1));

    std::fs::write(&executable, "agentmux 0.0.10, upgraded").or_fail()?;
    assert_that!(
        store.start(&request()?),
        err(matches_pattern!(RunError::ExecutableReplaced {
            path: eq(&executable)
        }))
    );
    // Nothing was launched, and no consultation was left behind to report as running.
    assert_that!(launcher.launches().len(), eq(1));
    assert_that!(store.list(10, &ListFilter::default())?.len(), eq(1));
    Ok(())
}

/// A launcher that plays the scripted one, except that the upgrade lands while it is asked the
/// delegate CLI's version, which can take seconds.
struct UpgradedDuringProbe {
    scripted: ScriptedLauncher,
    executable: PathBuf,
}

impl Launcher for UpgradedDuringProbe {
    fn launch(&self, spec: &LaunchSpec<'_>) -> std::result::Result<Launched, LaunchError> {
        self.scripted.launch(spec)
    }

    fn liveness(&self, launched: Launched) -> Liveness {
        self.scripted.liveness(launched)
    }

    fn terminate(&self, launched: Launched) {
        self.scripted.terminate(launched);
    }

    fn kill(&self, launched: Launched) {
        self.scripted.kill(launched);
    }

    fn reap(&self, launched: Launched) -> Option<ExitStatus> {
        self.scripted.reap(launched)
    }

    fn version(&self, invocation: &Invocation, cwd: &Path) -> Option<String> {
        std::fs::write(&self.executable, "agentmux 0.0.10, upgraded").ok()?;
        self.scripted.version(invocation, cwd)
    }
}

/// An upgrade that lands while a launch is being prepared is caught just before the spawn, and
/// the consultation it was for is not left behind.
#[gtest]
fn an_upgrade_during_the_launch_launches_nothing() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let executable = dir.path().join("agentmux");
    std::fs::write(&executable, "agentmux 0.0.9").or_fail()?;
    let launcher = Arc::new(UpgradedDuringProbe {
        scripted: ScriptedLauncher::new([Script::running("")]),
        executable: executable.clone(),
    });
    let store = RunStore::open(dir.path().join("state"), launcher.clone(), env())
        .or_fail()?
        .with_supervisor(&executable);
    assert_that!(
        store.start(&request()?),
        err(matches_pattern!(RunError::ExecutableReplaced {
            path: eq(&executable)
        }))
    );
    assert_that!(launcher.scripted.launches().len(), eq(0));
    assert_that!(store.list(10, &ListFilter::default())?.len(), eq(0));
    Ok(())
}
