//! The part of codex's protocol agentmux uses: `surface!`, the one table of it.
//!
//! Every method codex lists is bound by name, so a method codex adds or drops changes the
//! generated `Any*` enums and fails to build where they are matched.
//! Only the methods in this table carry types: the params of each request and notification
//! agentmux sends or decodes, and the response of each request it sends or answers.
//! Everything else stays a name, so nothing agentmux never reads is generated, and decoding it is
//! a compile error rather than a dependency on codex's whole schema.
//!
//! The table also states what codex's schema never says: which type answers a request.
//! Two checks keep it honest:
//!
//! - generation ([`crate::methods::read`]) checks every entry against the vendored schema: the
//!   method is listed in the entry's section, its params are the ones the entry names, no other
//!   method of that section carries the same params, and the response type exists;
//! - the oracle (`oracle/`, built by `task codegen:app-server:oracle`) expands the same table into
//!   a match on codex's own enums at the pinned release, which compiles only when each variant
//!   carries exactly the entry's params and response.
//!
//! Because the params are unique within a section, the variant the oracle checks is the method
//! the schema lists, so each entry is verified end to end without reading codex's source.
//! The tie is as strong as that uniqueness: it is checked over the stable schema the bundle
//! exports, so an experimental method sharing an entry's params, or a type codex names under an
//! alias, would let a wrong variant pass both checks, and a method without params is tied to its
//! variant by name alone.

/// The table, handed to `$emit` as one section per message union.
///
/// Each entry is `"method" = Variant(Params) => Response`: `Variant` is codex's own name for the
/// method, `Params?` marks params that may be left out, a method without params has no
/// parentheses, and only a request has a response.
#[macro_export]
macro_rules! surface {
    ($emit:ident) => {
        $emit! {
            client_requests {
                "initialize" = Initialize(InitializeParams) => InitializeResponse,
                "thread/start" = ThreadStart(ThreadStartParams) => ThreadStartResponse,
                "thread/resume" = ThreadResume(ThreadResumeParams) => ThreadResumeResponse,
                "turn/start" = TurnStart(TurnStartParams) => TurnStartResponse,
                "turn/steer" = TurnSteer(TurnSteerParams) => TurnSteerResponse,
                "turn/interrupt" = TurnInterrupt(TurnInterruptParams) => TurnInterruptResponse,
                "account/rateLimits/read" =
                    GetAccountRateLimits(GetAccountRateLimitsParams?) => GetAccountRateLimitsResponse,
            }
            client_notifications {
                "initialized" = Initialized,
            }
            server_requests {
                "item/commandExecution/requestApproval" =
                    CommandExecutionRequestApproval(CommandExecutionRequestApprovalParams)
                    => CommandExecutionRequestApprovalResponse,
                "item/fileChange/requestApproval" =
                    FileChangeRequestApproval(FileChangeRequestApprovalParams)
                    => FileChangeRequestApprovalResponse,
                "item/permissions/requestApproval" =
                    PermissionsRequestApproval(PermissionsRequestApprovalParams)
                    => PermissionsRequestApprovalResponse,
                "item/tool/requestUserInput" =
                    ToolRequestUserInput(ToolRequestUserInputParams) => ToolRequestUserInputResponse,
                "mcpServer/elicitation/request" =
                    McpServerElicitationRequest(McpServerElicitationRequestParams)
                    => McpServerElicitationRequestResponse,
                "applyPatchApproval" =
                    ApplyPatchApproval(ApplyPatchApprovalParams) => ApplyPatchApprovalResponse,
                "execCommandApproval" =
                    ExecCommandApproval(ExecCommandApprovalParams) => ExecCommandApprovalResponse,
            }
            server_notifications {
                "turn/started" = TurnStarted(TurnStartedNotification),
                "turn/completed" = TurnCompleted(TurnCompletedNotification),
                "item/completed" = ItemCompleted(ItemCompletedNotification),
                "error" = Error(ErrorNotification),
                "warning" = Warning(WarningNotification),
                "configWarning" = ConfigWarning(ConfigWarningNotification),
                "deprecationNotice" = DeprecationNotice(DeprecationNoticeNotification),
                "model/rerouted" = ModelRerouted(ModelReroutedNotification),
                "thread/tokenUsage/updated" =
                    ThreadTokenUsageUpdated(ThreadTokenUsageUpdatedNotification),
            }
        }
    };
}

/// What one entry says a method's `params` carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryParams {
    /// Nothing.
    Nothing,
    /// Always this schema.
    Required(&'static str),
    /// This schema, or nothing.
    Optional(&'static str),
}

/// One entry of the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    /// The method on the wire.
    pub method: &'static str,
    /// What its params carry.
    pub params: EntryParams,
    /// The schema of the `result` that answers it, for a request.
    pub response: Option<&'static str>,
}

/// The table, one section per message union.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Surface {
    /// Requests the client sends.
    pub client_requests: Vec<Entry>,
    /// Notifications the client sends.
    pub client_notifications: Vec<Entry>,
    /// Requests the server sends.
    pub server_requests: Vec<Entry>,
    /// Notifications the server sends.
    pub server_notifications: Vec<Entry>,
}

/// What an entry's params carry, from its `(Params)` or `(Params?)`, or nothing.
macro_rules! entry_params {
    () => {
        EntryParams::Nothing
    };
    ($params:ident) => {
        EntryParams::Required(stringify!($params))
    };
    ($params:ident ?) => {
        EntryParams::Optional(stringify!($params))
    };
}

/// A request's response from its `=> Response`, or `None`.
macro_rules! response {
    () => {
        None
    };
    ($response:ident) => {
        Some(stringify!($response))
    };
}

/// The table as data.
macro_rules! entries {
    ($(
        $section:ident {
            $(
                $method:literal = $variant:ident
                    $(($params:ident $($optional:tt)?))?
                    $(=> $response:ident)?,
            )*
        }
    )*) => {
        Surface {
            $(
                $section: vec![$(
                    Entry {
                        method: $method,
                        params: entry_params!($($params $($optional)?)?),
                        response: response!($($response)?),
                    },
                )*],
            )*
        }
    };
}

impl Surface {
    /// The table `surface!` states.
    #[must_use]
    pub fn table() -> Self {
        surface!(entries)
    }
}

#[cfg(test)]
mod tests {
    use googletest::prelude::*;

    use super::{Entry, EntryParams, Surface};

    /// An entry reads with its method, its params, and a request's response; `?` marks optional
    /// params, and a method without params carries nothing.
    #[gtest]
    fn the_table_reads_as_data() {
        let table = Surface::table();
        assert_that!(
            table.client_requests,
            contains(eq(&Entry {
                method: "turn/start",
                params: EntryParams::Required("TurnStartParams"),
                response: Some("TurnStartResponse"),
            }))
        );
        assert_that!(
            table.client_requests,
            contains(eq(&Entry {
                method: "account/rateLimits/read",
                params: EntryParams::Optional("GetAccountRateLimitsParams"),
                response: Some("GetAccountRateLimitsResponse"),
            }))
        );
        assert_that!(
            table.client_notifications,
            elements_are![eq(&Entry {
                method: "initialized",
                params: EntryParams::Nothing,
                response: None,
            })]
        );
        assert_that!(
            table.server_notifications,
            contains(eq(&Entry {
                method: "turn/completed",
                params: EntryParams::Required("TurnCompletedNotification"),
                response: None,
            }))
        );
    }
}
