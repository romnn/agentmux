//! Renders `methods.rs`: a marker type per method, bound to its payload types, and per direction
//! an `Any*` enum that classifies an incoming message by its method.

use indoc::{formatdoc, indoc};

use crate::methods::{Direction, Method, Params, Typed};

/// The source of `methods.rs`, without its header.
#[must_use]
pub fn render(methods: &[Method], codex_version: &str) -> String {
    let mut out = indoc! {"
        //! Every method of the codex app-server protocol, each bound to its payload types.
        //!
        //! Each marker type stands for one JSON-RPC method and names it on the wire; the direction
        //! traits say who may send it.
        //! A method agentmux uses is also bound to the type its `params` decode into and, for a
        //! request, the type of its `result`, so a message cannot be built with one method and
        //! another method's payload, and a method agentmux does not use cannot be decoded at all.
        //!
        //! Each `Any*` enum lists every method of one direction, so a receiver that matches on it
        //! is told by the compiler when a regeneration adds one.

        use serde_json::Value;

        use crate::types;
        use crate::{
            Answered, ClientNotification, ClientRequest, Method, Notification, Payload, Request,
            ServerNotification, ServerRequest, Typed,
        };
    "}
    .to_owned();
    out.push_str(&formatdoc! {"

        /// The codex release this protocol was generated from.
        pub const CODEX_VERSION: &str = {codex_version:?};
    "});

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
        wire, name, typed, ..
    } = method;
    let direction = method.direction.union();
    let (what, kind) = if method.direction.is_request() {
        ("request", "Request")
    } else {
        ("notification", "Notification")
    };
    let typed = typed
        .as_ref()
        .map_or_else(String::new, |typed| bound(name, typed));
    formatdoc! {"
        /// The `{wire}` {what}.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub struct {name};

        impl Method for {name} {{
            const METHOD: &'static str = {wire:?};
        }}

        {typed}impl {kind} for {name} {{}}

        impl {direction} for {name} {{}}
    "}
}

/// The trait implementations that bind a method agentmux uses to its payload types.
fn bound(name: &str, typed: &Typed) -> String {
    let params = match &typed.params {
        Params::None => "()".to_owned(),
        Params::Required(schema) => format!("types::{schema}"),
        Params::Optional(schema) => format!("Option<types::{schema}>"),
    };
    let answered = typed
        .response
        .as_deref()
        .map_or_else(String::new, |response| {
            formatdoc! {"
            impl Answered for {name} {{
                type Response = types::{response};
            }}

        "}
        });
    formatdoc! {"
        impl Typed for {name} {{
            type Params = {params};
        }}

        {answered}"}
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
    let variants = lines(&|Method {
                               wire, name, typed, ..
                           }| {
        match typed {
            Some(_) => format!("    /// `{wire}`.\n    {name}(Payload<'a, {name}>),\n"),
            None => format!("    /// `{wire}`, which agentmux does not decode.\n    {name},\n"),
        }
    });
    let classify = lines(&|Method { name, typed, .. }| match typed {
        Some(_) => format!("            {name}::METHOD => Self::{name}(Payload::new(params)),\n"),
        None => format!("            {name}::METHOD => Self::{name},\n"),
    });
    let names = lines(&|Method { name, typed, .. }| match typed {
        Some(_) => format!("            Self::{name}(_) => {name}::METHOD,\n"),
        None => format!("            Self::{name} => {name}::METHOD,\n"),
    });
    formatdoc! {"
        /// {what}, classified by its method, with the parameters of one agentmux decodes not yet
        /// decoded.
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
    use crate::methods::{Direction, Method, Params, Typed};

    fn method(direction: Direction, wire: &str, name: &str, typed: Option<Typed>) -> Method {
        Method {
            direction,
            wire: wire.to_owned(),
            name: name.to_owned(),
            typed,
        }
    }

    fn typed(params: Params, response: Option<&str>) -> Typed {
        Typed {
            params,
            response: response.map(ToOwned::to_owned),
        }
    }

    /// A request agentmux uses binds its params and response, a notification it uses its params,
    /// a method without params carries `()` and optional params an `Option`, a method it does not
    /// use is named and nothing more, and every method is a variant of its direction's enum.
    #[gtest]
    fn each_method_becomes_a_marker_bound_to_what_agentmux_uses_of_it() {
        let source = render(
            &[
                method(
                    Direction::ClientRequest,
                    "turn/start",
                    "TurnStart",
                    Some(typed(
                        Params::Required("TurnStartParams".to_owned()),
                        Some("TurnStartResponse"),
                    )),
                ),
                method(
                    Direction::ClientRequest,
                    "account/rateLimits/read",
                    "AccountRateLimitsRead",
                    Some(typed(
                        Params::Optional("GetAccountRateLimitsParams".to_owned()),
                        Some("GetAccountRateLimitsResponse"),
                    )),
                ),
                method(
                    Direction::ClientRequest,
                    "account/logout",
                    "AccountLogout",
                    None,
                ),
                method(
                    Direction::ClientNotification,
                    "initialized",
                    "Initialized",
                    Some(typed(Params::None, None)),
                ),
            ],
            "0.1.0",
        );

        // A request agentmux sends: named, typed, answered, and sent by the client.
        assert_that!(
            source,
            contains_substring(indoc! {r#"
                impl Method for TurnStart {
                    const METHOD: &'static str = "turn/start";
                }

                impl Typed for TurnStart {
                    type Params = types::TurnStartParams;
                }

                impl Answered for TurnStart {
                    type Response = types::TurnStartResponse;
                }

                impl Request for TurnStart {}

                impl ClientRequest for TurnStart {}
            "#})
        );
        assert_that!(
            source,
            contains_substring("type Params = Option<types::GetAccountRateLimitsParams>;")
        );
        // A request agentmux never sends: named and directed, with no types.
        assert_that!(
            source,
            contains_substring(indoc! {r#"
                impl Method for AccountLogout {
                    const METHOD: &'static str = "account/logout";
                }

                impl Request for AccountLogout {}

                impl ClientRequest for AccountLogout {}
            "#})
        );
        assert_that!(
            source,
            contains_substring(indoc! {"
                impl Typed for Initialized {
                    type Params = ();
                }

                impl Notification for Initialized {}
            "})
        );
        // The enum carries the payload of what agentmux decodes, and names the rest.
        assert_that!(
            source,
            contains_substring(indoc! {"
                pub enum AnyClientRequest<'a> {
                    /// `turn/start`.
                    TurnStart(Payload<'a, TurnStart>),
                    /// `account/rateLimits/read`.
                    AccountRateLimitsRead(Payload<'a, AccountRateLimitsRead>),
                    /// `account/logout`, which agentmux does not decode.
                    AccountLogout,
                }
            "})
        );
        assert_that!(
            source,
            contains_substring(
                "            TurnStart::METHOD => Self::TurnStart(Payload::new(params)),"
            )
        );
        assert_that!(
            source,
            contains_substring("            AccountLogout::METHOD => Self::AccountLogout,")
        );
    }
}
