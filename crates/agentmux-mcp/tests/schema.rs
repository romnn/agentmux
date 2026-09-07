//! The shape of the tool schemas a calling model has to fill in.
//!
//! The caller here is a language model that sees these schemas once, in a list of many other
//! tools, and fills one in without asking a follow-up question.
//! So the schemas are held to a standard: flat, every field described, nothing required that has a
//! sensible default.

use std::collections::BTreeSet;

use googletest::prelude::*;

/// The nine tools, and nothing else.
#[gtest]
fn the_server_offers_exactly_the_documented_tools() {
    let names: BTreeSet<String> = agentmux_mcp::tool_router()
        .list_all()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    let expected: BTreeSet<String> = [
        "ask",
        "start",
        "status",
        "tail",
        "result",
        "follow_up",
        "cancel",
        "list",
        "quota",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    assert_that!(names, eq(&expected));
}

/// Every tool argument is a flat, described property, and only the genuinely unguessable ones are
/// required.
#[gtest]
fn every_tool_argument_is_flat_and_described() {
    for tool in agentmux_mcp::tool_router().list_all() {
        let name = tool.name.to_string();
        let schema = &*tool.input_schema;

        assert_that!(
            schema.get("type").and_then(|t| t.as_str()),
            some(eq("object")),
            "{name} must take an object"
        );
        assert_that!(
            tool.description,
            some(anything()),
            "{name} must describe itself"
        );

        let properties = schema
            .get("properties")
            .and_then(|p| p.as_object())
            .cloned()
            .unwrap_or_default();
        for (field, spec) in &properties {
            let described = spec
                .get("description")
                .and_then(|d| d.as_str())
                .is_some_and(|d| d.len() > 20);
            assert_that!(
                described,
                eq(true),
                "{name}.{field} needs a real description"
            );
            // A nested object argument is one more thing for a caller to get wrong, and so is a
            // `$ref` — it moves the description into a sibling position that strict hosts drop,
            // and it is the shape a model fills in least reliably.
            // Asserting the type is present rather than merely "not object" is what stops this
            // passing vacuously for a property that has no `type` key at all.
            // A plain string, never an array: `["string", "null"]` tells a caller a field may
            // be null when what it actually means is that the field may be omitted, and
            // `#[serde(default)]` already says that by keeping it out of `required`.
            let kind = spec.get("type").and_then(|t| t.as_str());
            assert_that!(
                kind,
                some(anything()),
                "{name}.{field} must declare one plain type"
            );
            // A flat map of string to string is the one object shape allowed: it has no inner
            // struct for a caller to get wrong, and `env` has no faithful flat encoding — a list
            // of `KEY=VALUE` strings would just move the parsing into the caller.
            let flat_string_map = spec.get("properties").is_none()
                && spec
                    .get("additionalProperties")
                    .and_then(|a| a.get("type"))
                    .and_then(|t| t.as_str())
                    == Some("string");
            assert_that!(
                kind != Some("object") || flat_string_map,
                eq(true),
                "{name}.{field} must not be a nested object"
            );
            assert_that!(
                spec.get("$ref"),
                none(),
                "{name}.{field} must be inlined, not a $ref"
            );
        }
    }
}

/// Nothing a caller cannot know is optional, and nothing it can guess is required.
#[gtest]
fn the_required_arguments_are_the_unguessable_ones() {
    let required = |name: &str| -> BTreeSet<String> {
        agentmux_mcp::tool_router()
            .list_all()
            .into_iter()
            .find(|t| t.name == name)
            .and_then(|t| {
                t.input_schema
                    .get("required")
                    .and_then(|r| r.as_array())
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|v| v.as_str().map(str::to_owned))
                            .collect()
                    })
            })
            .unwrap_or_default()
    };
    let set =
        |names: &[&str]| -> BTreeSet<String> { names.iter().map(|s| (*s).to_owned()).collect() };

    // Who to ask, how hard to think, and what to ask.
    // Everything else has a sane default.
    assert_that!(
        required("ask"),
        eq(&set(&["delegate", "model", "effort", "question"]))
    );
    assert_that!(
        required("start"),
        eq(&set(&["delegate", "model", "effort", "question"]))
    );
    assert_that!(required("follow_up"), eq(&set(&["run_id", "question"])));
    for name in ["status", "tail", "result", "cancel"] {
        assert_that!(required(name), eq(&set(&["run_id"])), "{name}");
    }
    assert_that!(required("list"), is_empty());
}

/// No tool promises "the answer".
///
/// The whole design rests on the transcript being the deliverable.
/// A field or a phrase that names a single answer invites a caller to read it instead, which is
/// the failure this project exists to prevent, so the absence is asserted rather than assumed.
#[gtest]
fn no_tool_offers_a_single_answer_field() {
    for tool in agentmux_mcp::tool_router().list_all() {
        let name = tool.name.to_string();
        let schema = serde_json::to_string(&*tool.input_schema).unwrap_or_default();
        let description = tool.description.clone().unwrap_or_default().to_string();
        for forbidden in ["final_message", "\"answer\"", "\"summary\""] {
            assert_that!(
                schema.as_str(),
                not(contains_substring(forbidden)),
                "{name} schema"
            );
        }
        assert_that!(
            description.to_lowercase().as_str(),
            not(contains_substring("the final message")),
            "{name} description"
        );
    }
}

/// The whole schema is self-contained, with nothing hidden behind a definitions table.
#[gtest]
fn no_tool_schema_needs_a_definitions_table() {
    for tool in agentmux_mcp::tool_router().list_all() {
        assert_that!(
            tool.input_schema.get("$defs"),
            none(),
            "{} moved part of its schema into $defs",
            tool.name
        );
    }
}
