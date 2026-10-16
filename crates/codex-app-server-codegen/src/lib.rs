//! Generates the `codex-app-server-protocol` crate's sources from the JSON Schema codex publishes.
//!
//! The inputs are all checked in:
//!
//! - `schema/codex_app_server_protocol.schemas.json`, the bundle `codex app-server
//!   generate-json-schema` writes, and `schema/codex-version`, the codex release it came from;
//! - `schema/codex_app_server_protocol.responses.json`, which type answers each request, read
//!   from that release's own request definitions ([`responses`]), since the bundle does not say;
//! - `Cargo.lock`, whose progeny commit renders the types, configured by this crate's code.
//!
//! The outputs are `src/schema.rs` (the payload types, rendered by progeny) and `src/methods.rs`
//! (the method bindings) of `codex-app-server-protocol`, and they are checked in too.
//! Every method the bundle lists is bound ([`methods`]); nothing is chosen by hand.
//! Generation needs neither codex nor the network, so a test can regenerate both and fail when
//! the committed files differ.
//! Only [`upstream`] runs codex and fetches its source, to replace the vendored inputs with what
//! the pinned release exports.

pub mod bindings;
pub mod methods;
pub mod openapi;
pub mod responses;
pub mod upstream;

use std::path::{Path, PathBuf};

use crate::openapi::Schemas;
use crate::responses::{Responses, Side};

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
    ProgenyCommit,
    /// A schema a method reaches does not exist.
    #[error("codex's schema defines no `{0}`")]
    UnknownSchema(String),
    /// A method cannot be bound.
    #[error("cannot bind `{method}`: {reason}")]
    Binding {
        /// The method.
        method: String,
        /// Why.
        reason: String,
    },
    /// codex's request definitions cannot be read.
    #[error("codex's request definitions are unusable: {0}")]
    Definitions(String),
    /// A file could not be downloaded.
    #[error("cannot fetch {url}")]
    Fetch {
        /// What was asked for.
        url: String,
        /// Why.
        #[source]
        source: Box<ureq::Error>,
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
         (schema/codex-version): install that release, or move the pin deliberately"
    )]
    CodexVersion {
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
    IgnoreUserConfig {
        /// The release that accepts it.
        version: String,
    },
}

/// Where the inputs and outputs live in the repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// This crate's directory, which holds every input.
    pub root: PathBuf,
}

impl Layout {
    /// The layout of the checkout this generator was built from.
    #[must_use]
    pub fn of_this_checkout() -> Self {
        Self {
            root: PathBuf::from(env!("CARGO_MANIFEST_DIR")),
        }
    }

    /// The vendored schema bundle.
    #[must_use]
    pub fn bundle(&self) -> PathBuf {
        self.root
            .join("schema")
            .join("codex_app_server_protocol.schemas.json")
    }

    /// Which type answers each request, from codex's request definitions.
    #[must_use]
    pub fn responses(&self) -> PathBuf {
        self.root
            .join("schema")
            .join("codex_app_server_protocol.responses.json")
    }

    /// The codex release the bundle came from.
    #[must_use]
    pub fn codex_version(&self) -> PathBuf {
        self.root.join("schema").join("codex-version")
    }

    /// This crate's lockfile, which pins progeny.
    #[must_use]
    pub fn lockfile(&self) -> PathBuf {
        self.root.join("Cargo.lock")
    }

    /// The generated payload types.
    #[must_use]
    pub fn schema_source(&self) -> PathBuf {
        self.protocol_source().join("schema.rs")
    }

    /// The generated method bindings.
    #[must_use]
    pub fn methods_source(&self) -> PathBuf {
        self.protocol_source().join("methods.rs")
    }

    fn protocol_source(&self) -> PathBuf {
        self.root
            .join("..")
            .join("codex-app-server-protocol")
            .join("src")
    }
}

/// Everything generation reads.
#[derive(Debug, Clone)]
pub struct Inputs {
    /// The schema bundle codex exported.
    pub bundle: Vec<u8>,
    /// Which type answers each request.
    pub responses: Vec<u8>,
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
    pub fn read(layout: &Layout) -> Result<Self, Error> {
        let progeny_commit = locked_progeny_commit(&read_text(&layout.lockfile())?)?;
        Ok(Self {
            bundle: read(&layout.bundle())?,
            responses: read(&layout.responses())?,
            codex_version: pinned_codex_version(layout)?,
            progeny_commit,
        })
    }
}

/// The codex release the vendored inputs come from, which is all that refreshing them reads.
///
/// # Errors
///
/// Returns [`Error::Read`] when the pin cannot be read.
pub fn pinned_codex_version(layout: &Layout) -> Result<String, Error> {
    Ok(read_text(&layout.codex_version())?.trim().to_owned())
}

/// One generated file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct File {
    /// Where it is checked in.
    pub path: PathBuf,
    /// What it holds.
    pub contents: String,
}

impl File {
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
/// Returns an error when the bundle is unusable, it and the responses disagree, or progeny
/// refuses the document.
pub fn generate(inputs: &Inputs, layout: &Layout) -> Result<Vec<File>, Error> {
    let bundle: serde_json::Value =
        serde_json::from_slice(&inputs.bundle).map_err(Error::BundleJson)?;
    let schemas = Schemas::from_bundle(&bundle)?;
    let responses = Responses::from_json(&inputs.responses)?;
    let methods = methods::read(&bundle, &responses, &schemas)?;

    let kept = schemas.closure(methods::roots(&methods))?;
    let document = schemas.document(&kept, &inputs.codex_version);
    let output = progeny::generate(document.to_string().as_bytes(), &progeny_config())
        .map_err(Error::Progeny)?;
    let module = match output.files.into_values().collect::<Vec<_>>().as_slice() {
        [module] => module.clone(),
        files => return Err(Error::ProgenyOutput(files.len())),
    };
    let header = header(inputs);
    let mut schema = header.clone();
    if !output.diagnostics.is_empty() {
        schema.push_str("//\n// What progeny could not represent faithfully:\n//\n");
        let lines: Vec<String> = output
            .diagnostics
            .iter()
            .map(|diagnostic| format!("// - {diagnostic}\n"))
            .collect();
        schema.push_str(&lines.concat());
    }
    schema.push('\n');
    schema.push_str(&module);

    let bindings = header + "\n" + &bindings::render(&methods, &inputs.codex_version);

    Ok(vec![
        File {
            path: layout.schema_source(),
            contents: schema,
        },
        File {
            path: layout.methods_source(),
            contents: bindings,
        },
    ])
}

/// How progeny renders the types: one module of types alone, deriving what it can of `Clone`,
/// `Debug`, `PartialEq` and `Default`.
///
/// No client or server: the protocol is JSON-RPC over stdio, which progeny's HTTP renderers do
/// not describe.
fn progeny_config() -> progeny::Config {
    progeny::Config {
        packaging: progeny::Packaging::Module,
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
    format!(
        "// @generated by codex-app-server-codegen from codex-cli {} and progeny {}. Do not edit.\n",
        inputs.codex_version, inputs.progeny_commit
    )
}

/// The progeny commit `Cargo.lock` resolved, from its `source` line.
fn locked_progeny_commit(lockfile: &str) -> Result<String, Error> {
    #[derive(serde::Deserialize)]
    struct Lockfile {
        package: Vec<Package>,
    }
    #[derive(serde::Deserialize)]
    struct Package {
        name: String,
        source: Option<String>,
    }
    let lockfile: Lockfile = toml::from_str(lockfile).map_err(|_| Error::ProgenyCommit)?;
    lockfile
        .package
        .into_iter()
        .find(|package| package.name == "progeny")
        .and_then(|package| package.source)
        .and_then(|source| {
            source
                .strip_prefix("git+")
                .and_then(|git| git.rsplit_once('#'))
                .map(|(_, commit)| commit.to_owned())
        })
        .ok_or(Error::ProgenyCommit)
}

/// Which type answers each request the bundle lists, read from codex's request definitions.
///
/// # Errors
///
/// Returns an error when the bundle is unusable or the definitions cannot be read.
pub fn vendored_responses(bundle: &[u8], definitions: &str) -> Result<Responses, Error> {
    let bundle: serde_json::Value = serde_json::from_slice(bundle).map_err(Error::BundleJson)?;
    let client = methods::requests_sent_by(&bundle, Side::Client)?;
    let server = methods::requests_sent_by(&bundle, Side::Server)?;
    Ok(
        Responses::parse(definitions)?.retain(|side, method| match side {
            Side::Client => client.contains(method),
            Side::Server => server.contains(method),
        }),
    )
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
    use indoc::indoc;

    use super::locked_progeny_commit;

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
}
