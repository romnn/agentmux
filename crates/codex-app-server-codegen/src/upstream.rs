//! Asks the pinned codex CLI for its schema bundle.
//!
//! This is the only part of the generator that needs codex, and it is not part of generation: it
//! replaces the vendored bundle, which generation then reads like any other checked-in input.

use std::process::Command;

use crate::Error;

/// The program asked, from `PATH`.
const CODEX: &str = "codex";

/// The bundle the pinned codex exports, after checking that it is the pinned release and that its
/// app-server still cannot be isolated.
///
/// The bundle is exported without `--experimental`, so it describes only the stable protocol.
///
/// # Errors
///
/// Returns [`Error::CodexVersion`] when the codex on `PATH` is another release,
/// [`Error::IgnoreUserConfig`] when its app-server accepts `--ignore-user-config`, and an error
/// when codex cannot be run or writes no bundle.
pub fn export(pinned: &str) -> Result<Vec<u8>, Error> {
    let version = run(&["--version"])?.stdout;
    let installed = version.split_whitespace().last().unwrap_or_default();
    if installed != pinned {
        return Err(Error::CodexVersion {
            installed: installed.to_owned(),
            pinned: pinned.to_owned(),
        });
    }

    // An isolated consultation runs over `codex exec` only because the app-server cannot skip the
    // user's configuration, so the release that adds the flag is the one to change
    // `Delegate::app_server_settings` for.
    // `--help` writes to either stream, depending on the release, so both are searched.
    let help = run(&["app-server", "--help"])?;
    if help.stdout.contains("--ignore-user-config") || help.stderr.contains("--ignore-user-config")
    {
        return Err(Error::IgnoreUserConfig {
            version: installed.to_owned(),
        });
    }

    let out = tempfile::tempdir().map_err(|source| Error::Spawn {
        command: "a temporary directory".to_owned(),
        source,
    })?;
    let out_dir = out.path().to_string_lossy().into_owned();
    run(&["app-server", "generate-json-schema", "--out", &out_dir])?;
    let bundle = out.path().join("codex_app_server_protocol.schemas.json");
    std::fs::read(&bundle).map_err(|source| Error::Read {
        path: bundle,
        source,
    })
}

/// What a successful codex command wrote.
struct Said {
    stdout: String,
    stderr: String,
}

/// Run codex with `args`.
fn run(args: &[&str]) -> Result<Said, Error> {
    let command = format!("{CODEX} {}", args.join(" "));
    let output = Command::new(CODEX)
        .args(args)
        .output()
        .map_err(|source| Error::Spawn {
            command: command.clone(),
            source,
        })?;
    if !output.status.success() {
        return Err(Error::Command {
            command,
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    Ok(Said {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}
