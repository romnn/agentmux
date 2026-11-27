//! Google's Antigravity CLI, `agy`: its fixed permission policy, the home an isolated one runs
//! in, and the arguments and input it runs with.

pub mod fold;
pub mod quota;

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

use crate::accounting::{ReadSpend, Scope, Tokens};
use crate::delegate::{
    Delegate, DelegateError, Effort, Invocation, LaunchRequest, ModelId, PreparedLaunch, Stdin,
    TurnPlan, Vendor,
};
use crate::run::{LaunchMode, RunStore};
use crate::transcript::Usage;

/// The home every isolated agy delegate of a run store runs in, under the store's root, which
/// keeps their conversations for as long as the store is kept.
///
/// One for the store, because agy has one login, which its keyring holds.
/// The store owns it: nothing but agy writes there, and nothing else should.
const HOME_IN_STORE: &str = "agy-home";

/// Prepare an agy turn's launch, as `delegate`.
///
/// An isolated delegate runs in the home its store keeps for it, and every turn reads its
/// question as the one stream-json record [`agy_input`] writes.
///
/// # Errors
///
/// Returns [`DelegateError::Environment`] when the home an isolated delegate runs in cannot be
/// made, and [`DelegateError::AgySettingsUnreadable`] when agy's own settings file in the home
/// the delegate runs in cannot be read.
pub(crate) fn prepare(
    delegate: &Delegate,
    request: LaunchRequest<'_>,
) -> Result<PreparedLaunch, DelegateError> {
    let LaunchRequest {
        plan,
        question,
        cwd,
        mut env,
        billing,
        launcher,
        store_root,
        ..
    } = request;
    // Set over every other layer, so no account or request can move it; see ADR 0035.
    if !delegate.pinned_isolation().inherits() {
        use_agy_home(&mut env, &store_root.join(HOME_IN_STORE))?;
    }
    // Built from the complete environment, whose home is where agy reads its own settings.
    let args = agy_args(
        &delegate.model,
        delegate.effort.as_ref(),
        plan,
        &agy_user_rules(&env)?,
    );
    let mut invocation = Invocation::of_delegate(Vendor::Agy, args, env, billing);
    invocation.set_state_dir_env(RunStore::STATE_DIR_ENV, store_root);
    let cli_version = launcher.version(&invocation, cwd);
    Ok(PreparedLaunch {
        config_dir: Vendor::Agy.config_dir(invocation.env()),
        session_store: Vendor::Agy.session_store(invocation.env()),
        invocation,
        stdin: Stdin::Record(agy_input(question)),
        plan_file: None,
        mode: LaunchMode::DirectCli,
        cli_version,
    })
}

/// What an agy turn's report says it spent, whichever release wrote it.
///
/// Its tokens are its conversation's running total, which `--conversation` carries on from, and
/// count the cached prompt tokens beside its `input_tokens`.
/// It reports no price, so a price it did report would be in a scope nothing documents.
pub(crate) fn read_spend(usage: &Usage, _cli_version: Option<&str>) -> ReadSpend {
    ReadSpend {
        tokens: Tokens::cache_beside_input(usage),
        tokens_cover: Scope::Session,
        price_covers: None,
    }
}

/// Argument vector for the agy CLI.
///
/// - `--print=` with `--input-format stream-json` and `--output-format stream-json`: the question
///   arrives on stdin as the one record [`agy_input`] writes, so it never enters the argument
///   vector, and the events stream as they happen.
///   `--print` takes the prompt as its value, which stream-json input leaves empty.
/// - `--conversation` on a resume.
/// - `--model` on every turn, and `--effort` only when one was named, as for the other CLIs.
/// - `--settings`, always: the permission policy [`agy_settings`] builds.
/// - `--add-dir`, once for each directory the turn must read outside its working directory.
///
/// agy has no argument that skips the user's settings, hooks or MCP servers, so the run store
/// runs an isolated delegate with `HOME` set to a home it keeps for agy instead, which holds none
/// of them and no rules of the user's to carry over.
/// The project's own hooks, MCP servers and plugins load in either mode: agy finds them in any
/// directory it can read, and nothing turns them off.
/// ADR 0035 records why.
pub(crate) fn agy_args(
    model: &ModelId,
    effort: Option<&Effort>,
    plan: &TurnPlan<'_>,
    user_rules: &AgyPermissionRules,
) -> Vec<String> {
    let mut args: Vec<String> = [
        "--print=",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
    ]
    .map(str::to_owned)
    .into();
    if let Some(session) = plan.resume {
        args.push("--conversation".to_owned());
        args.push(session.as_str().to_owned());
    }
    args.push("--model".to_owned());
    args.push(model.as_str().to_owned());
    if let Some(effort) = effort {
        args.push("--effort".to_owned());
        args.push(effort.as_str().to_owned());
    }
    args.push("--settings".to_owned());
    args.push(agy_settings(user_rules));
    for dir in plan.read_dirs {
        args.push("--add-dir".to_owned());
        args.push(dir.to_string_lossy().into_owned());
    }
    args
}

/// What an agy delegate may never do, as rules of agy's own: run a command, write a file, call an
/// MCP tool, drive a browser or download a file.
pub(crate) const AGY_DENY_RULES: [&str; 5] = [
    "command(*)",
    "write_file(*)",
    "mcp(*)",
    "execute_url(*)",
    "download_url(*)",
];

/// The settings every agy launch overrides, as the one JSON object `--settings` takes.
///
/// - `agentMode: ""` selects agy's default execution mode, which headless agy otherwise replaces
///   with `auto` where its terminal sandbox is available; `--mode` has no value that names it.
/// - `toolPermission: "request-review"` sends every call that needs approval to a review, which
///   headless agy denies, whatever preset the user chose.
/// - `permissions.deny` names [`AGY_DENY_RULES`], which win over any rule allowing the same call,
///   in every mode.
/// - `useG1Credits: false` keeps a model whose quota is used up from going on at the price of the
///   account's AI credits, which a subscription consultation must not spend.
///
/// `--settings` replaces the user's whole `permissions` object rather than merging its lists, so
/// the user's own `deny` and `ask` rules are carried over beside these.
/// Their `allow` rules are not, since those only widen what a delegate may do.
/// Reading files and fetching a URL stay at agy's own defaults: a read is allowed, and a fetch
/// asks, which headless agy denies.
fn agy_settings(user_rules: &AgyPermissionRules) -> String {
    let deny: Vec<&str> = AGY_DENY_RULES
        .into_iter()
        .chain(user_rules.deny.iter().map(String::as_str))
        .collect();
    serde_json::json!({
        "agentMode": "",
        "toolPermission": "request-review",
        "permissions": {"deny": deny, "ask": user_rules.ask},
        "useG1Credits": false,
    })
    .to_string()
}

/// The part of agy's own `settings.json` that `--settings` would otherwise drop.
#[derive(Debug, Default, Deserialize)]
struct AgySettingsFile {
    #[serde(default)]
    permissions: AgyPermissionRules,
}

/// The user's own `deny` and `ask` rules, verbatim.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct AgyPermissionRules {
    #[serde(default)]
    deny: Vec<String>,
    #[serde(default)]
    ask: Vec<String>,
}

/// Read the user's own agy rules from the settings file agy reads in `env`, which holds none when
/// it is missing.
pub(crate) fn agy_user_rules(
    env: &BTreeMap<String, String>,
) -> Result<AgyPermissionRules, DelegateError> {
    let Some(path) = Vendor::Agy
        .config_dir(env)
        .map(|dir| dir.join("settings.json"))
    else {
        return Ok(AgyPermissionRules::default());
    };
    let unreadable = |reason: String| DelegateError::AgySettingsUnreadable {
        path: path.clone(),
        reason,
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(AgyPermissionRules::default());
        }
        Err(err) => return Err(unreadable(err.to_string())),
    };
    // agy reads a file saved with a byte-order mark, which JSON itself does not allow.
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    serde_json::from_str::<AgySettingsFile>(text)
        .map(|settings| settings.permissions)
        .map_err(|err| unreadable(err.to_string()))
}

/// Point `env` at `home`, the home an isolated agy delegate runs in, creating it on first use.
///
/// Both spellings are set: `HOME`, and `USERPROFILE`, which is where agy looks on Windows.
fn use_agy_home(env: &mut BTreeMap<String, String>, home: &Path) -> Result<(), DelegateError> {
    std::fs::create_dir_all(home).map_err(|err| {
        DelegateError::Environment(format!(
            "cannot create the home an isolated agy delegate runs in, {}: {err}",
            home.display()
        ))
    })?;
    let home = home.to_string_lossy().into_owned();
    for name in ["HOME", "USERPROFILE"] {
        env.insert(crate::config::canonical_env_name(name), home.clone());
    }
    Ok(())
}

/// What an agy turn reads on stdin: its question as the one stream-json record agy takes.
///
/// Its end of input then ends the session once the turn is over.
#[must_use]
pub(crate) fn agy_input(question: &str) -> String {
    let mut line =
        serde_json::json!({"event": "user", "message": {"content": question}}).to_string();
    line.push('\n');
    line
}
