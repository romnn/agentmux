//! Renders `methods.rs`: one marker type per manifest entry, bound to its payload types, and the
//! names of every method the server can send.

use std::collections::BTreeSet;

use indoc::{formatdoc, indoc};

use crate::Error;
use crate::manifest::{Direction, Entry, Manifest};

/// Every method the bundle lists in the server's direction, whether or not the manifest binds it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalogue {
    /// Every notification method the server can send.
    pub server_notifications: Vec<String>,
    /// Every request method the server can send.
    pub server_requests: Vec<String>,
}

/// The source of `methods.rs`, without its header.
///
/// # Errors
///
/// Returns [`Error::Binding`] when a method cannot be named as a Rust type, or two methods would
/// share a name.
pub fn render(
    manifest: &Manifest,
    catalogue: &Catalogue,
    codex_version: &str,
) -> Result<String, Error> {
    let mut out = indoc! {"
        //! The methods agentmux exchanges with `codex app-server`, each bound to its payload types.
        //!
        //! Each marker type stands for one JSON-RPC method: its trait implementations name the wire
        //! method, the type its `params` decode into, and, for a request, the type of its `result`,
        //! so a message cannot be built with one method and another method's payload.
        //! The direction traits say who may send it.

        use crate::types;
        use crate::{
            ClientNotification, ClientRequest, Notification, Request, ServerNotification,
            ServerRequest,
        };
    "}
    .to_owned();

    let mut names = BTreeSet::new();
    for entry in manifest.entries() {
        let name = marker(entry.method)?;
        if !names.insert(name.clone()) {
            return Err(Error::Binding {
                method: entry.method.to_owned(),
                reason: format!("another method is also named `{name}`"),
            });
        }
        out.push('\n');
        out.push_str(&binding(&name, &entry));
    }

    let list = |methods: &[String]| -> String {
        let lines: Vec<String> = methods
            .iter()
            .map(|method| format!("    {method:?},\n"))
            .collect();
        lines.concat()
    };
    out.push_str(&formatdoc! {"

        /// Every notification method codex-cli {codex_version} can send, whether or not a type here
        /// binds it.
        pub const SERVER_NOTIFICATIONS: &[&str] = &[
        {notifications}];

        /// Every request method codex-cli {codex_version} can send, whether or not a type here binds
        /// it.
        pub const SERVER_REQUESTS: &[&str] = &[
        {requests}];
        ",
        notifications = list(&catalogue.server_notifications),
        requests = list(&catalogue.server_requests),
    });
    Ok(out)
}

/// The marker type and trait implementations for one entry.
fn binding(name: &str, entry: &Entry) -> String {
    let method = entry.method;
    let params = entry
        .params
        .map_or_else(|| "()".to_owned(), |params| format!("types::{params}"));
    let (what, base, direction) = match entry.direction {
        Direction::ClientRequest => ("request", "Request", "ClientRequest"),
        Direction::ClientNotification => ("notification", "Notification", "ClientNotification"),
        Direction::ServerRequest => ("request", "Request", "ServerRequest"),
        Direction::ServerNotification => ("notification", "Notification", "ServerNotification"),
    };
    let response = entry
        .response
        .map(|response| format!("    type Response = types::{response};\n"))
        .unwrap_or_default();
    formatdoc! {"
        /// The `{method}` {what}.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub struct {name};

        impl {base} for {name} {{
            const METHOD: &'static str = {method:?};
            type Params = {params};
        {response}}}

        impl {direction} for {name} {{}}
    "}
}

/// The marker type's name for a method: each `/`-separated segment capitalised and joined, so
/// `item/commandExecution/requestApproval` is `ItemCommandExecutionRequestApproval`.
fn marker(method: &str) -> Result<String, Error> {
    let name: String = method
        .split('/')
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
        return Err(Error::Binding {
            method: method.to_owned(),
            reason: format!("`{name}` is not a Rust type name"),
        });
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use googletest::prelude::*;
    use indoc::indoc;

    use super::{Catalogue, marker, render};
    use crate::manifest::{Manifest, Notification, Request};

    /// A method's segments become one type name.
    #[gtest]
    fn a_method_is_named_by_its_segments() -> Result<()> {
        assert_that!(
            marker("item/commandExecution/requestApproval").or_fail()?,
            eq("ItemCommandExecutionRequestApproval")
        );
        assert_that!(marker("error").or_fail()?, eq("Error"));
        assert_that!(marker("turn/a-b"), err(anything()));
        Ok(())
    }

    /// A request binds its params and response; a notification without params carries `()`.
    #[gtest]
    fn each_entry_becomes_a_marker_bound_to_its_payloads() -> Result<()> {
        let manifest = Manifest {
            client_requests: &[Request {
                method: "turn/start",
                params: Some("TurnStartParams"),
                response: "TurnStartResponse",
            }],
            client_notifications: &[Notification {
                method: "initialized",
                params: None,
            }],
            ..Manifest::default()
        };
        let source = render(&manifest, &Catalogue::default(), "0.1.0").or_fail()?;

        assert_that!(
            source,
            contains_substring(indoc! {r#"
                impl Request for TurnStart {
                    const METHOD: &'static str = "turn/start";
                    type Params = types::TurnStartParams;
                    type Response = types::TurnStartResponse;
                }

                impl ClientRequest for TurnStart {}
            "#})
        );
        assert_that!(
            source,
            contains_substring(indoc! {r#"
                impl Notification for Initialized {
                    const METHOD: &'static str = "initialized";
                    type Params = ();
                }
            "#})
        );
        Ok(())
    }

    /// Two methods that would share a type name are refused.
    #[gtest]
    fn two_methods_with_one_name_are_refused() {
        let manifest = Manifest {
            server_notifications: &[
                Notification {
                    method: "turn/started",
                    params: None,
                },
                Notification {
                    method: "turnStarted",
                    params: None,
                },
            ],
            ..Manifest::default()
        };
        assert_that!(
            render(&manifest, &Catalogue::default(), "0.1.0"),
            err(displays_as(contains_substring("also named")))
        );
    }
}
