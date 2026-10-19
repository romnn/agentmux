//! `codex-app-server-codegen`: regenerate the codex app-server protocol crate, or check it.

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use codex_app_server_codegen::{
    Error, GeneratorPaths, Inputs, OutputFile, generate, pinned_codex_version, upstream,
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
    /// Replace the vendored schema bundle with what the pinned codex on PATH exports, then
    /// regenerate.
    Refresh {
        /// Change nothing, and fail when the vendored bundle differs from what the pinned release
        /// exports.
        #[arg(long)]
        check: bool,
    },
    /// Print the pinned codex release: the tag the oracle builds codex's own crate at.
    Pin,
}

fn main() -> ExitCode {
    let options = Options::parse();
    let paths = GeneratorPaths::of_this_checkout();
    let outcome = match options.command {
        Command::Generate { check } => regenerate(&paths, check),
        Command::Refresh { check } => refresh(&paths, check),
        Command::Pin => pinned_codex_version(&paths).map(|version| {
            println!("{version}");
            true
        }),
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
fn regenerate(paths: &GeneratorPaths, check: bool) -> Result<bool, Error> {
    let files = generate(&Inputs::read(paths)?, paths)?;
    if !check {
        files.iter().try_for_each(OutputFile::write)?;
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

/// Export the vendored bundle from the pinned release and regenerate, or with `check`, report
/// whether it is what that release exports.
fn refresh(paths: &GeneratorPaths, check: bool) -> Result<bool, Error> {
    let pinned = pinned_codex_version(paths)?;
    let exported = OutputFile {
        path: paths.bundle(),
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
    regenerate(paths, false)
}
