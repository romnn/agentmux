//! An agentmux process that outlives an upgrade of its own executable, through the scripted
//! launcher.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use agentmux::delegate::{
    AccountChoice, CodexPermissions, CodexSandbox, Delegate, Effort, ModelId,
};
use agentmux::run::{ListFilter, Retention, RunError, RunStore, StartRequest};
use agentmux::testing::{Script, ScriptedLauncher};
use googletest::prelude::*;

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
    let env = [("PATH", "/usr/bin"), ("HOME", "/home/dev")]
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect();
    let store = RunStore::open(dir.path().join("state"), launcher.clone(), env)
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
