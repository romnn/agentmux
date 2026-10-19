//! Checks the mirror of a session rollout's usage windows (`codex_app_server_protocol::rollout`)
//! against codex's own types at the pinned release.
//!
//! The conversion below destructures codex's snapshot with every field named, so a field codex
//! adds or drops is a build error here, whether or not the mirror reads it.
//! A field the mirror copies is type-checked by the copy too; one it ignores, bound as `_`, is
//! not, so retyping that one compiles.
//! The test writes codex's own rollout lines and reads each back as the mirror, which checks the
//! names on the wire and that every line the mirror does not read still reads as something.

use codex_protocol::protocol as codex;
use mirror::rollout as ours;

/// codex's snapshot of one bucket, as the mirror reads it.
#[must_use]
pub fn snapshot(snapshot: codex::RateLimitSnapshot) -> ours::RateLimitSnapshot {
    let codex::RateLimitSnapshot {
        limit_id,
        limit_name: _,
        normal_model_slug: _,
        primary,
        secondary,
        credits: _,
        individual_limit: _,
        spend_control_reached: _,
        plan_type: _,
        rate_limit_reached_type: _,
    } = snapshot;
    ours::RateLimitSnapshot {
        limit_id,
        primary: primary.map(window),
        secondary: secondary.map(window),
    }
}

fn window(window: codex::RateLimitWindow) -> ours::RateLimitWindow {
    let codex::RateLimitWindow {
        used_percent,
        window_minutes,
        resets_at,
    } = window;
    ours::RateLimitWindow {
        used_percent,
        window_minutes,
        resets_at,
    }
}

#[cfg(test)]
mod tests {
    use codex_history::{RolloutItem, RolloutLine};
    use codex_protocol::protocol as codex;
    use googletest::prelude::*;
    use mirror::rollout as ours;

    /// Read one of codex's lines as the mirror.
    ///
    /// Through a `Value`, because codex's graph turns on serde_json's `arbitrary_precision`, under
    /// which a number a tagged enum buffers reaches an `f64` as a map; agentmux builds without it
    /// and reads the line directly.
    fn read(line: &RolloutLine) -> Result<ours::RolloutItem> {
        let written = serde_json::to_string(line).or_fail()?;
        let value = serde_json::from_str::<serde_json::Value>(&written).or_fail()?;
        serde_json::from_value(value).or_fail()
    }

    fn line(item: RolloutItem) -> RolloutLine {
        RolloutLine {
            timestamp: "2026-10-17T00:00:00.000Z".to_owned(),
            ordinal: Some(7),
            item,
        }
    }

    fn token_count(rate_limits: Option<codex::RateLimitSnapshot>) -> RolloutLine {
        line(RolloutItem::EventMsg(codex::EventMsg::TokenCount(
            codex::TokenCountEvent {
                info: None,
                rate_limits,
            },
        )))
    }

    /// A snapshot with both windows and every field set, and one with only the fields codex
    /// always writes.
    fn snapshots() -> Vec<codex::RateLimitSnapshot> {
        let window = |used_percent, minutes| codex::RateLimitWindow {
            used_percent,
            window_minutes: Some(minutes),
            resets_at: Some(1_792_669_569),
        };
        vec![
            codex::RateLimitSnapshot {
                limit_id: Some("codex".to_owned()),
                limit_name: Some("Codex".to_owned()),
                normal_model_slug: Some("gpt-5".to_owned()),
                primary: Some(window(28.5, 300)),
                secondary: Some(window(100.0, 10_080)),
                credits: Some(codex::CreditsSnapshot {
                    has_credits: true,
                    unlimited: false,
                    balance: Some("62399.6".to_owned()),
                }),
                individual_limit: None,
                spend_control_reached: Some(false),
                plan_type: Some(codex_protocol::account::PlanType::Plus),
                rate_limit_reached_type: Some(codex::RateLimitReachedType::RateLimitReached),
            },
            codex::RateLimitSnapshot {
                limit_id: None,
                limit_name: None,
                normal_model_slug: None,
                primary: Some(codex::RateLimitWindow {
                    used_percent: 3.0,
                    window_minutes: None,
                    resets_at: None,
                }),
                secondary: None,
                credits: None,
                individual_limit: None,
                spend_control_reached: None,
                plan_type: None,
                rate_limit_reached_type: None,
            },
        ]
    }

    /// The mirror reads the snapshot of every token count codex writes into a rollout, as the
    /// value it converts to.
    #[gtest]
    fn the_mirror_reads_the_usage_windows_codex_writes() -> Result<()> {
        let mut samples = vec![(token_count(None), None)];
        for snapshot in snapshots() {
            let mirrored = super::snapshot(snapshot.clone());
            samples.push((token_count(Some(snapshot)), Some(mirrored)));
        }
        for (sample, expected) in samples {
            expect_that!(
                read(&sample)?,
                eq(&ours::RolloutItem::EventMsg {
                    payload: ours::EventMsg::TokenCount(ours::TokenCountEvent {
                        rate_limits: expected
                    })
                })
            );
        }
        Ok(())
    }

    /// A line of any other kind, and an event of any other kind, reads as one the mirror passes
    /// over rather than failing.
    #[gtest]
    fn every_other_line_reads_as_passed_over() -> Result<()> {
        let message = line(RolloutItem::EventMsg(codex::EventMsg::AgentMessage(
            codex::AgentMessageEvent {
                message: "done".to_owned(),
                phase: None,
                memory_citation: None,
                delivery: None,
                questions: None,
            },
        )));
        let metadata = line(RolloutItem::InterAgentCommunicationMetadata { trigger_turn: true });

        expect_that!(
            read(&message)?,
            eq(&ours::RolloutItem::EventMsg {
                payload: ours::EventMsg::Other
            })
        );
        expect_that!(read(&metadata)?, eq(&ours::RolloutItem::Other));
        Ok(())
    }
}
