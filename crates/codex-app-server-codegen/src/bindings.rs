//! Renders `methods.rs`: a marker type per method, bound to its payload types, and per direction
//! an `Any*` enum that classifies an incoming message by its method.

use indoc::{formatdoc, indoc};

use crate::methods::{Direction, Method, Params};

/// The source of `methods.rs`, without its header.
#[must_use]
pub fn render(methods: &[Method], codex_version: &str) -> String {
    let mut out = indoc! {"
        //! Every method of the codex app-server protocol, each bound to its payload types.
        //!
        //! Each marker type stands for one JSON-RPC method: its trait implementations name the wire
        //! method, the type its `params` decode into, and, for a request, the type of its `result`,
        //! so a message cannot be built with one method and another method's payload.
        //! The direction traits say who may send it.
        //!
        //! Each `Any*` enum lists every method of one direction, so a receiver that matches on it
        //! is told by the compiler when a regeneration adds one.

        use serde_json::Value;

        use crate::types;
        use crate::{
            ClientNotification, ClientRequest, Method, Notification, Payload, Request,
            ServerNotification, ServerRequest,
        };
    "}
    .to_owned();

    for method in methods {
        out.push('\n');
        out.push_str(&binding(method));
    }
    for direction in Direction::ALL {
        let members: Vec<&Method> = methods
            .iter()
            .filter(|method| method.direction == direction)
            .collect();
        out.push('\n');
        out.push_str(&dispatch(direction, &members, codex_version));
    }
    out
}

/// The marker type and trait implementations for one method.
fn binding(method: &Method) -> String {
    let Method {
        wire, name, params, ..
    } = method;
    let params = match params {
        Params::None => "()".to_owned(),
        Params::Required(schema) => format!("types::{schema}"),
        Params::Optional(schema) => format!("Option<types::{schema}>"),
    };
    let direction = method.direction.union();
    let (what, kind) = match method.response.as_deref() {
        Some(response) => (
            "request",
            formatdoc! {"
                impl Request for {name} {{
                    type Response = types::{response};
                }}
            "},
        ),
        None => (
            "notification",
            format!("impl Notification for {name} {{}}\n"),
        ),
    };
    formatdoc! {"
        /// The `{wire}` {what}.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub struct {name};

        impl Method for {name} {{
            const METHOD: &'static str = {wire:?};
            type Params = {params};
        }}

        {kind}
        impl {direction} for {name} {{}}
    "}
}

/// The `Any*` enum of one direction.
fn dispatch(direction: Direction, members: &[&Method], codex_version: &str) -> String {
    let union = direction.union();
    let what = match direction {
        Direction::ClientRequest => "A request the client sends",
        Direction::ClientNotification => "A notification the client sends",
        Direction::ServerRequest => "A request the server sends",
        Direction::ServerNotification => "A notification the server sends",
    };
    let lines = |line: &dyn Fn(&Method) -> String| -> String {
        members.iter().map(|method| line(method)).collect()
    };
    let variants = lines(&|Method { wire, name, .. }| {
        format!("    /// `{wire}`.\n    {name}(Payload<'a, {name}>),\n")
    });
    let classify = lines(&|Method { name, .. }| {
        format!("            {name}::METHOD => Self::{name}(Payload::new(params)),\n")
    });
    let names =
        lines(&|Method { name, .. }| format!("            Self::{name}(_) => {name}::METHOD,\n"));
    formatdoc! {"
        /// {what}, classified by its method, with its parameters not yet decoded.
        #[derive(Debug, Clone, Copy)]
        pub enum Any{union}<'a> {{
        {variants}}}

        impl<'a> Any{union}<'a> {{
            /// The message of `method` carrying `params`, or `None` when codex-cli {codex_version}
            /// sends no such method this way.
            #[must_use]
            pub fn classify(method: &str, params: &'a Value) -> Option<Self> {{
                Some(match method {{
        {classify}            _ => return None,
                }})
            }}

            /// Its JSON-RPC method.
            #[must_use]
            pub fn method(self) -> &'static str {{
                match self {{
        {names}        }}
            }}
        }}
    "}
}

#[cfg(test)]
mod tests {
    use googletest::prelude::*;
    use indoc::indoc;

    use super::render;
    use crate::methods::{Direction, Method, Params};

    fn method(
        direction: Direction,
        wire: &str,
        name: &str,
        params: Params,
        response: Option<&str>,
    ) -> Method {
        Method {
            direction,
            wire: wire.to_owned(),
            name: name.to_owned(),
            params,
            response: response.map(ToOwned::to_owned),
        }
    }

    /// A request binds its params and response, a method without params carries `()`, optional
    /// params an `Option`, and every method is a variant of its direction's enum.
    #[gtest]
    fn each_method_becomes_a_marker_bound_to_its_payloads() {
        let source = render(
            &[
                method(
                    Direction::ClientRequest,
                    "turn/start",
                    "TurnStart",
                    Params::Required("TurnStartParams".to_owned()),
                    Some("TurnStartResponse"),
                ),
                method(
                    Direction::ClientRequest,
                    "account/rateLimits/read",
                    "AccountRateLimitsRead",
                    Params::Optional("GetAccountRateLimitsParams".to_owned()),
                    Some("GetAccountRateLimitsResponse"),
                ),
                method(
                    Direction::ClientNotification,
                    "initialized",
                    "Initialized",
                    Params::None,
                    None,
                ),
            ],
            "0.1.0",
        );

        assert_that!(
            source,
            contains_substring(indoc! {r#"
                impl Method for TurnStart {
                    const METHOD: &'static str = "turn/start";
                    type Params = types::TurnStartParams;
                }

                impl Request for TurnStart {
                    type Response = types::TurnStartResponse;
                }

                impl ClientRequest for TurnStart {}
            "#})
        );
        assert_that!(
            source,
            contains_substring("type Params = Option<types::GetAccountRateLimitsParams>;")
        );
        assert_that!(
            source,
            contains_substring(indoc! {r#"
                impl Method for Initialized {
                    const METHOD: &'static str = "initialized";
                    type Params = ();
                }

                impl Notification for Initialized {}
            "#})
        );
        assert_that!(
            source,
            contains_substring(indoc! {"
                pub enum AnyClientRequest<'a> {
                    /// `turn/start`.
                    TurnStart(Payload<'a, TurnStart>),
                    /// `account/rateLimits/read`.
                    AccountRateLimitsRead(Payload<'a, AccountRateLimitsRead>),
                }
            "})
        );
        assert_that!(
            source,
            contains_substring(
                "            TurnStart::METHOD => Self::TurnStart(Payload::new(params)),"
            )
        );
    }
}
