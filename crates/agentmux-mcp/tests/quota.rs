//! How an account's remaining quota reaches a calling model.
//!
//! The dangerous failure is not a wrong number but a missing one read as a good one: an account
//! that could not be asked must never look like an account with capacity to spare.

use agentmux::delegate::Vendor;
use agentmux::quota::{AccountQuota, Observation, Origin};
use googletest::prelude::*;

/// An account that could not be asked must never render as an idle one.
///
/// This is the failure that turns a typo into "always route to the account that cannot answer": an
/// unauthenticated Claude directory reports zero usage cheerfully, so anything that reads silence
/// as capacity picks the broken account every time.
#[gtest]
fn an_unavailable_account_never_reads_as_spare_capacity() {
    let entry = AccountQuota {
        vendor: Vendor::Claude,
        account: None,
        description: None,
        observation: Observation::Unavailable {
            reason: "not logged in".to_owned(),
        },
    };

    let rendered = agentmux_mcp::render::quota_report(&entry).join("\n");

    assert_that!(rendered, contains_substring("unavailable"));
    assert_that!(rendered, contains_substring("not idle"));
    // No percentage anywhere, because there is no measurement to report.
    assert_that!(rendered, not(contains_substring("%")));
}

/// agentmux states no opinion about which account is better placed to answer.
///
/// Ranking would need a table of what each plan tier is worth, which is the roster this crate
/// refuses to keep.
/// The words below are the ones that would appear if that rule were broken.
#[gtest]
fn nothing_rendered_recommends_an_account() {
    let entry = AccountQuota {
        vendor: Vendor::Claude,
        account: None,
        description: None,
        observation: Observation::Reported {
            origin: Origin::Live,
            payload: serde_json::json!({
                "utilization": {"limits": [
                    {"kind": "weekly_all", "percent": 3, "severity": "normal"}
                ]}
            }),
        },
    };

    let rendered = agentmux_mcp::render::quota_report(&entry)
        .join("\n")
        .to_lowercase();

    for verdict in ["best", "recommend", "use this", "most capacity", "prefer"] {
        assert_that!(
            rendered.contains(verdict),
            eq(false),
            "the rendering recommends an account with {verdict:?}"
        );
    }
}
