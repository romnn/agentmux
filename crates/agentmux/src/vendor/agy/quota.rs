//! What agy's login has left: the usage its `/usage` command reports, and how it reads.

use std::time::Duration;

use serde::Deserialize;

use crate::delegate::Vendor;
use crate::quota::{Observation, Origin, PROBE_TIMEOUT, ProbeRequest, probe_command};

/// Ask agy's `/usage` command, which it answers locally without spending a turn.
///
/// What comes back is the reply's `command.data`, passed on verbatim: groups of models, each with
/// buckets naming a window, the fraction of it left and when it resets, where a window nobody has
/// used yet resets at the Unix epoch.
pub(crate) fn probe(request: &ProbeRequest<'_>) -> Observation {
    match agy_usage(request) {
        Ok(payload) => Observation::Reported {
            origin: Origin::Live,
            payload,
        },
        Err(reason) => Observation::Unavailable { reason },
    }
}

/// Run `agy --print /usage --output-format json` and read the usage it reports.
///
/// It runs in a fresh home of its own, because agy starts the MCP servers its home configures even
/// to answer `/usage`, and its login is in the operating system's keyring rather than in the home.
/// Its output goes to a file rather than a pipe, as the version probe's does: a descendant that
/// inherited a pipe could hold it open past the deadline, where a file is read for what it holds.
fn agy_usage(request: &ProbeRequest<'_>) -> Result<serde_json::Value, String> {
    use std::io::{Read as _, Seek as _};
    use std::process::Stdio;

    let unavailable = |what: &str, error: std::io::Error| format!("{what}: {error}");
    let home = tempfile::tempdir()
        .map_err(|error| unavailable("cannot make a home for `agy --print /usage`", error))?;
    let mut output = tempfile::tempfile()
        .map_err(|error| unavailable("cannot make a file for `agy --print /usage`", error))?;
    let stdout = output
        .try_clone()
        .map_err(|error| unavailable("cannot hand `agy --print /usage` its output", error))?;
    let mut command = probe_command(request, Vendor::Agy.program());
    command
        .args(["--print", "/usage", "--output-format", "json"])
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(Stdio::null());
    let mut child = command
        .spawn()
        .map_err(|error| unavailable("cannot run `agy --print /usage`", error))?;

    let deadline = std::time::Instant::now() + PROBE_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "`agy --print /usage` did not answer within {}s",
                    PROBE_TIMEOUT.as_secs()
                ));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(unavailable(
                    "`agy --print /usage` could not be waited on",
                    error,
                ));
            }
        }
    };
    if !status.success() {
        return Err(format!("`agy --print /usage` exited with {status}"));
    }
    let mut printed = String::new();
    output
        .rewind()
        .and_then(|()| output.read_to_string(&mut printed))
        .map_err(|error| unavailable("reading `agy --print /usage`", error))?;
    agy_usage_payload(&printed)
}

/// The usage figures in what `agy --print /usage --output-format json` printed.
///
/// Only a reply that ran the `usage` command carries them: a prompt that reached the model instead
/// comes back with a `response` and no `command`.
fn agy_usage_payload(output: &str) -> Result<serde_json::Value, String> {
    #[derive(Deserialize)]
    struct Reply {
        status: String,
        #[serde(default)]
        error: Option<String>,
        #[serde(default)]
        command: Option<Command>,
    }

    #[derive(Deserialize)]
    struct Command {
        name: String,
        data: serde_json::Value,
    }

    let reply = serde_json::from_str::<Reply>(output.trim())
        .map_err(|error| format!("`agy --print /usage` printed no usage reply: {error}"))?;
    if reply.status != "SUCCESS" {
        return Err(format!(
            "`agy --print /usage` reported {}: {}",
            reply.status,
            reply.error.as_deref().unwrap_or("no reason given")
        ));
    }
    match reply.command {
        Some(Command { name, data }) if name == "usage" && data.is_object() => Ok(data),
        _ => Err(
            "`agy --print /usage` did not run its usage command. That login may not be \
                  signed in on this machine."
                .to_owned(),
        ),
    }
}

/// agy reports groups of models, each with buckets naming a window, the fraction of it left and
/// when it resets.
///
/// A window nobody has used yet resets at the Unix epoch, which is said as such rather than as a
/// reset half a century ago.
pub(crate) fn windows(payload: &serde_json::Value) -> Option<Vec<String>> {
    let groups = payload.get("groups")?.as_array()?;
    Some(
        groups
            .iter()
            .flat_map(|group| {
                let name = group
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("models");
                let buckets = group.get("buckets").and_then(|b| b.as_array());
                buckets.into_iter().flatten().map(move |bucket| {
                    let window = bucket
                        .get("window")
                        .and_then(|w| w.as_str())
                        .unwrap_or("window");
                    let used = bucket
                        .get("remaining_fraction")
                        .and_then(serde_json::Value::as_f64)
                        .map_or_else(
                            || "?".to_owned(),
                            |left| format!("{:.0}%", (1.0 - left) * 100.0),
                        );
                    let resets = match bucket.get("reset_time").and_then(|r| r.as_str()) {
                        Some(reset) if reset.starts_with("1970-01-01") => {
                            ", not used yet".to_owned()
                        }
                        Some(reset) => format!(", resets {reset}"),
                        None => String::new(),
                    };
                    format!("{name} {window}: {used} used{resets}")
                })
            })
            .collect(),
    )
}
