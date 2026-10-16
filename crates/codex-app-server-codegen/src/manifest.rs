//! The methods the generated crate binds: [`MANIFEST`].
//!
//! The manifest is the one list of what agentmux exchanges with `codex app-server`: the generated
//! types are exactly the payloads it names and what they reference, and each entry becomes a
//! method binding that ties the wire name to its payload types.
//! Everything the bundle can say about an entry is checked against it, so a method codex renamed,
//! or a payload it retyped, stops generation rather than generating a binding codex would refuse.
//! A request's response type is the exception: codex's schema lists a request's parameters but not
//! its result, so the manifest's pairing is checked only for the type existing, and is reviewed
//! against codex's own request definitions (`codex-rs/app-server-protocol/src/protocol/common.rs`).
//! A method the server can send that is not bound here is still named in the generated
//! `SERVER_NOTIFICATIONS` or `SERVER_REQUESTS`.

use std::collections::BTreeSet;

use serde_json::Value;

use crate::Error;
use crate::openapi::Schemas;

/// The methods agentmux exchanges with `codex app-server`.
pub const MANIFEST: Manifest = Manifest {
    // The handshake, the thread, and the turn
    client_requests: &[
        Request {
            method: "initialize",
            params: Some("InitializeParams"),
            response: "InitializeResponse",
        },
        Request {
            method: "thread/start",
            params: Some("ThreadStartParams"),
            response: "ThreadStartResponse",
        },
        Request {
            method: "thread/resume",
            params: Some("ThreadResumeParams"),
            response: "ThreadResumeResponse",
        },
        Request {
            method: "turn/start",
            params: Some("TurnStartParams"),
            response: "TurnStartResponse",
        },
        Request {
            method: "turn/steer",
            params: Some("TurnSteerParams"),
            response: "TurnSteerResponse",
        },
        Request {
            method: "turn/interrupt",
            params: Some("TurnInterruptParams"),
            response: "TurnInterruptResponse",
        },
    ],
    client_notifications: &[Notification {
        method: "initialized",
        params: None,
    }],
    // What the driver declines, because a consultation has nobody to ask
    server_requests: &[
        Request {
            method: "item/commandExecution/requestApproval",
            params: Some("CommandExecutionRequestApprovalParams"),
            response: "CommandExecutionRequestApprovalResponse",
        },
        Request {
            method: "item/fileChange/requestApproval",
            params: Some("FileChangeRequestApprovalParams"),
            response: "FileChangeRequestApprovalResponse",
        },
        Request {
            method: "item/permissions/requestApproval",
            params: Some("PermissionsRequestApprovalParams"),
            response: "PermissionsRequestApprovalResponse",
        },
        Request {
            method: "execCommandApproval",
            params: Some("ExecCommandApprovalParams"),
            response: "ExecCommandApprovalResponse",
        },
        Request {
            method: "applyPatchApproval",
            params: Some("ApplyPatchApprovalParams"),
            response: "ApplyPatchApprovalResponse",
        },
        Request {
            method: "mcpServer/elicitation/request",
            params: Some("McpServerElicitationRequestParams"),
            response: "McpServerElicitationRequestResponse",
        },
        Request {
            method: "item/tool/requestUserInput",
            params: Some("ToolRequestUserInputParams"),
            response: "ToolRequestUserInputResponse",
        },
    ],
    // What the driver translates into the capture
    server_notifications: &[
        notification("turn/started", "TurnStartedNotification"),
        notification("turn/completed", "TurnCompletedNotification"),
        notification("item/completed", "ItemCompletedNotification"),
        notification("error", "ErrorNotification"),
        notification("warning", "WarningNotification"),
        notification("configWarning", "ConfigWarningNotification"),
        notification("deprecationNotice", "DeprecationNoticeNotification"),
        notification("model/rerouted", "ModelReroutedNotification"),
        notification(
            "thread/tokenUsage/updated",
            "ThreadTokenUsageUpdatedNotification",
        ),
    ],
};

/// Every method binding to generate, by who sends it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Manifest {
    /// Requests the client sends, which the server answers.
    pub client_requests: &'static [Request],
    /// Notifications the client sends.
    pub client_notifications: &'static [Notification],
    /// Requests the server sends, which the client answers.
    pub server_requests: &'static [Request],
    /// Notifications the server sends.
    pub server_notifications: &'static [Notification],
}

/// A request and the types it carries each way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Request {
    /// The JSON-RPC method.
    pub method: &'static str,
    /// The schema of its `params`, or `None` for a request that carries none.
    pub params: Option<&'static str>,
    /// The schema of its `result`.
    pub response: &'static str,
}

/// A notification and the type it carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Notification {
    /// The JSON-RPC method.
    pub method: &'static str,
    /// The schema of its `params`, or `None` for a notification that carries none.
    pub params: Option<&'static str>,
}

/// A notification that carries `params`.
const fn notification(method: &'static str, params: &'static str) -> Notification {
    Notification {
        method,
        params: Some(params),
    }
}

/// Who sends a message, which names the bundle's union that lists it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// The bundle's union of every message in this direction.
    #[must_use]
    pub fn union(self) -> &'static str {
        match self {
            Self::ClientRequest => "ClientRequest",
            Self::ClientNotification => "ClientNotification",
            Self::ServerRequest => "ServerRequest",
            Self::ServerNotification => "ServerNotification",
        }
    }
}

impl Manifest {
    /// Every entry, with the direction it is sent in, its parameters and its response.
    #[must_use]
    pub fn entries(&self) -> Vec<Entry> {
        let requests = [
            (Direction::ClientRequest, self.client_requests),
            (Direction::ServerRequest, self.server_requests),
        ];
        let notifications = [
            (Direction::ClientNotification, self.client_notifications),
            (Direction::ServerNotification, self.server_notifications),
        ];
        let mut entries = Vec::new();
        for (direction, requests) in requests {
            entries.extend(requests.iter().map(|request| Entry {
                direction,
                method: request.method,
                params: request.params,
                response: Some(request.response),
            }));
        }
        for (direction, notifications) in notifications {
            entries.extend(notifications.iter().map(|notification| Entry {
                direction,
                method: notification.method,
                params: notification.params,
                response: None,
            }));
        }
        entries
    }

    /// Every schema an entry names, which is what the generated types are grown from.
    #[must_use]
    pub fn roots(&self) -> BTreeSet<&'static str> {
        self.entries()
            .into_iter()
            .flat_map(|entry| [entry.params, entry.response])
            .flatten()
            .collect()
    }

    /// Check every entry against the bundle it is generated from.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Binding`] for the first entry whose method the bundle's union for its
    /// direction does not list, whose parameters differ from the union's, or whose response names
    /// no schema.
    pub fn verify(&self, bundle: &Value, schemas: &Schemas) -> Result<(), Error> {
        for entry in self.entries() {
            let refuse = |reason: String| Error::Binding {
                method: entry.method.to_owned(),
                reason,
            };
            let union = entry.direction.union();
            let branch = branches(bundle, union)
                .find(|branch| branch_method(branch) == Some(entry.method))
                .ok_or_else(|| refuse(format!("codex's `{union}` has no such method")))?;
            let declared = branch
                .pointer("/properties/params/$ref")
                .and_then(Value::as_str)
                .map(|target| target.rsplit('/').next().unwrap_or(target));
            if declared != entry.params {
                return Err(refuse(format!(
                    "the manifest names its params {}, codex's `{union}` {}",
                    describe(entry.params),
                    describe(declared)
                )));
            }
            if let Some(response) = entry.response
                && schemas.get(response).is_none()
            {
                return Err(refuse(format!(
                    "codex defines no response type `{response}`"
                )));
            }
        }
        Ok(())
    }
}

/// One binding to generate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    /// Who sends it.
    pub direction: Direction,
    /// The JSON-RPC method.
    pub method: &'static str,
    /// The schema of its `params`, if it carries any.
    pub params: Option<&'static str>,
    /// The schema of its `result`, for a request.
    pub response: Option<&'static str>,
}

/// The branches of one of the bundle's message unions.
fn branches<'a>(bundle: &'a Value, union: &str) -> impl Iterator<Item = &'a Value> {
    bundle
        .pointer(&format!("/definitions/{union}/oneOf"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
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

/// A params schema as an error message names it.
fn describe(params: Option<&str>) -> String {
    params.map_or_else(|| "none".to_owned(), |name| format!("`{name}`"))
}

#[cfg(test)]
mod tests {
    use googletest::prelude::*;
    use serde_json::json;

    use super::{MANIFEST, Manifest, Request};
    use crate::openapi::Schemas;

    fn bundle() -> serde_json::Value {
        json!({"definitions": {
            "ClientRequest": {"oneOf": [{
                "properties": {
                    "method": {"enum": ["turn/start"]},
                    "params": {"$ref": "#/definitions/v2/TurnStartParams"},
                },
            }]},
            "v2": {"TurnStartParams": {}, "TurnStartResponse": {}},
        }})
    }

    fn turn_start(request: &'static [Request]) -> Manifest {
        Manifest {
            client_requests: request,
            ..Manifest::default()
        }
    }

    /// An entry that agrees with codex's union passes, and its payloads are the roots.
    #[gtest]
    fn an_entry_codex_agrees_with_is_accepted() -> Result<()> {
        let manifest = turn_start(&[Request {
            method: "turn/start",
            params: Some("TurnStartParams"),
            response: "TurnStartResponse",
        }]);
        let bundle = bundle();
        let schemas = Schemas::from_bundle(&bundle).or_fail()?;

        assert_that!(manifest.verify(&bundle, &schemas), ok(anything()));
        assert_that!(
            manifest.roots(),
            unordered_elements_are![eq(&"TurnStartParams"), eq(&"TurnStartResponse")]
        );
        Ok(())
    }

    /// A method codex does not list, params codex types differently, and a response codex does
    /// not define are each refused.
    #[gtest]
    fn an_entry_codex_disagrees_with_is_refused() -> Result<()> {
        let bundle = bundle();
        let schemas = Schemas::from_bundle(&bundle).or_fail()?;
        let cases: [(&'static [Request], &str); 3] = [
            (
                &[Request {
                    method: "turn/begin",
                    params: Some("TurnStartParams"),
                    response: "TurnStartResponse",
                }],
                "has no such method",
            ),
            (
                &[Request {
                    method: "turn/start",
                    params: Some("TurnSteerParams"),
                    response: "TurnStartResponse",
                }],
                "names its params",
            ),
            (
                &[Request {
                    method: "turn/start",
                    params: Some("TurnStartParams"),
                    response: "TurnBeginResponse",
                }],
                "no response type",
            ),
        ];
        for (requests, reason) in cases {
            assert_that!(
                turn_start(requests).verify(&bundle, &schemas),
                err(displays_as(contains_substring(reason)))
            );
        }
        Ok(())
    }

    /// No two entries bind the same method.
    #[gtest]
    fn the_manifest_binds_each_method_once() {
        let entries = MANIFEST.entries();
        let methods: std::collections::BTreeSet<&str> =
            entries.iter().map(|entry| entry.method).collect();
        assert_that!(methods.len(), eq(entries.len()));
    }
}
