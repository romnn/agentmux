//! The identity of a consultation.

use chrono::Utc;
use serde::{Deserialize, Serialize};

/// A run id was not of the accepted shape.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("invalid consultation id {value:?}: ids are 1-64 characters of [0-9A-Za-z-]")]
pub struct RunIdError {
    /// What was supplied.
    pub value: String,
}

/// The identity of a consultation.
///
/// A run id arrives from a language model and is used to open a directory.
/// Parsing is the only way to build one, so path traversal through a tool argument is not a bug
/// that was fixed; it is a state that cannot be constructed.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RunId(String);

impl RunId {
    /// Longest id accepted.
    pub const MAX_LEN: usize = 64;

    /// Validate an id supplied by a caller.
    ///
    /// # Errors
    ///
    /// Returns [`RunIdError`] for anything outside `[0-9A-Za-z-]{1,64}`, which rejects `..`, `/`,
    /// `\`, absolute paths, NUL and every other control character by construction.
    pub fn parse(value: &str) -> Result<Self, RunIdError> {
        let ok = !value.is_empty()
            && value.len() <= Self::MAX_LEN
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-');
        if ok {
            Ok(Self(value.to_owned()))
        } else {
            Err(RunIdError {
                value: value.to_owned(),
            })
        }
    }

    /// Mint a fresh id: sortable by creation time, with a random tail against collisions.
    #[must_use]
    pub fn generate() -> Self {
        let stamp = Utc::now().format("%Y%m%dT%H%M%S");
        let tail = uuid::Uuid::new_v4().simple().to_string();
        Self(format!("{stamp}-{}", tail.get(..8).unwrap_or("00000000")))
    }

    /// The id as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for RunId {
    type Error = RunIdError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<RunId> for String {
    fn from(value: RunId) -> Self {
        value.0
    }
}

impl std::fmt::Display for RunId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
