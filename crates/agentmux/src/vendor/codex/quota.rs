//! What a Codex account has left: the rate limits its app-server reports, and the usage window
//! a turn's rollout records.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use codex_app_server_protocol::rollout::{
    EventMsg, RateLimitSnapshot, RateLimitWindow, RolloutItem,
};

use crate::config::home_dir;
use crate::delegate::{CODEX_CREDENTIAL_VARS, Vendor};
use crate::quota::{Observation, Origin, PROBE_TIMEOUT, ProbeRequest, ProbeTarget, probe_command};
use crate::transcript::UsageWindow;

/// The window a Codex identity's environment, `env`, lets a probe read.
///
/// A key in the environment does not by itself make an account key-only: Codex may prefer the
/// login it recorded, and [`stored_login_wins`] is what asks.
pub(crate) fn probe_target(
    env: &BTreeMap<String, String>,
    host_env: &BTreeMap<String, String>,
) -> ProbeTarget {
    let vars = CODEX_CREDENTIAL_VARS;
    let config_dir = env.get(vars.config_dir).map(PathBuf::from);
    // An endpoint of the identity's own is not the vendor's window whatever the login says, so it
    // answers before the login is consulted at all.
    if env.contains_key(vars.base_url) {
        return ProbeTarget::NoSubscriptionWindow;
    }
    if env.contains_key(vars.api_key) && !stored_login_wins(config_dir.as_deref(), host_env) {
        return ProbeTarget::NoSubscriptionWindow;
    }
    ProbeTarget::Window { config_dir }
}

/// Whether the login Codex stored on disk outranks a key in the environment.
///
/// The precedence here is measured, not assumed.
/// Codex records the mode it authenticated in at `auth.json`, and a `chatgpt` login there is the
/// one it uses even when `OPENAI_API_KEY` is exported: a delegate launched with both is refused
/// by the API as a `chatgpt` account.
///
/// Reading the key alone would therefore report every Codex account on a machine with a stray key
/// in its environment as having no window to read, which is the one thing `quota` exists to say.
///
fn stored_login_wins(config_dir: Option<&Path>, host_env: &BTreeMap<String, String>) -> bool {
    // This is the CLI's own default when the identity names no directory, and also where it
    // wrote the file the last time a person logged in.
    let Some(dir) = config_dir
        .map(Path::to_path_buf)
        .or_else(|| home_dir(host_env).map(|home| home.join(".codex")))
    else {
        return false;
    };
    let Ok(text) = std::fs::read_to_string(dir.join("auth.json")) else {
        return false;
    };
    let Ok(auth) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    // Only the mode is read.
    // Whether the stored tokens still work is the probe's business, and it reports that as
    // `Unavailable` with a reason, which is honest where "billed per token" would be false.
    auth.get("auth_mode").and_then(serde_json::Value::as_str) == Some("chatgpt")
}

/// Ask Codex over the app-server protocol.
///
/// Codex publishes nothing to disk outside a session's rollout, so this is the only way to read it
/// without spending a turn.
/// It is a short-lived child speaking JSON-RPC on stdio, and it costs no tokens.
pub(crate) fn probe(request: &ProbeRequest<'_>) -> Observation {
    match codex_rate_limits(request) {
        Ok(payload) => Observation::Reported {
            origin: Origin::Live,
            payload,
        },
        Err(reason) => Observation::Unavailable { reason },
    }
}

/// One JSON-RPC exchange with `codex app-server`.
fn codex_rate_limits(request: &ProbeRequest<'_>) -> Result<serde_json::Value, String> {
    use std::process::Stdio;

    let mut command = probe_command(request, Vendor::Codex.program());
    command.arg("app-server");

    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("cannot run `codex app-server`: {error}"))?;

    let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        let _ = child.kill();
        return Err("codex app-server did not offer stdio".to_owned());
    };

    // The exchange runs on its own thread so a server that never answers cannot hang the caller.
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // A caller that stopped waiting has already reported the timeout.
        let _ = sender.send(exchange_rate_limits(&mut stdin, stdout));
    });

    let answer = receiver.recv_timeout(PROBE_TIMEOUT);
    // The child is finished with either way; leaving it running would leak a process per probe.
    let _ = child.kill();
    let _ = child.wait();
    match answer {
        Ok(result) => result,
        Err(_) => Err(format!(
            "codex app-server did not answer within {}s",
            PROBE_TIMEOUT.as_secs()
        )),
    }
}

/// The handshake and the `account/rateLimits/read` request, over the protocol crate's own
/// bindings, and the answer to it.
///
/// The answer is passed on exactly as it arrived: what a usage figure means depends on the plan
/// behind it, which agentmux does not know, so it is not decoded into the schema's shape.
fn exchange_rate_limits(
    stdin: &mut impl std::io::Write,
    stdout: impl std::io::Read,
) -> Result<serde_json::Value, String> {
    use std::io::BufRead as _;

    use codex_app_server_protocol::envelope::{Message, RequestId};
    use codex_app_server_protocol::methods;

    const INITIALIZE_REQUEST_ID: RequestId = RequestId::Integer(1);
    const RATE_LIMITS_REQUEST_ID: RequestId = RequestId::Integer(2);

    let outgoing_messages = [
        Message::request::<methods::Initialize>(
            INITIALIZE_REQUEST_ID,
            &crate::appserver::initialize_params(),
        ),
        Message::notification::<methods::Initialized>(&()),
        Message::request::<methods::AccountRateLimitsRead>(RATE_LIMITS_REQUEST_ID, &None),
    ]
    .into_iter()
    .collect::<Result<Vec<_>, _>>()
    .map_err(|err| format!("{err}: {}", err.source))?;
    outgoing_messages
        .iter()
        .try_for_each(|message| writeln!(stdin, "{}", message.to_line()))
        .and_then(|()| stdin.flush())
        .map_err(|_| "codex app-server closed its input".to_owned())?;

    // The server interleaves unsolicited notifications with replies, so the answers are found by
    // their request ids rather than by taking the next lines.
    for line in std::io::BufReader::new(stdout).lines() {
        let line = line.map_err(|err| format!("reading codex app-server: {err}"))?;
        let Ok(Message::Response { id, outcome }) = Message::parse(&line) else {
            continue;
        };
        match (id, outcome) {
            (id, Err(error)) if id == INITIALIZE_REQUEST_ID => {
                return Err(format!(
                    "codex app-server refused the handshake: {}",
                    error_message_or_fallback(&error.message)
                ));
            }
            (id, Err(error)) if id == RATE_LIMITS_REQUEST_ID => {
                return Err(format!(
                    "codex app-server refused: {}",
                    error_message_or_fallback(&error.message)
                ));
            }
            (id, Ok(result)) if id == RATE_LIMITS_REQUEST_ID => return Ok(result),
            _ => {}
        }
    }
    Err("codex app-server ended without answering. That account may not be logged in.".to_owned())
}

/// What a JSON-RPC error said, or that it said nothing.
fn error_message_or_fallback(message: &str) -> &str {
    if message.is_empty() {
        "no reason given"
    } else {
        message
    }
}

/// The rate-limit figures Codex wrote while running one turn.
///
/// Codex reports nothing about usage on the stream agentmux captures, but it records it in the
/// session rollout it writes anyway.
/// Reading that afterwards costs nothing and needs no second process, which is the only reason a
/// Codex consultation can report a usage window at all.
///
/// `thread_id` is the identifier the stream already yielded, and the rollout file carrying it is
/// found by name.
#[must_use]
pub fn codex_rollout_rate_limits(codex_home: &Path, thread_id: &str) -> Option<RateLimitSnapshot> {
    let rollout = find_rollout(&codex_home.join("sessions"), thread_id)?;
    let text = std::fs::read_to_string(rollout).ok()?;
    // The last record wins: each is a snapshot, and the newest is the state the turn ended in.
    text.lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<RolloutItem>(line).ok())
        .find_map(|item| match item {
            RolloutItem::EventMsg {
                payload: EventMsg::TokenCount(count),
            } => count.rate_limits,
            RolloutItem::EventMsg {
                payload: EventMsg::Other,
            }
            | RolloutItem::Other => None,
        })
}

/// Find the rollout file for `thread_id`.
///
/// Rollouts are filed under `sessions/YYYY/MM/DD/` as `rollout-<timestamp>-<thread_id>.jsonl`,
/// so the search is a bounded walk rather than a glob dependency.
/// The id is matched as the whole tail of the name, not as a substring: a thread id is opaque,
/// and an opaque id that happened to be short would otherwise match every file on the day.
fn find_rollout(sessions: &Path, thread_id: &str) -> Option<PathBuf> {
    fn walk(dir: &Path, suffix: &str, depth: usize) -> Option<PathBuf> {
        if depth > 4 {
            return None;
        }
        let mut directories = Vec::new();
        let mut matches = Vec::new();
        for entry in std::fs::read_dir(dir).ok()?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                directories.push(path);
            } else if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(suffix))
            {
                matches.push(path);
            }
        }
        // Newest first at both levels: names lead with a timestamp, so the last one sorts last,
        // and a repeated thread id resolves to the most recent run.
        if let Some(newest) = matches.into_iter().max() {
            return Some(newest);
        }
        directories.sort_unstable();
        directories
            .into_iter()
            .rev()
            .find_map(|child| walk(&child, suffix, depth + 1))
    }
    walk(sessions, &format!("-{thread_id}.jsonl"), 0)
}

/// The usage window Codex recorded in the rollout of the thread `thread_id` under `codex_home`.
///
/// Only the account-wide bucket is recorded there; the per-model ones are not in a rollout.
pub(crate) fn rollout_usage_window(codex_home: &Path, thread_id: &str) -> Option<UsageWindow> {
    let snapshot = codex_rollout_rate_limits(codex_home, thread_id)?;
    let selected = select_rate_limit_window(&snapshot)?;
    let resets_at = selected
        .resets_at
        .and_then(chrono::DateTime::from_timestamp_secs)?;
    Some(UsageWindow {
        resets_at,
        exhausted: selected.exhausted(),
        used_percent: Some(selected.used_percent),
        window: snapshot.limit_id.clone(),
    })
}

/// The window a Codex turn's usage is reported against.
///
/// An exhausted window is the one a request is refused against, and when several are, the one
/// that reopens last is the one that decides when a request can succeed again.
/// With none exhausted it is the one closest to closing, which is what the next question has to
/// expect.
/// A window that names no reopening time cannot answer either question, and is passed over.
fn select_rate_limit_window(snapshot: &RateLimitSnapshot) -> Option<&RateLimitWindow> {
    [&snapshot.primary, &snapshot.secondary]
        .into_iter()
        .flatten()
        .filter(|window| window.resets_at.is_some())
        .max_by(|left_window, right_window| {
            left_window
                .exhausted()
                .cmp(&right_window.exhausted())
                .then_with(|| {
                    if left_window.exhausted() {
                        left_window.resets_at.cmp(&right_window.resets_at)
                    } else {
                        left_window
                            .used_percent
                            .total_cmp(&right_window.used_percent)
                    }
                })
        })
}

/// Codex reports a map of named buckets, each with a primary and optional secondary window.
pub(crate) fn windows(payload: &serde_json::Value) -> Option<Vec<String>> {
    let buckets = payload.get("rateLimitsByLimitId")?.as_object()?;
    Some(
        buckets
            .iter()
            .map(|(id, bucket)| {
                let name = bucket
                    .get("limitName")
                    .and_then(|n| n.as_str())
                    .map_or_else(String::new, |n| format!(" ({n})"));
                let windows = ["primary", "secondary"]
                    .into_iter()
                    .filter_map(|which| {
                        let window = bucket.get(which)?;
                        let percent = window.get("usedPercent")?.as_i64()?;
                        let minutes = window
                            .get("windowDurationMins")
                            .and_then(serde_json::Value::as_i64)
                            .map_or_else(String::new, |m| format!(" over {m}m"));
                        Some(format!("{percent}% used{minutes}"))
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{id}{name}: {windows}")
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use googletest::prelude::*;
    use indoc::indoc;
    use serde_json::{Value, json};

    use codex_app_server_protocol::rollout::{RateLimitSnapshot, RateLimitWindow};

    use super::{exchange_rate_limits, select_rate_limit_window};

    /// The probe sends the handshake and the read through the protocol's bindings, skips what the
    /// server says in between, and hands back the answer exactly as it arrived.
    #[gtest]
    fn the_answer_to_the_read_is_passed_on_verbatim() -> Result<()> {
        let server = indoc! {r#"
            {"id":1,"result":{"userAgent":"codex"}}
            {"method":"account/rateLimits/updated","params":{}}
            {"id":2,"result":{"rateLimits":{"primary":{"usedPercent":11}},"someFieldInventedNextYear":1}}
        "#};
        let mut sent = Vec::new();

        let answer = exchange_rate_limits(&mut sent, server.as_bytes()).or_fail()?;

        assert_that!(
            answer,
            eq(
                &json!({"rateLimits": {"primary": {"usedPercent": 11}}, "someFieldInventedNextYear": 1})
            )
        );
        let methods: Vec<Value> = String::from_utf8(sent)
            .or_fail()?
            .lines()
            .map(|line| {
                serde_json::from_str::<Value>(line).map(|message| message["method"].clone())
            })
            .collect::<std::result::Result<_, _>>()
            .or_fail()?;
        assert_that!(
            methods,
            elements_are![
                eq(&json!("initialize")),
                eq(&json!("initialized")),
                eq(&json!("account/rateLimits/read"))
            ]
        );
        Ok(())
    }

    /// A refused handshake is reported as such at once, not left to look like a server that never
    /// answered.
    #[gtest]
    fn a_refused_handshake_is_reported_as_refused() {
        let server = r#"{"id":1,"error":{"code":-32600,"message":"not logged in"}}"#;
        assert_that!(
            exchange_rate_limits(&mut Vec::new(), server.as_bytes()),
            err(eq("codex app-server refused the handshake: not logged in"))
        );
    }

    fn window(used_percent: f64, resets_at: Option<i64>) -> RateLimitWindow {
        RateLimitWindow {
            used_percent,
            window_minutes: None,
            resets_at,
        }
    }

    fn selected_reset_timestamp(
        primary: Option<RateLimitWindow>,
        secondary: Option<RateLimitWindow>,
    ) -> Option<i64> {
        let snapshot = RateLimitSnapshot {
            limit_id: None,
            primary,
            secondary,
        };
        select_rate_limit_window(&snapshot).and_then(|window| window.resets_at)
    }

    /// The window a refusal is reported against is the exhausted one, however full the other is,
    /// and with both exhausted it is the one that reopens last.
    #[gtest]
    fn an_exhausted_window_binds_whichever_slot_it_is_in() {
        // A five-hour window nearly full and a weekly one used up: waiting for the five-hour
        // window to reopen would only be refused again.
        assert_that!(
            selected_reset_timestamp(Some(window(90.0, Some(10))), Some(window(100.0, Some(500)))),
            some(eq(500))
        );
        assert_that!(
            selected_reset_timestamp(
                Some(window(100.0, Some(10))),
                Some(window(100.0, Some(500)))
            ),
            some(eq(500))
        );
        assert_that!(
            selected_reset_timestamp(Some(window(100.0, Some(10))), Some(window(40.0, Some(500)))),
            some(eq(10))
        );
    }

    /// With nothing exhausted, the window closest to closing is the one to expect, and a window
    /// with no reopening time cannot be reported.
    #[gtest]
    fn otherwise_the_fullest_window_with_a_reopening_time_binds() {
        assert_that!(
            selected_reset_timestamp(Some(window(20.0, Some(10))), Some(window(60.0, Some(500)))),
            some(eq(500))
        );
        assert_that!(
            selected_reset_timestamp(Some(window(20.0, Some(10))), Some(window(100.0, None))),
            some(eq(10))
        );
        assert_that!(selected_reset_timestamp(None, None), none());
    }
}
