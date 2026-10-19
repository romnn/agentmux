//! Generates the `codex-app-server-protocol` crate's sources from the JSON Schema codex publishes.
//!
//! The inputs are all checked in:
//!
//! - `schema/codex_app_server_protocol.schemas.json`, the bundle `codex app-server
//!   generate-json-schema` writes;
//! - [`surface!`], the one table of the methods agentmux uses and their payload types, which is
//!   also the only place that says which type answers a request, since the bundle does not;
//! - `oracle/Cargo.lock`, whose codex tag is the pinned release: the oracle builds the table
//!   against that release's own Rust, and the bundle is what that release exports;
//! - the workspace's `Cargo.lock`, whose progeny commit renders the types, configured by this
//!   crate's code.
//!
//! The outputs are `src/generated/schema.rs` (the payload types, rendered by progeny) and
//! `src/generated/methods.rs` (the method bindings) of `codex-app-server-protocol`, and they are
//! checked in too.
//! Every method the bundle lists is bound by name, and the surface's methods to their types
//! ([`methods`]).
//! Generation needs neither codex nor the network, so a test can regenerate both and fail when
//! the committed files differ.
//! Only [`upstream`] runs codex, to replace the vendored bundle with what the pinned release
//! exports.

pub mod bindings;
pub mod methods;
pub mod surface;
pub mod upstream;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::surface::Surface;

/// Why generation failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A file could not be read.
    #[error("cannot read {path}")]
    Read {
        /// The file.
        path: PathBuf,
        /// Why.
        #[source]
        source: std::io::Error,
    },
    /// A file could not be written.
    #[error("cannot write {path}")]
    Write {
        /// The file.
        path: PathBuf,
        /// Why.
        #[source]
        source: std::io::Error,
    },
    /// The schema bundle is not JSON.
    #[error("the schema bundle is not JSON")]
    BundleJson(#[source] serde_json::Error),
    /// The schema bundle lacks something generation reads.
    #[error("the schema bundle is unusable: {0}")]
    Bundle(String),
    /// `Cargo.lock` does not say which progeny commit generates.
    #[error("Cargo.lock pins no git commit of progeny")]
    MissingProgenyCommit,
    /// `oracle/Cargo.lock` does not say which codex release the protocol is pinned to.
    #[error("oracle/Cargo.lock pins no `rust-v…` tag of codex")]
    MissingCodexPin,
    /// `oracle/Cargo.lock` resolved codex's crates at more than one revision, so the checks the
    /// oracle runs would not all be against one release.
    #[error("oracle/Cargo.lock resolves codex at more than one revision: {}", .0.join(", "))]
    ConflictingCodexPins(Vec<String>),
    /// A method cannot be bound.
    #[error("cannot bind `{method}`: {reason}")]
    Binding {
        /// The method.
        method: String,
        /// Why.
        reason: String,
    },
    /// progeny refused the document.
    #[error("progeny refused the document")]
    Progeny(#[source] progeny::RejectError),
    /// progeny rendered something other than one module.
    #[error("progeny rendered {0} files where one module was expected")]
    ProgenyOutput(usize),
    /// A command could not be started.
    #[error("cannot run `{command}`")]
    Spawn {
        /// The command line.
        command: String,
        /// Why.
        #[source]
        source: std::io::Error,
    },
    /// A command failed.
    #[error("`{command}` failed: {stderr}")]
    Command {
        /// The command line.
        command: String,
        /// What it wrote to stderr.
        stderr: String,
    },
    /// The codex on `PATH` is not the pinned release.
    #[error(
        "codex-cli {installed} is on PATH, but the protocol is pinned to {pinned} \
         (the oracle's codex tag): install that release, or move the pin deliberately"
    )]
    CodexVersionMismatch {
        /// The release on `PATH`.
        installed: String,
        /// The pinned release.
        pinned: String,
    },
    /// The codex on `PATH` can isolate an app-server, which changes what agentmux may drive.
    #[error(
        "codex app-server {version} accepts --ignore-user-config, so isolated consultations can \
         now be driven and steered: pass it in Delegate::app_server_settings, drop the isolation \
         condition there, then remove this check"
    )]
    AppServerIsolationAvailable {
        /// The release that accepts it.
        version: String,
    },
}

/// Where codex's bundle keeps its schemas, in precedence order: the v2 protocol's, then the
/// top-level ones, so a name both define is the v2 protocol's.
pub const NAMESPACES: [&str; 2] = ["/definitions/v2", "/definitions"];

/// Where the inputs and outputs live in the repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratorPaths {
    /// This crate's directory, which holds every input.
    pub codegen_dir: PathBuf,
}

impl GeneratorPaths {
    /// The layout of the checkout this generator was built from.
    #[must_use]
    pub fn of_this_checkout() -> Self {
        Self {
            codegen_dir: PathBuf::from(env!("CARGO_MANIFEST_DIR")),
        }
    }

    /// The vendored schema bundle.
    #[must_use]
    pub fn bundle(&self) -> PathBuf {
        self.codegen_dir
            .join("schema")
            .join("codex_app_server_protocol.schemas.json")
    }

    /// The oracle's lockfile, which pins the codex release.
    #[must_use]
    pub fn oracle_lockfile(&self) -> PathBuf {
        self.codegen_dir.join("oracle").join("Cargo.lock")
    }

    /// The workspace's lockfile, which pins progeny.
    #[must_use]
    pub fn workspace_lockfile(&self) -> PathBuf {
        self.codegen_dir.join("..").join("..").join("Cargo.lock")
    }

    /// The generated payload types.
    #[must_use]
    pub fn schema_source_path(&self) -> PathBuf {
        self.generated_src_dir().join("schema.rs")
    }

    /// The generated method bindings.
    #[must_use]
    pub fn methods_source_path(&self) -> PathBuf {
        self.generated_src_dir().join("methods.rs")
    }

    /// The protocol crate's directory of generated sources, which holds nothing written by hand.
    fn generated_src_dir(&self) -> PathBuf {
        self.codegen_dir
            .join("..")
            .join("codex-app-server-protocol")
            .join("src")
            .join("generated")
    }
}

/// Everything generation reads.
#[derive(Debug, Clone)]
pub struct Inputs {
    /// The schema bundle codex exported.
    pub bundle: Vec<u8>,
    /// The codex release it came from.
    pub codex_version: String,
    /// The progeny commit that renders them.
    pub progeny_commit: String,
}

impl Inputs {
    /// Read the checked-in inputs.
    ///
    /// # Errors
    ///
    /// Returns an error when an input cannot be read or parsed.
    pub fn read(paths: &GeneratorPaths) -> Result<Self, Error> {
        let progeny_commit = locked_progeny_commit(&read_text(&paths.workspace_lockfile())?)?;
        Ok(Self {
            bundle: read(&paths.bundle())?,
            codex_version: pinned_codex_version(paths)?,
            progeny_commit,
        })
    }
}

/// The codex release the protocol is pinned to: the `rust-v…` tag the oracle's lockfile resolved
/// codex's crate at.
///
/// # Errors
///
/// Returns [`Error::Read`] when the lockfile cannot be read, [`Error::MissingCodexPin`] when it
/// pins no such tag, and [`Error::ConflictingCodexPins`] when codex's crates resolve at more than
/// one.
pub fn pinned_codex_version(paths: &GeneratorPaths) -> Result<String, Error> {
    locked_codex_version(&read_text(&paths.oracle_lockfile())?)
}

/// codex's repository, as a lockfile names the source of a package it provides.
const CODEX_REPOSITORY: &str = "git+https://github.com/openai/codex";

/// The codex release a lockfile resolved codex's crates at, from their `rust-v…` tag.
///
/// Every package codex's repository provides must resolve to one revision: the oracle checks the
/// method table against one of codex's crates and the exec mirror against others, and a tag moved
/// for one of them alone would check the two against different releases.
fn locked_codex_version(lockfile: &str) -> Result<String, Error> {
    // Every source in codex's repository, however it is pinned: a crate on a branch beside one
    // on a tag is two revisions too.
    let sources: Vec<String> = locked_packages(lockfile)
        .into_iter()
        .filter_map(|package| package.source)
        .filter(|source| {
            source
                .strip_prefix(CODEX_REPOSITORY)
                .is_some_and(|rest| rest.starts_with(['?', '#']))
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    match sources.as_slice() {
        [source] => source
            .split_once("?tag=rust-v")
            .and_then(|(_, tag)| tag.split_once('#'))
            .map(|(version, _)| version.to_owned())
            .ok_or(Error::MissingCodexPin),
        [] => Err(Error::MissingCodexPin),
        _ => Err(Error::ConflictingCodexPins(sources)),
    }
}

/// One generated file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputFile {
    /// Where it is checked in.
    pub path: PathBuf,
    /// What it holds.
    pub contents: String,
}

impl OutputFile {
    /// Whether the checked-in file holds exactly these contents.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Read`] when the file exists but cannot be read.
    pub fn is_current(&self) -> Result<bool, Error> {
        match std::fs::read_to_string(&self.path) {
            Ok(contents) => Ok(contents == self.contents),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(source) => Err(Error::Read {
                path: self.path.clone(),
                source,
            }),
        }
    }

    /// Write the file.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Write`] when it cannot be written.
    pub fn write(&self) -> Result<(), Error> {
        std::fs::write(&self.path, &self.contents).map_err(|source| Error::Write {
            path: self.path.clone(),
            source,
        })
    }
}

/// Generate the protocol crate's sources.
///
/// # Errors
///
/// Returns an error when the bundle is unusable, it does not bear out the [`surface!`] table, or
/// progeny refuses the document.
pub fn generate(inputs: &Inputs, paths: &GeneratorPaths) -> Result<Vec<OutputFile>, Error> {
    let bundle: serde_json::Value =
        serde_json::from_slice(&inputs.bundle).map_err(Error::BundleJson)?;
    let methods = methods::read(&bundle, &Surface::table())?;

    let input = progeny::SchemaInput {
        namespaces: NAMESPACES.map(str::to_owned).into(),
        roots: methods::roots(&methods),
    };
    let output = progeny::generate_schemas(&inputs.bundle, &input, &progeny_config())
        .map_err(Error::Progeny)?;
    let module = match output.files.into_values().collect::<Vec<_>>().as_slice() {
        [module] => module.clone(),
        files => return Err(Error::ProgenyOutput(files.len())),
    };
    let header = header(inputs);
    let mut schema = header.clone();
    if !output.diagnostics.is_empty() {
        schema.push_str(indoc::indoc! {"
            //
            // What progeny could not represent faithfully:
            //
        "});
        let lines: Vec<String> = output
            .diagnostics
            .iter()
            .map(|diagnostic| {
                indoc::formatdoc!(
                    "
                        // - {diagnostic}
                    "
                )
            })
            .collect();
        schema.push_str(&lines.concat());
    }
    schema.push('\n');
    schema.push_str(&module);

    let bindings = header + "\n" + &bindings::render(&methods, &inputs.codex_version);

    Ok(vec![
        OutputFile {
            path: paths.schema_source_path(),
            contents: schema,
        },
        OutputFile {
            path: paths.methods_source_path(),
            contents: bindings,
        },
    ])
}

/// How progeny renders the types: one module of types alone, deriving what it can of `Clone`,
/// `Debug`, `PartialEq` and `Default`.
///
/// No client or server: the protocol is JSON-RPC over stdio, which progeny's HTTP renderers do
/// not describe.
///
/// Decoded strictly, so each schema is one type, and what codex sends is the type agentmux would
/// send.
/// Its enums and unions stay open, so a value a newer codex adds still decodes, and a payload
/// that does not decode is recorded whole as drift by the driver.
///
/// Integers are as wide as their `format` says, which codex's schema writes from the Rust type
/// behind each one, so an exit code is the `i32` codex holds it in.
fn progeny_config() -> progeny::Config {
    progeny::Config {
        packaging: progeny::Packaging::Module,
        decoding: progeny::Decoding::Strict,
        formats: progeny::Formats {
            integers: progeny::IntegerWidth::ByFormat,
            ..progeny::Formats::default()
        },
        derives: [
            progeny::Derive::Clone,
            progeny::Derive::Debug,
            progeny::Derive::PartialEq,
            progeny::Derive::Default,
        ]
        .into(),
        emit: progeny::Emit {
            types: true,
            client: false,
            server: false,
        },
        ..progeny::Config::default()
    }
}

/// The first line of every generated file.
fn header(inputs: &Inputs) -> String {
    indoc::formatdoc!(
        "
            // @generated by codex-app-server-codegen from codex-cli {} and progeny {}. Do not edit.
        ",
        inputs.codex_version,
        inputs.progeny_commit
    )
}

/// The progeny commit `Cargo.lock` resolved.
fn locked_progeny_commit(lockfile: &str) -> Result<String, Error> {
    locked_source(lockfile, "progeny")
        .and_then(|source| {
            let (_, commit) = source.strip_prefix("git+")?.rsplit_once('#')?;
            Some(commit.to_owned())
        })
        .ok_or(Error::MissingProgenyCommit)
}

/// One package a lockfile resolved.
#[derive(serde::Deserialize)]
struct LockedPackage {
    name: String,
    source: Option<String>,
}

/// Every package a lockfile resolved, or none when it cannot be read as one.
fn locked_packages(lockfile: &str) -> Vec<LockedPackage> {
    #[derive(serde::Deserialize)]
    struct Lockfile {
        package: Vec<LockedPackage>,
    }
    toml::from_str::<Lockfile>(lockfile)
        .map(|lockfile| lockfile.package)
        .unwrap_or_default()
}

/// Where a lockfile resolved the package `name` from, or `None` when it locks no such package or
/// is not a lockfile.
///
/// A path package of the same name has no source: the oracle builds agentmux's own protocol
/// crate beside codex's, to check the mirror it holds.
fn locked_source(lockfile: &str, name: &str) -> Option<String> {
    locked_packages(lockfile)
        .into_iter()
        .filter(|package| package.name == name)
        .find_map(|package| package.source)
}

fn read(path: &Path) -> Result<Vec<u8>, Error> {
    std::fs::read(path).map_err(|source| Error::Read {
        path: path.to_path_buf(),
        source,
    })
}

fn read_text(path: &Path) -> Result<String, Error> {
    std::fs::read_to_string(path).map_err(|source| Error::Read {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use googletest::prelude::*;
    use indoc::{formatdoc, indoc};

    use super::{Error, locked_codex_version, locked_progeny_commit};

    /// The commit is read from the git source Cargo resolved, not from the manifest's request.
    #[gtest]
    fn the_progeny_commit_comes_from_the_lockfile() -> Result<()> {
        let lockfile = indoc! {r#"
            version = 4

            [[package]]
            name = "serde"
            version = "1.0.0"
            source = "registry+https://github.com/rust-lang/crates.io-index"

            [[package]]
            name = "progeny"
            version = "0.0.1"
            source = "git+https://github.com/romnn/progeny?rev=c8e4888#c8e4888b54954acef8e83a2902feaa26c860cef5"
        "#};
        assert_that!(
            locked_progeny_commit(lockfile).or_fail()?,
            eq("c8e4888b54954acef8e83a2902feaa26c860cef5")
        );
        Ok(())
    }

    /// The codex release is the tag the oracle's lockfile resolved codex's crate at, beside
    /// agentmux's own crate of the same name.
    #[gtest]
    fn the_codex_release_comes_from_the_oracles_lockfile() -> Result<()> {
        let lockfile = indoc! {r#"
            version = 4

            [[package]]
            name = "codex-app-server-protocol"
            version = "0.0.8"

            [[package]]
            name = "codex-app-server-protocol"
            version = "0.162.1"
            source = "git+https://github.com/openai/codex?tag=rust-v0.162.1#092d3acd7f8e0f2d1b6b4f3c4a5e6d7c8b9a0f1e"
        "#};
        assert_that!(locked_codex_version(lockfile).or_fail()?, eq("0.162.1"));

        // A lockfile that resolved codex at a branch or a bare commit pins no release.
        let unpinned = lockfile.replace("?tag=rust-v0.162.1", "?branch=main");
        assert_that!(
            locked_codex_version(&unpinned),
            err(matches_pattern!(Error::MissingCodexPin))
        );

        // Nor does one that resolved another of codex's crates at another release: the oracle
        // would check the two against different codex.
        let split = formatdoc! {r#"
            {lockfile}
            [[package]]
            name = "codex-exec"
            version = "0.163.0"
            source = "git+https://github.com/openai/codex?tag=rust-v0.163.0#1111111111111111111111111111111111111111"
        "#};
        assert_that!(
            locked_codex_version(&split),
            err(matches_pattern!(Error::ConflictingCodexPins(len(eq(2)))))
        );

        // Nor one that resolved another of them at a branch.
        let branch = formatdoc! {r#"
            {lockfile}
            [[package]]
            name = "codex-exec"
            version = "0.163.0"
            source = "git+https://github.com/openai/codex?branch=main#1111111111111111111111111111111111111111"
        "#};
        assert_that!(
            locked_codex_version(&branch),
            err(matches_pattern!(Error::ConflictingCodexPins(len(eq(2)))))
        );
        Ok(())
    }
}
