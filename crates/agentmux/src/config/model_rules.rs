//! What one vendor's model rules in a machine file do with the model a caller names.
//!
//! Three tables say it — `[models.<vendor>]` rewrites, `[efforts.<vendor>]` defaults and
//! `[blocked.<vendor>]` refusals — and [`ModelRules`] is the one reader of all three: the launch
//! resolves through it, every report lists through it, and the file is validated through it, so
//! what counts as a rewrite is decided in one place.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::ConfigError;
use crate::delegate::{Effort, ModelId, Vendor};

/// A model this machine refuses to launch, and the one to ask for instead.
///
/// Written either as the recommendation alone, `"gpt-5.6-sol" = "sol"`, or as a table that also
/// says why, `"gpt-6-sol" = { use = "sol", reason = "gpt-6.1-sol is much cheaper" }`.
/// Both spellings read into this one shape; only deserialising knows there are two.
///
/// Serialised back as the table, whichever spelling the file used, so the comparison that finds
/// [`Config::unknown`](super::Config::unknown) keys can check a written table key by key and
/// report a misspelled `reason`.
/// A written string is never compared against the table, so it is never reported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BlockedModel {
    /// The model to ask for instead, spelled as a caller would ask for it.
    ///
    /// It goes through the same file's rewrite like any request, so an alias such as `sol`
    /// recommends whatever that alias launches today.
    #[serde(rename = "use")]
    pub recommended: ModelId,
    /// Why the model is blocked, quoted in the refusal.
    ///
    /// One line of text when present: an empty reason, or one holding a newline or another
    /// control character, is refused when the file is read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The advice a refusal gives, `use model "sol" instead (why)`, in the one spelling every reader
/// of a block shows.
impl std::fmt::Display for BlockedModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "use model \"{}\" instead", self.recommended)?;
        match &self.reason {
            Some(reason) => write!(f, " ({reason})"),
            None => Ok(()),
        }
    }
}

/// Why a block's `reason` cannot be shown, if it cannot.
///
/// It is quoted inside a one-line refusal, so a reason that says nothing or that would break the
/// line is refused rather than shown.
fn check_block_reason(reason: &str) -> Result<(), &'static str> {
    if reason.trim().is_empty() {
        return Err("a blocked model's `reason` is empty; say why, or leave `reason` out");
    }
    if reason.chars().any(char::is_control) {
        return Err(
            "a blocked model's `reason` contains a newline or another control character; keep it \
             to one line",
        );
    }
    Ok(())
}

impl<'de> Deserialize<'de> for BlockedModel {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de;

        /// The table spelling, whose `use` is required and `reason` optional.
        #[derive(Deserialize)]
        struct Explained {
            #[serde(rename = "use")]
            recommended: ModelId,
            #[serde(default)]
            reason: Option<String>,
        }

        struct Visitor;

        impl<'de> de::Visitor<'de> for Visitor {
            type Value = BlockedModel;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(
                    "the model to use instead, or a table `{ use = \"<model>\", reason = \"<why>\" }`",
                )
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<BlockedModel, E> {
                Ok(BlockedModel {
                    recommended: ModelId::parse(value).map_err(E::custom)?,
                    reason: None,
                })
            }

            fn visit_map<A: de::MapAccess<'de>>(self, map: A) -> Result<BlockedModel, A::Error> {
                let Explained {
                    recommended,
                    reason,
                } = Explained::deserialize(de::value::MapAccessDeserializer::new(map))?;
                if let Some(reason) = &reason {
                    check_block_reason(reason).map_err(de::Error::custom)?;
                }
                Ok(BlockedModel {
                    recommended,
                    reason,
                })
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

/// One vendor's model rules, borrowed from a [`super::Config`].
///
/// Built by [`super::Config::model_rules`].
/// A vendor the file says nothing about has empty rules: every model runs as asked, at the CLI's
/// own default effort.
#[derive(Debug, Clone, Copy)]
pub struct ModelRules<'a> {
    pub(super) vendor: Vendor,
    pub(super) rewrites: Option<&'a BTreeMap<ModelId, ModelId>>,
    pub(super) efforts: Option<&'a BTreeMap<ModelId, Effort>>,
    pub(super) blocked: Option<&'a BTreeMap<ModelId, BlockedModel>>,
}

/// What a new consultation asking for a model launches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModel {
    /// The model to launch, after the machine's rewrite.
    pub launched: ModelId,
    /// What the caller asked for, when the rewrite changed it.
    pub rewritten_from: Option<ModelId>,
    /// The effort the machine names for [`Self::launched`], for a caller that names none.
    pub default_effort: Option<Effort>,
}

/// A model this machine refuses to launch, as a launch met it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefusedModel {
    /// The model that would have been launched.
    pub model: ModelId,
    /// What the machine file says about it.
    pub blocked: BlockedModel,
}

impl<'a> ModelRules<'a> {
    /// The vendor these rules are for.
    #[must_use]
    pub fn vendor(self) -> Vendor {
        self.vendor
    }

    /// Every rewrite that changes the model, as asked for and launched.
    ///
    /// A rule naming the same identifier on both sides asks for nothing, so it is not an alias and
    /// is left out here, in [`Self::resolve`] and in the checks the file is read under.
    pub fn aliases(self) -> impl Iterator<Item = (&'a ModelId, &'a ModelId)> {
        self.rewrites
            .into_iter()
            .flatten()
            .filter(|(from, to)| from != to)
    }

    /// The default efforts, by the model that runs.
    pub fn efforts(self) -> impl Iterator<Item = (&'a ModelId, &'a Effort)> {
        self.efforts.into_iter().flatten()
    }

    /// The blocked models, by the model that would run.
    pub fn blocked(self) -> impl Iterator<Item = (&'a ModelId, &'a BlockedModel)> {
        self.blocked.into_iter().flatten()
    }

    /// What a new consultation asking for `asked` launches, or why it may not.
    ///
    /// A model the file rewrites is exchanged once: an identifier the file says nothing about is
    /// untouched, so a model released this morning reaches the delegate CLI unchanged, and the
    /// rewritten one is never looked up again.
    /// The match is exact, because the delegate CLI's own match is.
    /// The default effort and the block are then looked up by the model that runs, the one
    /// identifier every consultation of it shares whatever name it asked by.
    ///
    /// Only a new consultation resolves its model.
    /// A follow-up keeps the model its consultation pinned at `start`, so a block added afterwards
    /// does not strand a consultation that is already paid for.
    ///
    /// # Errors
    ///
    /// Returns the [`RefusedModel`] when the model that would run is blocked; the recommendation
    /// is never launched in its place.
    pub fn resolve(self, asked: &ModelId) -> Result<ResolvedModel, RefusedModel> {
        let rewritten = self.rewritten(asked);
        let launched = rewritten.unwrap_or(asked);
        if let Some(blocked) = self.blocked_model(launched) {
            return Err(RefusedModel {
                model: launched.clone(),
                blocked: blocked.clone(),
            });
        }
        Ok(ResolvedModel {
            launched: launched.clone(),
            rewritten_from: rewritten.map(|_| asked.clone()),
            default_effort: self
                .efforts
                .and_then(|efforts| efforts.get(launched))
                .cloned(),
        })
    }

    /// What `model` is rewritten to, when a rule changes it.
    fn rewritten(self, model: &ModelId) -> Option<&'a ModelId> {
        self.rewrites?.get(model).filter(|to| *to != model)
    }

    fn blocked_model(self, model: &ModelId) -> Option<&'a BlockedModel> {
        self.blocked?.get(model)
    }

    /// Refuse rules that could not do what they read as.
    ///
    /// Each is refused when the file is read rather than resolved at launch, because either
    /// answer would be a guess at which of two lines the operator meant, made on every launch:
    ///
    /// - a rewrite whose target is rewritten again, because one substitution is all there is;
    /// - a rewrite onto a blocked model, which would refuse every request it matches;
    /// - a default effort or a block keyed by a model the file rewrites away, which could never
    ///   apply because both are looked up by the model that runs;
    /// - a recommendation that is itself blocked, which sends the caller to a second refusal.
    ///
    /// A recommendation the file rewrites launches the rewrite's target, which the second check
    /// has already found unblocked, so the last check reads the blocked models directly.
    /// Nothing is checked against a roster: whether the vendor still offers a model is the vendor
    /// CLI's to say.
    pub(super) fn validate(self, path: &Path) -> Result<(), ConfigError> {
        let vendor = self.vendor;
        for (from, to) in self.aliases() {
            if self.rewritten(to).is_some() {
                return Err(ConfigError::ChainedRewrite {
                    path: path.to_path_buf(),
                    vendor,
                    from: from.clone(),
                    to: to.clone(),
                });
            }
            if self.blocked_model(to).is_some() {
                return Err(ConfigError::RewriteToBlockedModel {
                    path: path.to_path_buf(),
                    vendor,
                    from: from.clone(),
                    to: to.clone(),
                });
            }
        }
        for (model, _) in self.efforts() {
            if let Some(to) = self.rewritten(model) {
                return Err(ConfigError::EffortForRewrittenModel {
                    path: path.to_path_buf(),
                    vendor,
                    model: model.clone(),
                    to: to.clone(),
                });
            }
        }
        for (model, block) in self.blocked() {
            if let Some(to) = self.rewritten(model) {
                return Err(ConfigError::BlockForRewrittenModel {
                    path: path.to_path_buf(),
                    vendor,
                    model: model.clone(),
                    to: to.clone(),
                });
            }
            if self.blocked_model(&block.recommended).is_some() {
                return Err(ConfigError::BlockedRecommendation {
                    path: path.to_path_buf(),
                    vendor,
                    model: model.clone(),
                    recommended: block.recommended.clone(),
                });
            }
        }
        Ok(())
    }
}
