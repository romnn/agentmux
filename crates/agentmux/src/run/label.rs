//! What a caller says a consultation, or one of its turns, is for.

use serde::{Deserialize, Serialize};

/// A label was not of the accepted shape.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error(
    "invalid label {value:?}: a label is one line of 1-{max} characters with no control \
     characters or line breaks, such as \"REVIEW CRD G1\"",
    max = RunLabel::MAX_CHARS
)]
pub struct RunLabelError {
    /// What was supplied.
    pub value: String,
}

/// A short purpose a caller gives a consultation, or a follow-up gives its turn, so a caller
/// running many consultations can tell them apart without a registry of its own.
///
/// One line, because every listing prints it on one; trimmed, because a label that differs from
/// another only in whitespace would filter differently and print the same.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RunLabel(String);

impl RunLabel {
    /// Longest label accepted, in characters.
    pub const MAX_CHARS: usize = 120;

    /// Validate a label supplied by a caller, trimmed of surrounding whitespace.
    ///
    /// # Errors
    ///
    /// Returns [`RunLabelError`] for a label that is empty once trimmed, longer than
    /// [`RunLabel::MAX_CHARS`], or holds a control character or a line break of any kind.
    pub fn parse(value: &str) -> Result<Self, RunLabelError> {
        let trimmed = value.trim();
        let ok = !trimmed.is_empty()
            && trimmed.chars().count() <= Self::MAX_CHARS
            // Unicode's line and paragraph separators are not control characters, and would
            // break a listing's line as a newline does.
            && !trimmed
                .chars()
                .any(|c| c.is_control() || matches!(c, '\u{2028}' | '\u{2029}'));
        if ok {
            Ok(Self(trimmed.to_owned()))
        } else {
            Err(RunLabelError {
                value: value.to_owned(),
            })
        }
    }

    /// The label as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether `needle` occurs in the label, ignoring case.
    #[must_use]
    pub fn matches(&self, needle: &str) -> bool {
        self.0.to_lowercase().contains(&needle.to_lowercase())
    }
}

impl TryFrom<String> for RunLabel {
    type Error = RunLabelError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<RunLabel> for String {
    fn from(value: RunLabel) -> Self {
        value.0
    }
}

impl std::fmt::Display for RunLabel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use googletest::prelude::*;

    use super::RunLabel;

    /// A label is one trimmed line, and anything that would break a listing's layout is refused.
    #[gtest]
    fn a_label_is_one_trimmed_line() {
        assert_that!(
            RunLabel::parse("  REVIEW CRD G1 ").map(String::from),
            ok(eq("REVIEW CRD G1"))
        );
        for refused in [
            "",
            "   ",
            "two\nlines",
            "tab\there",
            "two\u{2028}lines",
            &"x".repeat(121),
        ] {
            assert_that!(RunLabel::parse(refused), err(anything()), "{refused:?}");
        }
        assert_that!(RunLabel::parse(&"é".repeat(120)), ok(anything()));
    }

    /// A filter matches any part of a label, whatever its case.
    #[gtest]
    fn a_label_matches_a_substring_ignoring_case() -> Result<()> {
        let label = RunLabel::parse("RE-REVIEW after rework").or_fail()?;
        assert_that!(label.matches("review AFTER"), eq(true));
        assert_that!(label.matches("crd"), eq(false));
        Ok(())
    }
}
