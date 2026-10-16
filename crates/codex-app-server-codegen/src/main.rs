//! `codex-app-server-codegen`: regenerate the codex app-server protocol crate, or check it.

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use codex_app_server_codegen::{
    Error, File, Inputs, Layout, generate, pinned_codex_version, upstream, vendored_responses,
};

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
    /// Replace the vendored schema bundle with what the pinned codex on PATH exports, and the
    /// vendored responses with what that release's source declares, then regenerate.
    ///
    /// Fetches codex's source from GitHub.
    Refresh {
        /// Change nothing, and fail when a vendored input differs from what the pinned release
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

/// Export the vendored inputs from the pinned release and regenerate, or with `check`, report
/// whether they are what it exports.
fn refresh(layout: &Layout, check: bool) -> Result<bool, Error> {
    let pinned = pinned_codex_version(layout)?;
    let bundle = upstream::export(&pinned)?;
    let responses = vendored_responses(&bundle, &upstream::definitions(&pinned)?)?;
    let exported = [
        File {
            path: layout.bundle(),
            contents: String::from_utf8_lossy(&bundle).into_owned(),
        },
        File {
            path: layout.responses(),
            contents: responses.to_json()?,
        },
    ];
    if check {
        let mut current = true;
        for file in &exported {
            if !file.is_current()? {
                eprintln!(
                    "{} is not what codex-cli {pinned} exports; run `task codegen:app-server:refresh`",
                    file.path.display()
                );
                current = false;
            }
        }
        return Ok(current);
    }
    exported.iter().try_for_each(File::write)?;
    regenerate(layout, false)
}
