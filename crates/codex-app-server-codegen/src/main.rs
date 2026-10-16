//! `codex-app-server-codegen`: regenerate the codex app-server protocol crate, or check it.

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use codex_app_server_codegen::{Error, File, Inputs, Layout, generate, upstream};

#[derive(Debug, Parser)]
#[command(about = "Generate codex-app-server-protocol from codex's own JSON Schema")]
struct Options {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Regenerate the protocol crate's sources from the vendored schema bundle.
    ///
    /// Needs neither codex nor the network.
    Generate {
        /// Change nothing, and fail when a checked-in source differs from what would be
        /// generated.
        #[arg(long)]
        check: bool,
    },
    /// Replace the vendored schema bundle with what the pinned codex on PATH exports, then
    /// regenerate.
    Refresh {
        /// Change nothing, and fail when the vendored bundle differs from what the pinned codex
        /// exports.
        #[arg(long)]
        check: bool,
    },
}

fn main() -> ExitCode {
    let options = Options::parse();
    let layout = Layout::of_this_checkout();
    let outcome = match options.command {
        Command::Generate { check } => regenerate(&layout, check),
        Command::Refresh { check } => refresh(&layout, check),
    };
    match outcome {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(err) => {
            let mut causes = vec![err.to_string()];
            let mut source = std::error::Error::source(&err);
            while let Some(cause) = source {
                causes.push(cause.to_string());
                source = cause.source();
            }
            eprintln!("error: {}", causes.join(": "));
            ExitCode::FAILURE
        }
    }
}

/// Generate the sources and write them, or with `check`, report the ones that differ.
///
/// Returns whether everything is current.
fn regenerate(layout: &Layout, check: bool) -> Result<bool, Error> {
    let files = generate(&Inputs::read(layout)?, layout)?;
    if !check {
        files.iter().try_for_each(File::write)?;
        return Ok(true);
    }
    let mut current = true;
    for file in &files {
        if !file.is_current()? {
            eprintln!(
                "{} is not what the vendored schema generates; run `task codegen:app-server`",
                file.path.display()
            );
            current = false;
        }
    }
    Ok(current)
}

/// Export the bundle from the pinned codex and regenerate, or with `check`, report whether the
/// vendored bundle is what it exports.
fn refresh(layout: &Layout, check: bool) -> Result<bool, Error> {
    let pinned = Inputs::read(layout)?.codex_version;
    let exported = File {
        path: layout.bundle(),
        contents: String::from_utf8_lossy(&upstream::export(&pinned)?).into_owned(),
    };
    if check {
        let current = exported.is_current()?;
        if !current {
            eprintln!(
                "{} is not what codex-cli {pinned} exports; run `task codegen:app-server:refresh`",
                exported.path.display()
            );
        }
        return Ok(current);
    }
    exported.write()?;
    regenerate(layout, false)
}
