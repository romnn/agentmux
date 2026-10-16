//! Turns codex's JSON Schema bundle into the OpenAPI 3.1 document progeny generates from.
//!
//! The bundle (`codex app-server generate-json-schema`) is draft-07: its `definitions` hold the
//! top-level types and, under `v2`, the v2 protocol's.
//! progeny reads `components/schemas` of an OpenAPI document, so the two are merged into one
//! namespace, where a v2 schema replaces a top-level one of the same name, and every reference is
//! rewritten to point there.
//! Only the schemas the roots reach are kept, so the generated module carries the types agentmux
//! exchanges rather than all 700 of the protocol's.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use crate::Error;

/// Where an OpenAPI document keeps its schemas, as a reference spells it.
const COMPONENTS: &str = "#/components/schemas/";

/// Every schema of a bundle, by name, with references rewritten to [`COMPONENTS`].
#[derive(Debug, Clone, PartialEq)]
pub struct Schemas(BTreeMap<String, Value>);

impl Schemas {
    /// Merge a bundle's top-level and v2 definitions, and give each titled branch of a union a
    /// schema of its own.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Bundle`] when the bundle has no `definitions` object.
    pub fn from_bundle(bundle: &Value) -> Result<Self, Error> {
        let Some(definitions) = bundle.get("definitions").and_then(Value::as_object) else {
            return Err(Error::Bundle("it has no `definitions` object".to_owned()));
        };
        let mut schemas = BTreeMap::new();
        let v2 = definitions.get("v2").and_then(Value::as_object);
        let top_level = definitions.iter().filter(|(name, _)| name.as_str() != "v2");
        for (name, schema) in top_level.chain(v2.into_iter().flatten()) {
            let mut schema = schema.clone();
            if let Some(members) = schema.as_object_mut() {
                members.remove("$schema");
            }
            rewrite_references(&mut schema);
            schemas.insert(name.clone(), schema);
        }
        let mut schemas = Self(schemas);
        schemas.hoist_titled_branches();
        Ok(schemas)
    }

    /// The schema called `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.0.get(name)
    }

    /// Give each titled branch of a union a schema of its own, named by its title, so the
    /// generated type is named for what it is (`AgentMessageThreadItem`) rather than for its
    /// position in the union (`ThreadItem2`), which would change meaning if codex reordered them.
    ///
    /// A title already taken by a different schema is left inline.
    fn hoist_titled_branches(&mut self) {
        let unions: Vec<String> = self.0.keys().cloned().collect();
        for union in unions {
            let branches = self
                .0
                .get(&union)
                .and_then(|schema| schema.get("oneOf"))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for (index, branch) in branches.into_iter().enumerate() {
                let Some(title) = branch.get("title").and_then(Value::as_str) else {
                    continue;
                };
                if !branch.is_object() || title == union {
                    continue;
                }
                if self.0.get(title).is_some_and(|taken| *taken != branch) {
                    continue;
                }
                let reference = json!({"$ref": format!("{COMPONENTS}{title}")});
                self.0.insert(title.to_owned(), branch.clone());
                if let Some(slot) = self
                    .0
                    .get_mut(&union)
                    .and_then(|schema| schema.get_mut("oneOf"))
                    .and_then(|branches| branches.get_mut(index))
                {
                    *slot = reference;
                }
            }
        }
    }

    /// The names of `roots` and of every schema they reach.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnknownSchema`] when a root, or a schema a root reaches, does not exist.
    pub fn closure<'a>(
        &self,
        roots: impl IntoIterator<Item = &'a str>,
    ) -> Result<BTreeSet<String>, Error> {
        let mut kept = BTreeSet::new();
        let mut pending: Vec<String> = roots.into_iter().map(ToOwned::to_owned).collect();
        while let Some(name) = pending.pop() {
            if kept.contains(&name) {
                continue;
            }
            let schema = self
                .0
                .get(&name)
                .ok_or_else(|| Error::UnknownSchema(name.clone()))?;
            pending.extend(references(schema));
            kept.insert(name);
        }
        Ok(kept)
    }

    /// The OpenAPI document holding exactly the schemas named in `kept`.
    #[must_use]
    pub fn document(&self, kept: &BTreeSet<String>, codex_version: &str) -> Value {
        let schemas: serde_json::Map<String, Value> = self
            .0
            .iter()
            .filter(|(name, _)| kept.contains(*name))
            .map(|(name, schema)| (name.clone(), schema.clone()))
            .collect();
        json!({
            "openapi": "3.1.0",
            "info": {"title": "codex app-server protocol", "version": codex_version},
            "paths": {},
            "components": {"schemas": schemas},
        })
    }
}

/// Point every reference in `value` at [`COMPONENTS`].
fn rewrite_references(value: &mut Value) {
    match value {
        Value::Object(members) => {
            for (key, member) in members.iter_mut() {
                if key == "$ref"
                    && let Value::String(target) = member
                {
                    let name = target
                        .strip_prefix("#/definitions/v2/")
                        .or_else(|| target.strip_prefix("#/definitions/"));
                    if let Some(name) = name {
                        *target = format!("{COMPONENTS}{name}");
                    }
                } else {
                    rewrite_references(member);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(rewrite_references),
        _ => {}
    }
}

/// The names of the schemas `value` references anywhere inside it.
fn references(value: &Value) -> Vec<String> {
    let mut found = Vec::new();
    let mut pending = vec![value];
    while let Some(value) = pending.pop() {
        match value {
            Value::Object(members) => {
                for (key, member) in members {
                    match member {
                        Value::String(target) if key == "$ref" => {
                            let name = target.strip_prefix(COMPONENTS).unwrap_or(target);
                            found.push(name.to_owned());
                        }
                        _ => pending.push(member),
                    }
                }
            }
            Value::Array(items) => pending.extend(items),
            _ => {}
        }
    }
    found
}

/// The method of every branch of the bundle's union `union`, sorted and without duplicates.
///
/// # Errors
///
/// Returns [`Error::Bundle`] when the bundle has no such union.
pub fn union_methods(bundle: &Value, union: &str) -> Result<Vec<String>, Error> {
    let Some(schema) = bundle.pointer(&format!("/definitions/{union}")) else {
        return Err(Error::Bundle(format!("it defines no `{union}`")));
    };
    let mut methods = BTreeSet::new();
    let mut pending = vec![schema];
    while let Some(value) = pending.pop() {
        match value {
            Value::Object(members) => {
                if let Some(names) = members
                    .get("method")
                    .and_then(|method| method.get("enum"))
                    .and_then(Value::as_array)
                {
                    methods.extend(
                        names
                            .iter()
                            .filter_map(Value::as_str)
                            .map(ToOwned::to_owned),
                    );
                }
                pending.extend(members.values());
            }
            Value::Array(items) => pending.extend(items),
            _ => {}
        }
    }
    Ok(methods.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use googletest::prelude::*;
    use serde_json::json;

    use super::{Schemas, union_methods};

    /// A v2 schema replaces a top-level one of the same name, and references to either
    /// namespace land in the merged one.
    #[gtest]
    fn the_two_namespaces_merge_with_v2_winning() -> Result<()> {
        let bundle = json!({"definitions": {
            "Shared": {"$schema": "x", "type": "string"},
            "Top": {"$ref": "#/definitions/v2/Shared"},
            "v2": {
                "Shared": {"type": "integer"},
                "Nested": {"properties": {"a": {"$ref": "#/definitions/Top"}}},
            },
        }});
        let schemas = Schemas::from_bundle(&bundle).or_fail()?;

        assert_that!(schemas.get("Shared"), some(eq(&json!({"type": "integer"}))));
        assert_that!(
            schemas.get("Top"),
            some(eq(&json!({"$ref": "#/components/schemas/Shared"})))
        );
        assert_that!(
            schemas.closure(["Nested"]).or_fail()?,
            unordered_elements_are![eq("Nested"), eq("Top"), eq("Shared")]
        );
        Ok(())
    }

    /// A titled branch becomes a schema of its own unless its title names a different one.
    #[gtest]
    fn titled_branches_are_hoisted_unless_their_title_is_taken() -> Result<()> {
        let bundle = json!({"definitions": {
            "Taken": {"type": "boolean"},
            "Union": {"oneOf": [
                {"title": "Fresh", "type": "string"},
                {"title": "Taken", "type": "integer"},
                {"type": "null"},
            ]},
        }});
        let schemas = Schemas::from_bundle(&bundle).or_fail()?;

        assert_that!(
            schemas.get("Union"),
            some(eq(&json!({"oneOf": [
                {"$ref": "#/components/schemas/Fresh"},
                {"title": "Taken", "type": "integer"},
                {"type": "null"},
            ]})))
        );
        assert_that!(
            schemas.get("Fresh"),
            some(eq(&json!({"title": "Fresh", "type": "string"})))
        );
        Ok(())
    }

    /// A root that names no schema is an error rather than a silently smaller module.
    #[gtest]
    fn an_unknown_root_is_refused() -> Result<()> {
        let schemas = Schemas::from_bundle(&json!({"definitions": {}})).or_fail()?;
        assert_that!(schemas.closure(["Missing"]), err(anything()));
        Ok(())
    }

    /// Every branch's method is listed once, in order.
    #[gtest]
    fn a_unions_methods_are_listed_once_in_order() -> Result<()> {
        let bundle = json!({"definitions": {"ServerNotification": {"oneOf": [
            {"properties": {"method": {"enum": ["turn/started"]}}},
            {"properties": {"method": {"enum": ["error"]}}},
            {"properties": {"method": {"enum": ["turn/started"]}}},
        ]}}});
        assert_that!(
            union_methods(&bundle, "ServerNotification").or_fail()?,
            elements_are![eq("error"), eq("turn/started")]
        );
        Ok(())
    }
}
