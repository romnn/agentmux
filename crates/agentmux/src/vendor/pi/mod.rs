//! The pi coding agent, `pi`: the read-only command it runs as, driven over `pi --mode rpc`, and
//! the variables that decide who it bills.
//!
//! Every pi turn is driven by agentmux (see [`driver`]), so a running turn takes a steer.
//! Its session is kept in the consultation's own directory, and its configuration is always the
//! user's: ADR 0037 records why.

pub mod driver;
pub mod fold;
pub(crate) mod quota;

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::accounting::{ReadSpend, Scope, Tokens};
use crate::delegate::{
    Delegate, DelegateError, DriverPlan, Invocation, Isolation, LaunchRequest, PreparedLaunch,
    Vendor,
};
use crate::run::LaunchMode;
use crate::transcript::Usage;
use driver::{PiPlan, PiSession};

/// pi's own arguments for one turn under `isolation`, before the run store adds where its session
/// is kept.
///
/// - `--mode rpc`: the driver sends the model, the effort and the question as commands, and steers
///   the running turn, so none of them is in the argument vector.
/// - `--no-extensions` keeps out every extension, pi's built-in ones included, so nothing can
///   register a tool, open a dialog or queue a message of its own.
/// - `--offline` stops the package installs and the model catalog refresh pi starts with.
/// - `--tools read,grep,find,ls`: a consultation reads, and runs and writes nothing.
///
/// # Errors
///
/// Returns [`DelegateError::PiIsolated`] for an isolated delegate: no flag of pi's skips its
/// `settings.json` or its system prompt files.
pub(crate) fn native_args(isolation: Isolation) -> Result<Vec<String>, DelegateError> {
    if !isolation.inherits() {
        return Err(DelegateError::PiIsolated);
    }
    Ok([
        "--mode",
        "rpc",
        "--no-extensions",
        "--offline",
        "--tools",
        "read,grep,find,ls",
    ]
    .map(str::to_owned)
    .into())
}

/// How a run store launches `invocation`, pi's own command for one turn of `delegate`: always
/// through the driver, which saves the session in `pi/session.jsonl` of the consultation's
/// directory.
///
/// # Errors
///
/// Returns [`DelegateError::PiUndriven`] for a store without a driver.
pub(crate) fn prepare(
    delegate: &Delegate,
    mut invocation: Invocation,
    request: &LaunchRequest<'_>,
) -> Result<PreparedLaunch, DelegateError> {
    let driver = request.driver.ok_or(DelegateError::PiUndriven)?;
    let session_dir = request.run_dir.join("pi");
    let file = session_dir.join("session.jsonl");
    let mut args = invocation.args().to_vec();
    for (flag, path) in [("--session-dir", &session_dir), ("--session", &file)] {
        args.push(flag.to_owned());
        args.push(path.to_string_lossy().into_owned());
    }
    let plan = PiPlan {
        program: Vendor::Pi.program().to_owned(),
        args,
        model: delegate.model.clone(),
        effort: delegate.effort.clone(),
        session: PiSession {
            file,
            expect_id: request.resume.cloned(),
        },
        cwd: request.cwd.to_path_buf(),
        billing: invocation.billing(),
        turn_dir: request.turn.root.clone(),
    };
    invocation.set_driver_command(driver, driver::SUBCOMMAND, &request.turn.pi_plan_path());
    Ok(PreparedLaunch {
        invocation,
        mode: LaunchMode::Driven,
        driver_plan: Some(DriverPlan::Pi(plan)),
    })
}

/// What a pi turn's report says it spent.
///
/// The fold adds up every usage pi reported during the turn, each once, so the tokens cover the
/// turn; they count cache reads and writes beside `input`.
/// pi's own price is an estimate from its model catalog, so none is kept.
pub(crate) fn read_spend(usage: &Usage) -> ReadSpend {
    ReadSpend {
        tokens: Tokens::cache_beside_input(usage),
        tokens_cover: Scope::Turn,
        price_covers: None,
    }
}

/// Credential variables pi reads: the directory that holds its login, `auth.json`, and its
/// configuration.
///
/// Every variable it otherwise authenticates with is a
/// [billing name](crate::delegate::is_billing_env_name).
pub(crate) const CREDENTIAL_KEYS: &[&str] = &["PI_CODING_AGENT_DIR"];

/// Whether `upper`, a name in upper case, puts pi on a provider's API, another endpoint or another
/// login, beyond what [`crate::delegate::is_billing_env_name`] catches for every vendor.
///
/// Read from pi 1.6.0's own `process.env` reads and from the credential and endpoint selectors of
/// the AWS SDK it bundles; the names are in ADR 0037.
pub(crate) fn is_billing_env_name(upper: &str) -> bool {
    [
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_OAUTH_TOKEN",
        "ANTHROPIC_IDENTITY_TOKEN_FILE",
        "ANTHROPIC_FEDERATION_RULE_ID",
        "ANTHROPIC_SERVICE_ACCOUNT_ID",
        "ANTHROPIC_ORGANIZATION_ID",
        "ANTHROPIC_WORKSPACE_ID",
        "COPILOT_GITHUB_TOKEN",
        "HF_TOKEN",
        "HF_TOKEN_PATH",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
        "AWS_PROFILE",
        "AWS_BEARER_TOKEN_BEDROCK",
        "AWS_BEDROCK_SKIP_AUTH",
        "AWS_CONFIG_FILE",
        "AWS_SHARED_CREDENTIALS_FILE",
        "AWS_ROLE_ARN",
        "AWS_ROLE_SESSION_NAME",
        "AWS_WEB_IDENTITY_TOKEN_FILE",
        "AWS_CONTAINER_CREDENTIALS_FULL_URI",
        "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
        "AWS_CONTAINER_AUTHORIZATION_TOKEN",
        "AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE",
        "GOOGLE_APPLICATION_CREDENTIALS",
        "GOOGLE_GENAI_USE_VERTEXAI",
        "GOOGLE_GENAI_USE_ENTERPRISE",
        "CLOUDSDK_CONFIG",
        "AZURE_OPENAI_BASE_URL",
        "AZURE_OPENAI_ENDPOINT",
        "AZURE_OPENAI_RESOURCE_NAME",
        "LLAMA_BASE_URL",
        "GOOGLE_GEMINI_BASE_URL",
        "GOOGLE_VERTEX_BASE_URL",
        "CLOUDFLARE_ACCOUNT_ID",
        "CLOUDFLARE_GATEWAY_ID",
        "KIMI_CODE_OAUTH_HOST",
        "KIMI_OAUTH_HOST",
        "PI_OAUTH_CALLBACK_HOST",
    ]
    .contains(&upper)
        || upper.starts_with("AWS_ENDPOINT_URL")
}

/// pi's configuration directory, as it resolves it from `env`: `PI_CODING_AGENT_DIR`, or
/// `~/.pi/agent` when that is unset or empty.
pub(crate) fn config_dir(env: &BTreeMap<String, String>) -> Option<PathBuf> {
    env.get("PI_CODING_AGENT_DIR")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| Some(crate::config::home_dir(env)?.join(".pi").join("agent")))
}
