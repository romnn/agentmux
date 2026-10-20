//! A caller that captures the output of the command it runs, for the tests that run the real
//! `agentmux` under one.
//!
//! It runs its arguments as a command, with the outputs piped, reads both to their end, prints
//! them as its own and exits with the command's status.
//! Before it starts the command it keeps its own standard handles from it, as agentmux does, so
//! a test capturing this wrapper sees the end of its output once it exits even while a process
//! the command started runs on.

use std::io::{Read as _, Write as _};
use std::process::{Command, ExitCode, Stdio};

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(reason) => {
            eprintln!("capture-wrapper: {reason}");
            ExitCode::from(70)
        }
    }
}

fn run() -> Result<u8, String> {
    let mut args = std::env::args_os().skip(1);
    let program = args.next().ok_or("no command to run")?;

    agentmux::testing::keep_std_handles_from_children();
    let mut child = Command::new(&program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("cannot start {}: {err}", program.to_string_lossy()))?;

    // Stderr is read on a thread of its own, so a command that fills it never stalls on stdout.
    let mut stderr_pipe = child.stderr.take().ok_or("no stderr pipe")?;
    let stderr_reader = std::thread::spawn(move || {
        let mut text = Vec::new();
        stderr_pipe.read_to_end(&mut text).map(|_| text)
    });
    let mut stdout = Vec::new();
    child
        .stdout
        .take()
        .ok_or("no stdout pipe")?
        .read_to_end(&mut stdout)
        .map_err(|err| format!("cannot read stdout: {err}"))?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| "the stderr reader failed")?
        .map_err(|err| format!("cannot read stderr: {err}"))?;
    let status = child
        .wait()
        .map_err(|err| format!("cannot wait for the command: {err}"))?;

    std::io::stdout()
        .write_all(&stdout)
        .map_err(|err| format!("cannot write stdout: {err}"))?;
    std::io::stderr()
        .write_all(&stderr)
        .map_err(|err| format!("cannot write stderr: {err}"))?;
    // A command killed by a signal, or one whose status does not fit, ends this as a failure.
    Ok(status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .unwrap_or(1))
}
