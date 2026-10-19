//! Renders `methods.rs`: a marker type per method, bound to its payload types, and per message kind
//! an `Any*` enum that classifies an incoming message by its method.

use indoc::{formatdoc, indoc};

use crate::methods::{MessageKind, MethodBinding, ParamsSpec, PayloadSchemas};

/// The source of `methods.rs`, without its header.
#[must_use]
pub fn render(methods: &[MethodBinding], codex_version: &str) -> String {
    let mut source_text = indoc! {"
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
            ClientNotification, ClientRequest, Method, Notification, RawParams, Request,
            ServerNotification, ServerRequest, TypedParams, TypedResponse,
        };
    "}
    .to_owned();
    source_text.push_str(&formatdoc! {"

        /// The codex release this protocol was generated from.
        pub const CODEX_VERSION: &str = {codex_version:?};
    "});

    for method in methods {
        source_text.push('\n');
        source_text.push_str(&render_method_binding(method));
    }
    for kind in MessageKind::ALL {
        let members: Vec<&MethodBinding> = methods
            .iter()
            .filter(|method| method.kind == kind)
            .collect();
        source_text.push('\n');
        source_text.push_str(&render_dispatch_enum(kind, &members, codex_version));
    }
    source_text
}

/// The marker type and trait implementations for one method.
fn render_method_binding(method: &MethodBinding) -> String {
    let MethodBinding {
        wire_name,
        rust_type_name,
        payload_schemas,
        ..
    } = method;
    let direction_trait_name = method.kind.union_name();
    let (message_kind_label, message_trait_name) = if method.kind.is_request() {
        ("request", "Request")
    } else {
        ("notification", "Notification")
    };
    let payload_impls = payload_schemas
        .as_ref()
        .map_or_else(String::new, |payload_schemas| {
            render_payload_impls(rust_type_name, payload_schemas)
        });
    formatdoc! {"
        /// The `{wire_name}` {message_kind_label}.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub struct {rust_type_name};

        impl Method for {rust_type_name} {{
            const METHOD: &'static str = {wire_name:?};
        }}

        {payload_impls}impl {message_trait_name} for {rust_type_name} {{}}

        impl {direction_trait_name} for {rust_type_name} {{}}
    "}
}

/// The trait implementations that bind a method agentmux uses to its payload types.
fn render_payload_impls(rust_type_name: &str, payload_schemas: &PayloadSchemas) -> String {
    let params = match &payload_schemas.params {
        ParamsSpec::None => "()".to_owned(),
        ParamsSpec::Required(schema) => format!("types::{schema}"),
        ParamsSpec::Optional(schema) => format!("Option<types::{schema}>"),
    };
    let response_impl = payload_schemas
        .response
        .as_deref()
        .map_or_else(String::new, |response| {
            formatdoc! {"
            impl TypedResponse for {rust_type_name} {{
                type Response = types::{response};
            }}

        "}
        });
    formatdoc! {"
        impl TypedParams for {rust_type_name} {{
            type Params = {params};
        }}

        {response_impl}"}
}

/// The `Any*` enum of one message kind.
fn render_dispatch_enum(
    kind: MessageKind,
    members: &[&MethodBinding],
    codex_version: &str,
) -> String {
    let union_name = kind.union_name();
    let summary = match kind {
        MessageKind::ClientRequest => "A request the client sends",
        MessageKind::ClientNotification => "A notification the client sends",
        MessageKind::ServerRequest => "A request the server sends",
        MessageKind::ServerNotification => "A notification the server sends",
    };
    let render_members = |render_member: &dyn Fn(&MethodBinding) -> String| -> String {
        members.iter().map(|method| render_member(method)).collect()
    };
    let variants = render_members(&|MethodBinding {
                                        wire_name,
                                        rust_type_name,
                                        payload_schemas,
                                        ..
                                    }| {
        match payload_schemas {
            Some(_) => formatdoc! {"
            {indent}/// `{wire_name}`.
            {indent}{rust_type_name}(RawParams<'a, {rust_type_name}>),
        ", indent = "    "},
            None => formatdoc! {"
            {indent}/// `{wire_name}`, which agentmux does not decode.
            {indent}{rust_type_name},
        ", indent = "    "},
        }
    });
    let classification_arms = render_members(&|MethodBinding {
                                                   rust_type_name,
                                                   payload_schemas,
                                                   ..
                                               }| {
        match payload_schemas {
            Some(_) => formatdoc! {"
            {indent}{rust_type_name}::METHOD => Self::{rust_type_name}(RawParams::new(params)),
        ", indent = "            "},
            None => formatdoc! {"
            {indent}{rust_type_name}::METHOD => Self::{rust_type_name},
        ", indent = "            "},
        }
    });
    let method_name_arms = render_members(&|MethodBinding {
                                                rust_type_name,
                                                payload_schemas,
                                                ..
                                            }| {
        match payload_schemas {
            Some(_) => formatdoc! {"
            {indent}Self::{rust_type_name}(_) => {rust_type_name}::METHOD,
        ", indent = "            "},
            None => formatdoc! {"
            {indent}Self::{rust_type_name} => {rust_type_name}::METHOD,
        ", indent = "            "},
        }
    });
    formatdoc! {"
        /// {summary}, classified by its method, with the parameters of one agentmux decodes not yet
        /// decoded.
        #[derive(Debug, Clone, Copy)]
        pub enum Any{union_name}<'a> {{
        {variants}}}

        impl<'a> Any{union_name}<'a> {{
            /// The message of `method` carrying `params`, or `None` when codex-cli {codex_version}
            /// sends no such method this way.
            #[must_use]
            pub fn classify(method: &str, params: &'a Value) -> Option<Self> {{
                Some(match method {{
        {classification_arms}            _ => return None,
                }})
            }}

            /// Its JSON-RPC method.
            #[must_use]
            pub fn method(self) -> &'static str {{
                match self {{
        {method_name_arms}        }}
            }}
        }}
    "}
}

#[cfg(test)]
mod tests {
    use googletest::prelude::*;
    use indoc::indoc;

    use super::render;
    use crate::methods::{MessageKind, MethodBinding, ParamsSpec, PayloadSchemas};

    fn method(
        kind: MessageKind,
        wire_name: &str,
        rust_type_name: &str,
        payload_schemas: Option<PayloadSchemas>,
    ) -> MethodBinding {
        MethodBinding {
            kind,
            wire_name: wire_name.to_owned(),
            rust_type_name: rust_type_name.to_owned(),
            payload_schemas,
        }
    }

    fn payload_schemas(params: ParamsSpec, response: Option<&str>) -> PayloadSchemas {
        PayloadSchemas {
            params,
            response: response.map(ToOwned::to_owned),
        }
    }

    #[gtest]
    fn each_method_becomes_a_marker_bound_to_what_agentmux_uses_of_it() {
        let source = render(
            &[
                method(
                    MessageKind::ClientRequest,
                    "turn/start",
                    "TurnStart",
                    Some(payload_schemas(
                        ParamsSpec::Required("TurnStartParams".to_owned()),
                        Some("TurnStartResponse"),
                    )),
                ),
                method(
                    MessageKind::ClientRequest,
                    "account/rateLimits/read",
                    "AccountRateLimitsRead",
                    Some(payload_schemas(
                        ParamsSpec::Optional("GetAccountRateLimitsParams".to_owned()),
                        Some("GetAccountRateLimitsResponse"),
                    )),
                ),
                method(
                    MessageKind::ClientRequest,
                    "account/logout",
                    "AccountLogout",
                    None,
                ),
                method(
                    MessageKind::ClientNotification,
                    "initialized",
                    "Initialized",
                    Some(payload_schemas(ParamsSpec::None, None)),
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

                impl TypedParams for TurnStart {
                    type Params = types::TurnStartParams;
                }

                impl TypedResponse for TurnStart {
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
                impl TypedParams for Initialized {
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
                    TurnStart(RawParams<'a, TurnStart>),
                    /// `account/rateLimits/read`.
                    AccountRateLimitsRead(RawParams<'a, AccountRateLimitsRead>),
                    /// `account/logout`, which agentmux does not decode.
                    AccountLogout,
                }
            "})
        );
        assert_that!(
            source,
            contains_substring(
                "            TurnStart::METHOD => Self::TurnStart(RawParams::new(params)),"
            )
        );
        assert_that!(
            source,
            contains_substring("            AccountLogout::METHOD => Self::AccountLogout,")
        );
    }
}
