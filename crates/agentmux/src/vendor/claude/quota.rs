//! What a Claude account has left: the usage figures its CLI caches, and how they read.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::home_dir;
use crate::delegate::{CLAUDE_CREDENTIAL_VARS, Vendor, is_billing_env_name};
use crate::quota::{
    Observation, Origin, PROBE_TIMEOUT, ProbeRequest, ProbeTarget, Refresh, probe_command,
};

/// The window a Claude identity's environment, `env`, lets a probe read.
///
/// Claude is left to its environment: it keeps no stored login that outranks it, as Codex does,
/// and nothing measured says its precedence differs; a vendor that says nothing about itself is
/// not guessed at.
/// So any name that can move it onto API pricing or another backend, such as
/// `CLAUDE_CODE_USE_BEDROCK`, means the plain profile's window is not this account's, and its key,
/// its token and its endpoint are such names too.
/// A subscription identity has had every such name scrubbed already.
pub(crate) fn probe_target(env: &BTreeMap<String, String>) -> ProbeTarget {
    if env.keys().any(|name| is_billing_env_name(name)) {
        return ProbeTarget::NoSubscriptionWindow;
    }
    ProbeTarget::Window {
        config_dir: env
            .get(CLAUDE_CREDENTIAL_VARS.config_dir)
            .map(PathBuf::from),
    }
}

/// How old Claude's cached figures may be before the CLI is asked to refresh them.
///
/// The vendor writes that cache at most once every five minutes, so refreshing more eagerly than
/// this cannot produce newer numbers and only costs a process launch.
const MAX_CACHE_AGE: Duration = Duration::from_secs(300);

/// Read Claude's own usage cache.
///
/// No process is spawned: the CLI writes these figures to `.claude.json` inside whichever
/// configuration directory it authenticated from, and reading them costs nothing and cannot fail
/// in a way that spends money.
/// The trade is staleness, which the vendor's own `fetchedAtMs` reports rather than agentmux
/// guessing.
pub(crate) fn probe(request: &ProbeRequest<'_>) -> Observation {
    let home = home_dir(request.host_env);
    // Without `CLAUDE_CONFIG_DIR` the CLI keeps this file at the root of the home directory, not
    // under `~/.claude`.
    let path = match request.target.config_dir() {
        Some(dir) => dir.join(".claude.json"),
        None => match &home {
            Some(home) => home.join(".claude.json"),
            None => {
                return Observation::Unavailable {
                    reason: "neither an account config_dir nor HOME is known".to_owned(),
                };
            }
        },
    };

    // Refreshing first, only when the figures are old enough for it to change anything.
    // A weekly window moves with every other session on the account, so figures from hours ago
    // answer a question nobody asked.
    let before = read_cached_usage(&path);
    let refresh_process_outcome = is_stale(before.as_ref()).then(|| refresh_claude_cache(request));

    let Some(cached) = read_cached_usage(&path) else {
        return Observation::Unavailable {
            reason: format!(
                "{} holds no usage figures. That account may not be logged in on this machine.",
                path.display()
            ),
        };
    };

    // The figures decide whether the refresh happened, not the process.
    // The CLI writes the file before it exits, so an exit status or a deadline says only how the
    // process ended, and calling figures it demonstrably rewrote stale would be the wrong answer
    // dressed as a careful one.
    let fetched = fetched_at_ms(Some(&cached));
    let advanced = fetched > fetched_at_ms(before.as_ref());
    let refresh = match refresh_process_outcome {
        None => Refresh::Skipped,
        Some(_) if advanced => Refresh::Succeeded,
        Some(RefreshProcessOutcome::TimedOut) => Refresh::TimedOut,
        Some(RefreshProcessOutcome::Exited(status)) if status.success() => Refresh::Failed {
            reason: format!(
                "`{} --print /usage` exited cleanly, but the cached figures did not advance",
                Vendor::Claude.program()
            ),
        },
        Some(RefreshProcessOutcome::Exited(status)) => Refresh::Failed {
            reason: format!(
                "`{} --print /usage` exited with {status}",
                Vendor::Claude.program()
            ),
        },
        Some(RefreshProcessOutcome::ProcessError(reason)) => Refresh::Failed { reason },
    };

    Observation::Reported {
        origin: Origin::Cache {
            fetched_at_ms: fetched,
            path,
            refresh,
        },
        payload: cached,
    }
}

/// Read the usage figures the Claude CLI caches inside a configuration directory.
fn read_cached_usage(path: &Path) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(path).ok()?;
    let document = serde_json::from_str::<serde_json::Value>(&text).ok()?;
    document.get("cachedUsageUtilization").cloned()
}

/// Whether a set of cached figures is old enough to be worth refreshing.
fn is_stale(cached: Option<&serde_json::Value>) -> bool {
    let Some(fetched_at_ms) = fetched_at_ms(cached) else {
        return true;
    };
    let Some(fetched_at) = chrono::DateTime::from_timestamp_millis(fetched_at_ms) else {
        return true;
    };
    chrono::Utc::now()
        .signed_duration_since(fetched_at)
        .to_std()
        .is_ok_and(|age| age > MAX_CACHE_AGE)
}

/// The vendor's own timestamp on a set of cached figures, in milliseconds since the epoch.
fn fetched_at_ms(cached: Option<&serde_json::Value>) -> Option<i64> {
    cached
        .and_then(|c| c.get("fetchedAtMs"))
        .and_then(serde_json::Value::as_i64)
}

/// How a cache refresh's process ended, which says nothing about whether the figures advanced.
enum RefreshProcessOutcome {
    /// It exited with this status.
    Exited(std::process::ExitStatus),
    /// It was still running at the deadline and was stopped.
    TimedOut,
    /// It could not be started or waited on, for this reason.
    ProcessError(String),
}

/// Ask the Claude CLI to refresh its usage cache.
///
/// `/usage` is answered locally: the run reports `total_cost_usd: 0` and no API duration, so this
/// spends nothing.
/// Its output is discarded because the structured figures land in the cache, and the printed text
/// omits scoped limits that the cache keeps.
///
/// What comes back is how the process ended.
/// Whether the cache actually advanced is judged by `probe_claude` from the figures themselves.
fn refresh_claude_cache(request: &ProbeRequest<'_>) -> RefreshProcessOutcome {
    use std::process::Stdio;

    let mut command = probe_command(request, Vendor::Claude.program());
    command.args(["--print", "/usage", "--output-format", "text"]);
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(error) => {
            return RefreshProcessOutcome::ProcessError(format!(
                "`{} --print /usage` could not be run: {error}",
                Vendor::Claude.program()
            ));
        }
    };
    // The cached figures are read whatever happens here, so the outcome is reported rather than
    // acted on.
    // It is the difference between numbers that are current and numbers that only look current.
    let deadline = std::time::Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return RefreshProcessOutcome::Exited(status),
            Err(error) => {
                return RefreshProcessOutcome::ProcessError(format!(
                    "the refresh could not be waited on: {error}"
                ));
            }
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return RefreshProcessOutcome::TimedOut;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// Claude reports a `limits` array, each entry a percentage with its own severity.
pub(crate) fn windows(payload: &serde_json::Value) -> Option<Vec<String>> {
    let limits = payload.get("utilization")?.get("limits")?.as_array()?;
    Some(
        limits
            .iter()
            .map(|limit| {
                let kind = limit
                    .get("kind")
                    .and_then(|k| k.as_str())
                    .unwrap_or("window");
                // The scope names a model, and only by display name: the vendor leaves `id` null,
                // which is why nothing here tries to match it to the model a caller asked for.
                let scope = limit
                    .get("scope")
                    .and_then(|s| s.get("model"))
                    .and_then(|m| m.get("display_name"))
                    .and_then(|n| n.as_str())
                    .map_or_else(String::new, |name| format!(" ({name})"));
                let percent = limit
                    .get("percent")
                    .and_then(serde_json::Value::as_i64)
                    .map_or_else(|| "?".to_owned(), |p| format!("{p}%"));
                let severity = limit
                    .get("severity")
                    .and_then(|s| s.as_str())
                    .filter(|s| *s != "normal")
                    .map_or_else(String::new, |s| format!(" [{s}]"));
                let resets = limit
                    .get("resets_at")
                    .and_then(|r| r.as_str())
                    .map_or_else(String::new, |r| format!(", resets {r}"));
                format!("{kind}{scope}: {percent} used{severity}{resets}")
            })
            .collect(),
    )
}
