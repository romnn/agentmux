//! Checks [`codex_app_server_codegen::surface!`], the exec stream's mirror and a rollout's usage
//! windows' mirror against codex's own Rust at the pinned release.
//!
//! The mirrors' checks are [`exec`] and [`rollout`]; the rest of this file is the table's.
//!
//! Compiling this crate is the check.
//! The table is expanded into one match per enum codex declares its methods in, and each arm
//! binds the variant's params and response to the types the table names, so a variant that
//! carries anything else is a type error, and a variant codex does not have is a missing one.
//! The generator checks that each entry's params belong to exactly one method of the schema,
//! which ties the variant checked here to the method the generated bindings bind.

pub mod exec;
pub mod rollout;

use codex_app_server_protocol as codex;

/// The params type an entry names, `?` making it optional.
macro_rules! params {
    ($params:ident) => {
        codex::$params
    };
    ($params:ident ?) => {
        Option<codex::$params>
    };
}

/// One function per message union, matching codex's enums against the table.
macro_rules! check {
    (
        client_requests {
            $(
                $client_request_method:literal = $client_request:ident
                    ($client_request_params:ident $($client_request_optional:tt)?)
                    => $client_response:ident,
            )*
        }
        client_notifications {
            $($client_notification_method:literal = $client_notification:ident,)*
        }
        server_requests {
            $(
                $server_request_method:literal = $server_request:ident
                    ($server_request_params:ident $($server_request_optional:tt)?)
                    => $server_response:ident,
            )*
        }
        server_notifications {
            $(
                $server_notification_method:literal =
                    $server_notification:ident($server_notification_params:ident),
            )*
        }
    ) => {
        /// Every request the table says the client sends, and what answers it, as codex declares
        /// them.
        pub fn client_requests(request: codex::ClientRequest, response: codex::ClientResponse) {
            match request {
                $(
                    codex::ClientRequest::$client_request { params, .. } => {
                        let _: params!($client_request_params $($client_request_optional)?) =
                            params;
                    }
                )*
                _ => {}
            }
            match response {
                $(
                    codex::ClientResponse::$client_request { response, .. } => {
                        let _: codex::$client_response = response;
                    }
                )*
                _ => {}
            }
        }

        /// Every notification the table says the client sends, as codex declares it: none
        /// carries params.
        pub fn client_notifications(notification: codex::ClientNotification) {
            match notification {
                $(codex::ClientNotification::$client_notification => {})*
                #[allow(
                    unreachable_patterns,
                    reason = "codex may declare no client notification beyond the table's, as \
                              0.162.1 does; the arm keeps the check compiling when it adds one"
                )]
                _ => {}
            }
        }

        /// Every request the table says the server sends, and what answers it, as codex declares
        /// them.
        pub fn server_requests(request: codex::ServerRequest, response: codex::ServerResponse) {
            match request {
                $(
                    codex::ServerRequest::$server_request { params, .. } => {
                        let _: params!($server_request_params $($server_request_optional)?) =
                            params;
                    }
                )*
                _ => {}
            }
            match response {
                $(
                    codex::ServerResponse::$server_request { response, .. } => {
                        let _: codex::$server_response = response;
                    }
                )*
                _ => {}
            }
        }

        /// Every notification the table says the server sends, as codex declares it.
        pub fn server_notifications(notification: codex::ServerNotification) {
            match notification {
                $(
                    codex::ServerNotification::$server_notification(params) => {
                        let _: codex::$server_notification_params = params;
                    }
                )*
                _ => {}
            }
        }
    };
}

codex_app_server_codegen::surface!(check);
