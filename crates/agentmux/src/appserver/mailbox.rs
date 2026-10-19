//! The files a steered message travels through, between whoever steers and the driver.
//!
//! One message is one file name stem, and its suffix is its part:
//!
//! ```text
//! <stem>.request.json   written by the caller, whole, by renaming a temporary file into place
//! <stem>.decision.json  claimed by the driver, which then sends `turn/steer`, or withdrawn by a
//!                       caller that stopped waiting; created exactly once
//! <stem>.reply.json     written by the driver, whole, once the server has answered
//! ```
//!
//! Claiming and withdrawing both create the one decision file, so exactly one of them succeeds:
//! a message the caller withdrew is never sent, and a message the driver claimed is answered as
//! accepted, rejected, or, when it was sent and the server never said, unconfirmed.
//! Stems sort by the time they were submitted, so the driver sends them in that order.
//!
//! The decision is created with [`crate::once::create_once`], which says why a rename could not
//! arbitrate it.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

const REQUEST: &str = "request.json";
const DECISION: &str = "decision.json";
const REPLY: &str = "reply.json";

/// A turn's mailbox directory.
#[derive(Debug, Clone)]
pub struct Mailbox {
    dir: PathBuf,
}

/// One submitted message, as its sender tracks it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Letter {
    stem: String,
}

/// One message the driver has claimed and must answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    stem: String,
    /// The instruction to deliver.
    pub text: String,
}

/// What the driver says about a message it claimed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Reply {
    /// The server accepted it into the running turn.
    Accepted,
    /// The server, or the driver, refused it; it was not delivered.
    Rejected {
        /// Why, in the server's words when it gave any.
        reason: String,
    },
    /// It was sent, but whether the server took it is unknown: the turn or the server ended
    /// before the server answered, or the server took it and the transcript could not record it.
    Unconfirmed {
        /// What happened instead of an answer.
        reason: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct Request {
    text: String,
}

/// Who took a message out of the request state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Decision {
    /// The driver, which sends it.
    Claimed,
    /// Its sender, which stopped waiting; it is never sent.
    Withdrawn,
}

impl Mailbox {
    /// The mailbox in `dir`, which need not exist yet.
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn path(&self, stem: &str, state: &str) -> PathBuf {
        self.dir.join(format!("{stem}.{state}"))
    }

    /// Submit an instruction.
    ///
    /// # Errors
    ///
    /// Returns an error when the mailbox cannot be created or written.
    pub fn submit(&self, text: &str) -> std::io::Result<Letter> {
        std::fs::create_dir_all(&self.dir)?;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let tail = uuid::Uuid::new_v4().simple().to_string();
        let stem = format!("{nanos:024}-{}", tail.get(..8).unwrap_or("00000000"));
        let body = serde_json::to_string(&Request {
            text: text.to_owned(),
        })
        .map_err(std::io::Error::other)?;
        // Only its sender writes a request, and only the driver a reply, so neither rename has
        // anything to arbitrate.
        crate::once::write_whole(&self.path(&stem, REQUEST), body.as_bytes())?;
        Ok(Letter { stem })
    }

    /// Whether the driver has taken this letter.
    ///
    /// A decision that cannot be read counts as not claimed: it is created whole, so only a
    /// mailbox damaged from outside has one.
    #[must_use]
    pub fn is_claimed(&self, letter: &Letter) -> bool {
        self.decision(&letter.stem) == Some(Decision::Claimed)
    }

    fn decision(&self, stem: &str) -> Option<Decision> {
        let text = std::fs::read_to_string(self.path(stem, DECISION)).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Make the one decision about a message.
    ///
    /// Returns `false` when it was already made, by anyone.
    fn decide(&self, stem: &str, decision: Decision) -> std::io::Result<bool> {
        let body = serde_json::to_string(&decision).map_err(std::io::Error::other)?;
        crate::once::create_once(&self.path(stem, DECISION), body.as_bytes())
    }

    /// The driver's reply to a letter, once there is one.
    ///
    /// # Errors
    ///
    /// Returns an error when a reply exists but cannot be read.
    pub fn reply(&self, letter: &Letter) -> std::io::Result<Option<Reply>> {
        match std::fs::read_to_string(self.path(&letter.stem, REPLY)) {
            Ok(text) => serde_json::from_str(&text)
                .map(Some)
                .map_err(std::io::Error::other),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Take a letter back before the driver claims it.
    ///
    /// Returns `false` when the driver claimed it first, in which case it will be answered.
    ///
    /// # Errors
    ///
    /// Returns an error when the decision cannot be written.
    pub fn withdraw(&self, letter: &Letter) -> std::io::Result<bool> {
        self.decide(&letter.stem, Decision::Withdrawn)
    }

    /// Claim every waiting request, oldest first.
    ///
    /// A request that cannot be read after it was claimed is answered as rejected on the spot,
    /// so its sender is not left waiting on a message nobody will send.
    #[must_use]
    pub fn claim_all(&self) -> Vec<Claim> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let suffix = format!(".{REQUEST}");
        let mut stems: Vec<String> = entries
            .flatten()
            .filter_map(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .and_then(|name| name.strip_suffix(&suffix))
                    .map(ToOwned::to_owned)
            })
            .collect();
        stems.sort_unstable();

        let mut claims = Vec::new();
        for stem in stems {
            // Decided already, by an earlier claim or by its sender.
            if self.path(&stem, DECISION).exists() {
                continue;
            }
            match self.decide(&stem, Decision::Claimed) {
                Ok(true) => {}
                Ok(false) => continue,
                // Left waiting, for a later claim or for its sender to withdraw.
                Err(err) => {
                    tracing::warn!(?err, %stem, "cannot claim a steer");
                    continue;
                }
            }
            let request = std::fs::read_to_string(self.path(&stem, REQUEST))
                .map_err(|error| error.to_string())
                .and_then(|text| {
                    serde_json::from_str::<Request>(&text).map_err(|error| error.to_string())
                });
            match request {
                Ok(request) => claims.push(Claim {
                    stem,
                    text: request.text,
                }),
                // Its sender waits for a reply until it gives up on the driver, so one that
                // cannot be written leaves it waiting that long, and says why here.
                Err(reason) => {
                    let rejected = Reply::Rejected {
                        reason: format!("the request could not be read: {reason}"),
                    };
                    self.answer_stem(&stem, &rejected)
                        .inspect_err(|err| tracing::warn!(?err, %stem, "cannot reject a steer"))
                        .ok();
                }
            }
        }
        claims
    }

    /// Answer a claimed message.
    ///
    /// # Errors
    ///
    /// Returns an error when the reply cannot be written.
    pub fn answer(&self, claim: &Claim, reply: &Reply) -> std::io::Result<()> {
        self.answer_stem(&claim.stem, reply)
    }

    fn answer_stem(&self, stem: &str, reply: &Reply) -> std::io::Result<()> {
        let body = serde_json::to_string(reply).map_err(std::io::Error::other)?;
        crate::once::write_whole(&self.path(stem, REPLY), body.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use googletest::prelude::*;

    use super::{Mailbox, Reply};

    /// A message travels request, claim, reply, and its sender reads the reply back.
    #[gtest]
    fn a_claimed_message_is_answered() -> Result<()> {
        let dir = tempfile::tempdir().or_fail()?;
        let mailbox = Mailbox::new(dir.path().join("steer"));

        let letter = mailbox.submit("narrow it to auth").or_fail()?;
        assert_that!(mailbox.is_claimed(&letter), eq(false));

        let claims = mailbox.claim_all();
        assert_that!(claims.len(), eq(1));
        let claim = claims.first().or_fail()?;
        assert_that!(claim.text, eq("narrow it to auth"));
        assert_that!(mailbox.is_claimed(&letter), eq(true));

        mailbox.answer(claim, &Reply::Accepted).or_fail()?;
        assert_that!(
            mailbox.reply(&letter).or_fail()?,
            some(eq(&Reply::Accepted))
        );
        Ok(())
    }

    /// Withdrawing and claiming race to create one decision file, so only one of them can win.
    #[gtest]
    fn a_withdrawn_message_is_never_claimed_and_a_claimed_one_cannot_be_withdrawn() -> Result<()> {
        let dir = tempfile::tempdir().or_fail()?;
        let mailbox = Mailbox::new(dir.path());

        // Withdrawn first: the driver finds nothing to send.
        let withdrawn = mailbox.submit("first").or_fail()?;
        assert_that!(mailbox.withdraw(&withdrawn).or_fail()?, eq(true));
        assert_that!(mailbox.claim_all(), is_empty());
        assert_that!(mailbox.is_claimed(&withdrawn), eq(false));

        // Claimed first: the sender learns it is too late to take it back.
        let claimed = mailbox.submit("second").or_fail()?;
        assert_that!(mailbox.claim_all().len(), eq(1));
        assert_that!(mailbox.withdraw(&claimed).or_fail()?, eq(false));
        Ok(())
    }

    /// Two claimers and a withdrawal racing on one message: in every round exactly one of them
    /// wins, and nothing is left for a later claim.
    #[gtest]
    fn racing_claims_and_a_withdrawal_have_exactly_one_winner() -> Result<()> {
        use std::sync::{Arc, Barrier};

        let dir = tempfile::tempdir().or_fail()?;
        for round in 0..200 {
            let path = dir.path().join(round.to_string());
            let letter = Mailbox::new(&path).submit("racing").or_fail()?;
            let start = Arc::new(Barrier::new(3));
            let claimer = || {
                let (mailbox, start) = (Mailbox::new(&path), Arc::clone(&start));
                std::thread::spawn(move || {
                    start.wait();
                    mailbox.claim_all().len()
                })
            };
            let (first, second) = (claimer(), claimer());
            let withdrawer = {
                let (mailbox, start, letter) =
                    (Mailbox::new(&path), Arc::clone(&start), letter.clone());
                std::thread::spawn(move || {
                    start.wait();
                    mailbox.withdraw(&letter)
                })
            };

            let claims = first.join().ok().or_fail()? + second.join().ok().or_fail()?;
            let withdrawn = withdrawer.join().ok().or_fail()?.or_fail()?;
            assert_that!(
                (withdrawn, claims),
                any![eq((true, 0)), eq((false, 1))],
                "round {round}"
            );
            assert_that!(Mailbox::new(&path).claim_all(), is_empty());
        }
        Ok(())
    }

    /// Several waiting messages are claimed in the order they were sent.
    #[gtest]
    fn messages_are_claimed_oldest_first() -> Result<()> {
        let dir = tempfile::tempdir().or_fail()?;
        let mailbox = Mailbox::new(dir.path());
        for text in ["one", "two", "three"] {
            mailbox.submit(text).or_fail()?;
        }

        let texts: Vec<String> = mailbox
            .claim_all()
            .into_iter()
            .map(|claim| claim.text)
            .collect();
        assert_that!(texts, elements_are![eq("one"), eq("two"), eq("three")]);
        Ok(())
    }
}
