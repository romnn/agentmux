//! Every method of the protocol, as codex's bundle lists it: [`Method`].
//!
//! The bundle's four message unions (`ClientRequest`, `ClientNotification`, `ServerRequest`,
//! `ServerNotification`) name each method and the schema of its `params`.
//! They do not name what answers a request, which comes from codex's own request definitions,
//! vendored as [`Responses`]; the two are checked against each other method for method, so a
//! request codex added, renamed or dropped stops generation until both are refreshed together.
//! Nothing here is chosen by hand: every method the bundle lists is bound.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::Error;
use crate::openapi::Schemas;
use crate::responses::{Responses, Side};

/// Who sends a message, and whether it is answered: one of the bundle's message unions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Direction {
    /// A request the client sends.
    ClientRequest,
    /// A notification the client sends.
    ClientNotification,
    /// A request the server sends.
    ServerRequest,
    /// A notification the server sends.
    ServerNotification,
}

impl Direction {
    /// Every direction, in the order the generated module lists them.
    pub const ALL: [Self; 4] = [
        Self::ClientRequest,
        Self::ClientNotification,
        Self::ServerRequest,
        Self::ServerNotification,
    ];

    /// The bundle's union of every message in this direction, which is also the name of the
    /// protocol crate's trait for it.
    #[must_use]
    pub fn union(self) -> &'static str {
        match self {
            Self::ClientRequest => "ClientRequest",
            Self::ClientNotification => "ClientNotification",
            Self::ServerRequest => "ServerRequest",
            Self::ServerNotification => "ServerNotification",
        }
    }

    /// Who sends it, for a request, which is answered.
    #[must_use]
    pub fn request_sender(self) -> Option<Side> {
        match self {
            Self::ClientRequest => Some(Side::Client),
            Self::ServerRequest => Some(Side::Server),
            Self::ClientNotification | Self::ServerNotification => None,
        }
    }
}

/// What a message's `params` carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Params {
    /// Nothing: the member is absent or `null`.
    None,
    /// Always this schema.
    Required(String),
    /// This schema, or nothing.
    Optional(String),
}

impl Params {
    /// The schema it carries, if any.
    #[must_use]
    pub fn schema(&self) -> Option<&str> {
        match self {
            Self::None => None,
            Self::Required(schema) | Self::Optional(schema) => Some(schema),
        }
    }
}

/// One method, bound to its payload types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Method {
    /// Who sends it.
    pub direction: Direction,
    /// Its name on the wire.
    pub wire: String,
    /// The Rust type that stands for it, unique across every direction.
    pub name: String,
    /// What its `params` carry.
    pub params: Params,
    /// The schema of its `result`, for a request.
    pub response: Option<String>,
}

/// Every method the bundle lists, ordered by direction and then by wire name.
///
/// # Errors
///
/// Returns [`Error::Binding`] for a method whose `params` have a shape this generator does not
/// bind, a request [`Responses`] gives no response or a response codex's schema does not define,
/// a response [`Responses`] declares for a request the bundle does not list, two methods that
/// would share a type name, or a union branch that names no single method; and
/// [`Error::Bundle`] for a missing union.
pub fn read(
    bundle: &Value,
    responses: &Responses,
    schemas: &Schemas,
) -> Result<Vec<Method>, Error> {
    let mut methods = Vec::new();
    let mut names = BTreeSet::new();
    for direction in Direction::ALL {
        let union = direction.union();
        let mut listed = BTreeMap::new();
        for branch in branches(bundle, union)? {
            let wire = branch_method(branch).ok_or_else(|| {
                Error::Bundle(format!("a branch of `{union}` names no single method"))
            })?;
            if listed.insert(wire.to_owned(), branch).is_some() {
                return Err(Error::Bundle(format!("`{union}` lists `{wire}` twice")));
            }
        }
        let declared = direction
            .request_sender()
            .map(|side| responses.sent_by(side));
        if let Some(declared) = declared
            && let Some(stale) = declared.keys().find(|method| !listed.contains_key(*method))
        {
            return Err(Error::Binding {
                method: stale.clone(),
                reason: format!(
                    "codex's request definitions answer it, but codex's `{union}` does not list \
                     it: refresh the bundle and the definitions together"
                ),
            });
        }
        for (wire, branch) in listed {
            let refuse = |reason: String| Error::Binding {
                method: wire.clone(),
                reason,
            };
            let params = params(branch).map_err(refuse)?;
            let response = match declared {
                None => None,
                Some(declared) => {
                    let response = declared.get(&wire).ok_or_else(|| {
                        refuse("codex's request definitions give it no response".to_owned())
                    })?;
                    if schemas.get(response).is_none() {
                        return Err(refuse(format!(
                            "codex's schema defines no response type `{response}`"
                        )));
                    }
                    Some(response.clone())
                }
            };
            let name = type_name(&wire).map_err(refuse)?;
            if !names.insert(name.clone()) {
                return Err(refuse(format!("another method is also named `{name}`")));
            }
            methods.push(Method {
                direction,
                wire,
                name,
                params,
                response,
            });
        }
    }
    Ok(methods)
}

/// Every schema a method carries, which is what the generated types are grown from.
#[must_use]
pub fn roots(methods: &[Method]) -> BTreeSet<&str> {
    methods
        .iter()
        .flat_map(|method| [method.params.schema(), method.response.as_deref()])
        .flatten()
        .collect()
}

/// The methods of every request the bundle lists as sent by `side`.
///
/// # Errors
///
/// Returns [`Error::Bundle`] when the bundle lacks the union.
pub fn requests_sent_by(bundle: &Value, side: Side) -> Result<BTreeSet<&str>, Error> {
    let union = match side {
        Side::Client => Direction::ClientRequest,
        Side::Server => Direction::ServerRequest,
    }
    .union();
    Ok(branches(bundle, union)?.filter_map(branch_method).collect())
}

/// The branches of one of the bundle's message unions.
fn branches<'a>(bundle: &'a Value, union: &str) -> Result<impl Iterator<Item = &'a Value>, Error> {
    bundle
        .pointer(&format!("/definitions/{union}/oneOf"))
        .and_then(Value::as_array)
        .map(|branches| branches.iter())
        .ok_or_else(|| Error::Bundle(format!("it defines no `{union}` union")))
}

/// The single method a union branch carries.
fn branch_method(branch: &Value) -> Option<&str> {
    match branch
        .pointer("/properties/method/enum")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
    {
        Some([method]) => method.as_str(),
        _ => None,
    }
}

/// What a union branch's `params` carry, from the shapes codex's schema writes:
///
/// - no `params` member, or `{"type": "null"}` that is not required: nothing;
/// - `{"$ref": …}`, required: that schema;
/// - `{"anyOf": [{"$ref": …}, {"type": "null"}]}`, not required: that schema, or nothing.
///
/// Any other shape is refused rather than bound to something looser.
fn params(branch: &Value) -> Result<Params, String> {
    let required = branch
        .get("required")
        .and_then(Value::as_array)
        .is_some_and(|required| required.iter().any(|member| member == "params"));
    let Some(schema) = branch.pointer("/properties/params") else {
        return Ok(Params::None);
    };
    let null = serde_json::json!({"type": "null"});
    let reference = |schema: &Value| {
        let Value::Object(members) = schema else {
            return None;
        };
        match members.get("$ref") {
            Some(Value::String(target)) if members.len() == 1 => {
                target.rsplit('/').next().map(ToOwned::to_owned)
            }
            _ => None,
        }
    };
    let alternatives = schema
        .get("anyOf")
        .and_then(Value::as_array)
        .filter(|_| schema.as_object().is_some_and(|members| members.len() == 1))
        .map(Vec::as_slice);
    match (required, reference(schema), alternatives) {
        (true, Some(name), _) => Ok(Params::Required(name)),
        (false, None, _) if *schema == null => Ok(Params::None),
        (false, None, Some([some, none])) if *none == null => reference(some)
            .map(Params::Optional)
            .ok_or_else(|| format!("its optional params are not one schema: {schema}")),
        _ => Err(format!(
            "its params are {} `{schema}`, a shape this generator does not bind",
            if required {
                "a required"
            } else {
                "an optional"
            }
        )),
    }
}

/// The Rust type that stands for a method: each segment between `/`, `_` and `-` capitalised and
/// joined, so `item/commandExecution/requestApproval` is `ItemCommandExecutionRequestApproval` and
/// `thread/inject_items` is `ThreadInjectItems`.
fn type_name(method: &str) -> Result<String, String> {
    let name: String = method
        .split(['/', '_', '-'])
        .map(|segment| {
            let mut chars = segment.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_ascii_uppercase().to_string() + chars.as_str()
            })
        })
        .collect();
    let identifier = name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && name.chars().all(|c| c.is_ascii_alphanumeric());
    if !identifier {
        return Err(format!("`{name}` is not a Rust type name"));
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use googletest::prelude::*;
    use serde_json::{Value, json};

    use super::{Direction, Method, Params, read, roots, type_name};
    use crate::openapi::Schemas;
    use crate::responses::Responses;

    fn bundle(client_requests: &[Value], server_notifications: &[Value]) -> Value {
        json!({"definitions": {
            "ClientRequest": {"oneOf": client_requests},
            "ClientNotification": {"oneOf": []},
            "ServerRequest": {"oneOf": []},
            "ServerNotification": {"oneOf": server_notifications},
            "v2": {
                "TurnStartParams": {}, "TurnStartResponse": {},
                "GetAccountRateLimitsParams": {}, "GetAccountRateLimitsResponse": {},
                "LogoutAccountResponse": {}, "TurnStartedNotification": {},
            },
        }})
    }

    fn branch(method: &str, params: Option<Value>, required: bool) -> Value {
        let mut properties = json!({"id": {}, "method": {"enum": [method]}});
        let mut members = vec!["id", "method"];
        if let Some(params) = params {
            properties["params"] = params;
            if required {
                members.push("params");
            }
        }
        json!({"properties": properties, "required": members})
    }

    fn reference(name: &str) -> Value {
        json!({"$ref": format!("#/definitions/v2/{name}")})
    }

    fn responses(client: &[(&str, &str)]) -> Responses {
        Responses {
            client: client
                .iter()
                .map(|(method, response)| ((*method).to_owned(), (*response).to_owned()))
                .collect(),
            server: BTreeMap::new(),
        }
    }

    fn read_bundle(
        bundle: &Value,
        responses: &Responses,
    ) -> std::result::Result<Vec<Method>, crate::Error> {
        let schemas =
            Schemas::from_bundle(bundle).map_err(|err| crate::Error::Bundle(err.to_string()))?;
        read(bundle, responses, &schemas)
    }

    /// Each params shape codex writes is bound, a request takes the response codex's definitions
    /// give it, and every payload is a root.
    #[gtest]
    fn every_method_is_bound_to_its_payloads() -> Result<()> {
        let bundle = bundle(
            &[
                branch("turn/start", Some(reference("TurnStartParams")), true),
                branch("account/logout", Some(json!({"type": "null"})), false),
                branch(
                    "account/rateLimits/read",
                    Some(
                        json!({"anyOf": [reference("GetAccountRateLimitsParams"), {"type": "null"}]}),
                    ),
                    false,
                ),
            ],
            &[branch(
                "turn/started",
                Some(reference("TurnStartedNotification")),
                true,
            )],
        );
        let responses = responses(&[
            ("turn/start", "TurnStartResponse"),
            ("account/logout", "LogoutAccountResponse"),
            ("account/rateLimits/read", "GetAccountRateLimitsResponse"),
        ]);
        let methods = read_bundle(&bundle, &responses).or_fail()?;

        assert_that!(
            methods,
            elements_are![
                matches_pattern!(Method {
                    direction: eq(&Direction::ClientRequest),
                    name: eq("AccountLogout"),
                    params: eq(&Params::None),
                    response: some(eq("LogoutAccountResponse")),
                    ..
                }),
                matches_pattern!(Method {
                    name: eq("AccountRateLimitsRead"),
                    params: eq(&Params::Optional("GetAccountRateLimitsParams".to_owned())),
                    ..
                }),
                matches_pattern!(Method {
                    name: eq("TurnStart"),
                    params: eq(&Params::Required("TurnStartParams".to_owned())),
                    response: some(eq("TurnStartResponse")),
                    ..
                }),
                matches_pattern!(Method {
                    direction: eq(&Direction::ServerNotification),
                    name: eq("TurnStarted"),
                    response: none(),
                    ..
                }),
            ]
        );
        assert_that!(roots(&methods).len(), eq(6));
        Ok(())
    }

    /// A request without a declared response, a response declared for a request the bundle
    /// does not list, a response codex's schema does not define, and params of an unknown shape
    /// each stop generation.
    #[gtest]
    fn a_method_the_inputs_disagree_on_is_refused() {
        let turn_start = || branch("turn/start", Some(reference("TurnStartParams")), true);
        let cases = [
            (vec![turn_start()], responses(&[]), "give it no response"),
            (
                vec![turn_start()],
                responses(&[
                    ("turn/start", "TurnStartResponse"),
                    ("turn/begin", "TurnStartResponse"),
                ]),
                "does not list it",
            ),
            (
                vec![turn_start()],
                responses(&[("turn/start", "TurnBeginResponse")]),
                "no response type",
            ),
            (
                vec![branch("turn/start", Some(json!({"type": "object"})), true)],
                responses(&[("turn/start", "TurnStartResponse")]),
                "does not bind",
            ),
            (
                vec![branch(
                    "turn/start",
                    Some(reference("TurnStartParams")),
                    false,
                )],
                responses(&[("turn/start", "TurnStartResponse")]),
                "an optional",
            ),
        ];
        for (requests, responses, reason) in cases {
            assert_that!(
                read_bundle(&bundle(&requests, &[]), &responses),
                err(displays_as(contains_substring(reason)))
            );
        }
    }

    /// Two methods that would share a type name are refused.
    #[gtest]
    fn two_methods_with_one_name_are_refused() {
        let started =
            |method: &str| branch(method, Some(reference("TurnStartedNotification")), true);
        assert_that!(
            read_bundle(
                &bundle(&[], &[started("turn/started"), started("turnStarted")]),
                &responses(&[])
            ),
            err(displays_as(contains_substring("also named")))
        );
    }

    /// A method's segments become one type name.
    #[gtest]
    fn a_method_is_named_by_its_segments() -> Result<()> {
        assert_that!(
            type_name("item/commandExecution/requestApproval").or_fail()?,
            eq("ItemCommandExecutionRequestApproval")
        );
        assert_that!(type_name("error").or_fail()?, eq("Error"));
        assert_that!(
            type_name("thread/inject_items").or_fail()?,
            eq("ThreadInjectItems")
        );
        assert_that!(type_name("turn/a.b"), err(anything()));
        Ok(())
    }
}
