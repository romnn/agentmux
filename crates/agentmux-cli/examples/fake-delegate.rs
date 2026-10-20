//! A stand-in for a delegate CLI, for the tests that run the real `agentmux` against it.
//!
//! A test copies it into a directory of its own under the name of the CLI it plays, `claude` or
//! `codex`, puts that directory on the delegate's `PATH`, and writes beside it the
//! [`FAKE_DELEGATE_CONFIG`] that says what to do.
//! It reads its configuration from beside its own executable rather than from its environment or
//! its arguments, because agentmux decides both: the environment is an allowlist and the
//! arguments are the vendor's.
//!
//! Asked for `--version` it says so and exits.
//! Otherwise it reads its question from stdin, writes the configured stream and stderr, waits
//! until the configured release file exists or its directory has gone, writes what it was told to
//! write last, and exits with the configured status.
//! It is portable, so the same scenario runs on every platform agentmux supports.

use std::io::{Read as _, Write as _};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use serde::Deserialize;

/// The configuration's file name, beside the executable.
const FAKE_DELEGATE_CONFIG: &str = "fake-delegate.json";

/// What one run of the fake does.
#[derive(Debug, Deserialize)]
struct Config {
    /// Written to stdout as soon as the question has been read.
    #[serde(default)]
    stdout: String,
    /// Written to stderr straight after.
    #[serde(default)]
    stderr: String,
    /// A file whose appearance lets the fake go on; without one it goes on at once.
    #[serde(default)]
    release_path: Option<PathBuf>,
    /// Written to stdout once released, as a CLI flushes its closing event on its way out.
    #[serde(default)]
    stdout_on_release: String,
    /// The exit status.
    #[serde(default)]
    exit_code: u8,
}

fn main() -> ExitCode {
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("fake-delegate 1.0.0");
        return ExitCode::SUCCESS;
    }
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(reason) => {
            eprintln!("fake-delegate: {reason}");
            ExitCode::from(70)
        }
    }
}

fn run() -> Result<u8, String> {
    let config_path = std::env::current_exe()
        .map_err(|err| format!("cannot locate itself: {err}"))?
        .with_file_name(FAKE_DELEGATE_CONFIG);
    let text = std::fs::read_to_string(&config_path)
        .map_err(|err| format!("cannot read {}: {err}", config_path.display()))?;
    let config: Config = serde_json::from_str(&text)
        .map_err(|err| format!("cannot parse {}: {err}", config_path.display()))?;

    let mut question = String::new();
    std::io::stdin()
        .read_to_string(&mut question)
        .map_err(|err| format!("cannot read the question: {err}"))?;
    let mut stdout = std::io::stdout();
    write!(stdout, "{}", config.stdout).map_err(|err| format!("cannot write stdout: {err}"))?;
    stdout
        .flush()
        .map_err(|err| format!("cannot flush stdout: {err}"))?;
    eprint!("{}", config.stderr);

    // A release file whose directory has gone belongs to a test that has finished, which is
    // released too, so a fake never outlives the test that started it.
    if let Some(release) = &config.release_path {
        while !release.exists() && release.parent().is_some_and(std::path::Path::exists) {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    write!(stdout, "{}", config.stdout_on_release)
        .map_err(|err| format!("cannot write stdout: {err}"))?;
    stdout
        .flush()
        .map_err(|err| format!("cannot flush stdout: {err}"))?;
    Ok(config.exit_code)
}
