//! Every method of the protocol, as codex's bundle lists it: [`Method`].
//!
//! The bundle's four message unions (`ClientRequest`, `ClientNotification`, `ServerRequest`,
//! `ServerNotification`) name each method and the schema of its `params`, and every method they
//! list is bound by name.
//! The methods of the [`Surface`] are bound to their types too, and each of its entries is
//! checked here against the bundle.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::Error;
use crate::openapi::Schemas;
use crate::surface::{Carries, Entry, Surface};

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

    /// Whether its messages are requests, which are answered.
    #[must_use]
    pub fn is_request(self) -> bool {
        matches!(self, Self::ClientRequest | Self::ServerRequest)
    }

    /// The [`Surface`]'s entries for this direction.
    #[must_use]
    pub fn entries(self, surface: &Surface) -> &[Entry] {
        match self {
            Self::ClientRequest => &surface.client_requests,
            Self::ClientNotification => &surface.client_notifications,
            Self::ServerRequest => &surface.server_requests,
            Self::ServerNotification => &surface.server_notifications,
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

/// One method, bound to its name and, when agentmux uses it, to its payload types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Method {
    /// Who sends it.
    pub direction: Direction,
    /// Its name on the wire.
    pub wire: String,
    /// The Rust type that stands for it, unique across every direction.
    pub name: String,
    /// Its payload types, for a method of the [`Surface`].
    pub typed: Option<Typed>,
}

/// The payload types of a method of the [`Surface`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Typed {
    /// What its `params` carry.
    pub params: Params,
    /// The schema of its `result`, for a request.
    pub response: Option<String>,
}

/// Every method the bundle lists, ordered by direction and then by wire name.
///
/// # Errors
///
/// Returns [`Error::Binding`] for a [`Surface`] entry the bundle does not bear out, or two
/// methods that would share a type name; and [`Error::Bundle`] for a missing union or a branch
/// that names no single method.
pub fn read(bundle: &Value, surface: &Surface, schemas: &Schemas) -> Result<Vec<Method>, Error> {
    let mut methods = Vec::new();
    let mut names = BTreeSet::new();
    for direction in Direction::ALL {
        let union = direction.union();
        let mut listed = BTreeMap::new();
        for branch in branches(bundle, union)? {
            let wire = branch_method(branch).ok_or_else(|| {
                Error::Bundle(format!("a branch of `{union}` names no single method"))
            })?;
            if listed.insert(wire.to_owned(), params(branch)).is_some() {
                return Err(Error::Bundle(format!("`{union}` lists `{wire}` twice")));
            }
        }
        let mut typed = typed(direction, direction.entries(surface), &listed, schemas)?;
        for wire in listed.into_keys() {
            let name = type_name(&wire).map_err(|reason| Error::Binding {
                method: wire.clone(),
                reason,
            })?;
            if !names.insert(name.clone()) {
                return Err(Error::Binding {
                    method: wire,
                    reason: format!("another method is also named `{name}`"),
                });
            }
            methods.push(Method {
                typed: typed.remove(wire.as_str()),
                direction,
                wire,
                name,
            });
        }
    }
    Ok(methods)
}

/// The payload types of each [`Surface`] entry of one direction, by method, once the bundle bears
/// the entry out.
///
/// An entry is borne out when the direction's union lists its method with exactly its params, no
/// other method of the union carries those params, and a request's response is a schema codex
/// defines while a notification has none.
/// The uniqueness is what lets the oracle, which knows codex's methods by their payloads and not
/// by their wire names, vouch for the method.
fn typed(
    direction: Direction,
    entries: &[Entry],
    listed: &BTreeMap<String, Result<Params, String>>,
    schemas: &Schemas,
) -> Result<BTreeMap<&'static str, Typed>, Error> {
    let union = direction.union();
    let mut typed = BTreeMap::new();
    for entry in entries {
        let refuse = |reason: String| Error::Binding {
            method: entry.method.to_owned(),
            reason,
        };
        let expected = match entry.params {
            Carries::Nothing => Params::None,
            Carries::Required(schema) => Params::Required(schema.to_owned()),
            Carries::Optional(schema) => Params::Optional(schema.to_owned()),
        };
        match listed.get(entry.method) {
            None => {
                return Err(refuse(format!(
                    "the surface table lists it, but codex's `{union}` does not"
                )));
            }
            Some(Err(reason)) => return Err(refuse(reason.clone())),
            Some(Ok(params)) if *params != expected => {
                return Err(refuse(format!(
                    "the surface table says its params are {expected:?}, but codex's schema says \
                     {params:?}"
                )));
            }
            Some(Ok(_)) => {}
        }
        if expected != Params::None
            && let Some((other, _)) = listed.iter().find(|(other, params)| {
                other.as_str() != entry.method && params.as_ref() == Ok(&expected)
            })
        {
            return Err(refuse(format!(
                "`{other}` carries the same params, so the oracle cannot tell the two apart"
            )));
        }
        let response = match (direction.is_request(), entry.response) {
            (true, Some(response)) if schemas.get(response).is_some() => Some(response.to_owned()),
            (true, Some(response)) => {
                return Err(refuse(format!(
                    "codex's schema defines no response type `{response}`"
                )));
            }
            (true, None) => {
                return Err(refuse(
                    "it is a request, and the surface table gives it no response".to_owned(),
                ));
            }
            (false, Some(_)) => {
                return Err(refuse(
                    "it is a notification, which nothing answers, and the surface table gives \
                     it a response"
                        .to_owned(),
                ));
            }
            (false, None) => None,
        };
        let entry_typed = Typed {
            params: expected,
            response,
        };
        if typed.insert(entry.method, entry_typed).is_some() {
            return Err(refuse("the surface table lists it twice".to_owned()));
        }
    }
    Ok(typed)
}

/// Every schema a method of the [`Surface`] carries, which is what the generated types are grown
/// from.
#[must_use]
pub fn roots(methods: &[Method]) -> BTreeSet<&str> {
    methods
        .iter()
        .filter_map(|method| method.typed.as_ref())
        .flat_map(|typed| [typed.params.schema(), typed.response.as_deref()])
        .flatten()
        .collect()
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
    use googletest::prelude::*;
    use serde_json::{Value, json};

    use super::{Direction, Method, Params, Typed, read, roots, type_name};
    use crate::openapi::Schemas;
    use crate::surface::{Carries, Entry, Surface};

    fn bundle(client_requests: &[Value], server_notifications: &[Value]) -> Value {
        json!({"definitions": {
            "ClientRequest": {"oneOf": client_requests},
            "ClientNotification": {"oneOf": []},
            "ServerRequest": {"oneOf": []},
            "ServerNotification": {"oneOf": server_notifications},
            "v2": {
                "TurnStartParams": {}, "TurnStartResponse": {},
                "GetAccountRateLimitsParams": {}, "GetAccountRateLimitsResponse": {},
                "TurnStartedNotification": {},
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

    fn request(method: &'static str, params: &'static str, response: &'static str) -> Entry {
        Entry {
            method,
            params: Carries::Required(params),
            response: Some(response),
        }
    }

    fn surface(client_requests: Vec<Entry>, server_notifications: Vec<Entry>) -> Surface {
        Surface {
            client_requests,
            client_notifications: Vec::new(),
            server_requests: Vec::new(),
            server_notifications,
        }
    }

    fn read_bundle(
        bundle: &Value,
        surface: &Surface,
    ) -> std::result::Result<Vec<Method>, crate::Error> {
        let schemas =
            Schemas::from_bundle(bundle).map_err(|err| crate::Error::Bundle(err.to_string()))?;
        read(bundle, surface, &schemas)
    }

    /// A method of the surface is bound to its params and, for a request, its response; one
    /// outside it is bound by name alone, whatever shape its params have; and only the surface's
    /// payloads are roots.
    #[gtest]
    fn the_surface_is_typed_and_every_method_is_named() -> Result<()> {
        let bundle = bundle(
            &[
                branch("turn/start", Some(reference("TurnStartParams")), true),
                branch("account/logout", Some(json!({"type": "object"})), true),
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
        let surface = surface(
            vec![
                request("turn/start", "TurnStartParams", "TurnStartResponse"),
                Entry {
                    params: Carries::Optional("GetAccountRateLimitsParams"),
                    ..request(
                        "account/rateLimits/read",
                        "",
                        "GetAccountRateLimitsResponse",
                    )
                },
            ],
            vec![Entry {
                method: "turn/started",
                params: Carries::Required("TurnStartedNotification"),
                response: None,
            }],
        );
        let methods = read_bundle(&bundle, &surface).or_fail()?;

        assert_that!(
            methods,
            elements_are![
                matches_pattern!(Method {
                    direction: eq(&Direction::ClientRequest),
                    name: eq("AccountLogout"),
                    typed: none(),
                    ..
                }),
                matches_pattern!(Method {
                    name: eq("AccountRateLimitsRead"),
                    typed: some(eq(&Typed {
                        params: Params::Optional("GetAccountRateLimitsParams".to_owned()),
                        response: Some("GetAccountRateLimitsResponse".to_owned()),
                    })),
                    ..
                }),
                matches_pattern!(Method {
                    name: eq("TurnStart"),
                    typed: some(eq(&Typed {
                        params: Params::Required("TurnStartParams".to_owned()),
                        response: Some("TurnStartResponse".to_owned()),
                    })),
                    ..
                }),
                matches_pattern!(Method {
                    direction: eq(&Direction::ServerNotification),
                    name: eq("TurnStarted"),
                    typed: some(eq(&Typed {
                        params: Params::Required("TurnStartedNotification".to_owned()),
                        response: None,
                    })),
                    ..
                }),
            ]
        );
        assert_that!(roots(&methods).len(), eq(5));
        Ok(())
    }

    /// An entry for a method the bundle does not list, with params other than the bundle's or of
    /// a shape the generator does not bind, with params another method shares, a request entry
    /// with a response codex's schema does not define or with none, a notification entry with a
    /// response, and an entry listed twice each stop generation.
    #[gtest]
    fn an_entry_the_bundle_does_not_bear_out_is_refused() {
        let turn_start = || branch("turn/start", Some(reference("TurnStartParams")), true);
        let entry = || request("turn/start", "TurnStartParams", "TurnStartResponse");
        let started = || {
            branch(
                "turn/started",
                Some(reference("TurnStartedNotification")),
                true,
            )
        };
        let cases = [
            (
                vec![turn_start()],
                vec![],
                vec![request(
                    "turn/begin",
                    "TurnStartParams",
                    "TurnStartResponse",
                )],
                vec![],
                "does not",
            ),
            (
                vec![turn_start()],
                vec![],
                vec![request(
                    "turn/start",
                    "TurnBeginParams",
                    "TurnStartResponse",
                )],
                vec![],
                "its params are",
            ),
            (
                vec![branch("turn/start", Some(json!({"type": "object"})), true)],
                vec![],
                vec![entry()],
                vec![],
                "does not bind",
            ),
            (
                vec![
                    turn_start(),
                    branch("turn/begin", Some(reference("TurnStartParams")), true),
                ],
                vec![],
                vec![entry()],
                vec![],
                "cannot tell the two apart",
            ),
            (
                vec![turn_start()],
                vec![],
                vec![request(
                    "turn/start",
                    "TurnStartParams",
                    "TurnBeginResponse",
                )],
                vec![],
                "no response type",
            ),
            (
                vec![turn_start()],
                vec![],
                vec![Entry {
                    response: None,
                    ..entry()
                }],
                vec![],
                "gives it no response",
            ),
            (
                vec![],
                vec![started()],
                vec![],
                vec![Entry {
                    method: "turn/started",
                    params: Carries::Required("TurnStartedNotification"),
                    response: Some("TurnStartResponse"),
                }],
                "gives it a response",
            ),
            (
                vec![turn_start()],
                vec![],
                vec![entry(), entry()],
                vec![],
                "lists it twice",
            ),
        ];
        for (requests, notifications, request_entries, notification_entries, reason) in cases {
            assert_that!(
                read_bundle(
                    &bundle(&requests, &notifications),
                    &surface(request_entries, notification_entries)
                ),
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
                &surface(vec![], vec![])
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
