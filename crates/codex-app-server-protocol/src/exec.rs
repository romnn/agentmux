//! The events `codex exec --json` writes, one JSON object per line.
//!
//! codex publishes no schema for this stream, so these types mirror its own Rust
//! (`codex-rs/exec/src/exec_events.rs`, re-exported from the `codex_exec` crate) by hand, type for
//! type and field for field.
//! The oracle in `codex-app-server-codegen/oracle` converts every one of codex's types into its
//! mirror here at the pinned release, naming every field on both sides, so a field or variant
//! codex adds, removes or retypes fails to build there rather than drifting here, and checks that
//! each converted value serialises exactly as codex's own does.
//!
//! The mirror writes what codex writes and reads more than codex reads, because a reader of a
//! capture meets lines a newer codex wrote, and lines an older agentmux wrote, and must take from
//! them what it took before:
//!
//! - an `Unknown` variant where a reader meets a `type`, status or kind it was not built for, so
//!   a newer codex's event or item is named and counted rather than failing the line;
//! - an `Option` for every field a reader takes from a line that codex requires, so its absence
//!   or a `null` reads as absent, as the reader has always treated it; codex writes `None` as
//!   nothing at all, never as `null`, and agentmux never writes one;
//! - a default for an item that only narrates the delegate's work, which nothing reads, and for
//!   ids and error messages, which a reader takes only when it can, whenever they are not of
//!   their type, so a line from a codex that changed them still reads as what it is.
//!
//! Nothing writes an `Unknown`.

use std::collections::BTreeMap;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

/// Read a value, or its default when it is not one of its type.
///
/// For what no reader takes from a line, or takes only when it can: an older reader skipped
/// these without looking, so a value of the wrong type must not now fail the line around it.
fn forgiving<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned + Default,
{
    Ok(T::deserialize(Value::deserialize(deserializer)?).unwrap_or_default())
}

/// One line of the stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ThreadEvent {
    /// The thread was created or resumed; its id is what a follow-up resumes.
    #[serde(rename = "thread.started")]
    ThreadStarted(ThreadStartedEvent),
    /// A turn began.
    #[serde(rename = "turn.started")]
    TurnStarted(TurnStartedEvent),
    /// The turn finished.
    #[serde(rename = "turn.completed")]
    TurnCompleted(TurnCompletedEvent),
    /// The turn ended with an error.
    #[serde(rename = "turn.failed")]
    TurnFailed(TurnFailedEvent),
    /// An item began.
    #[serde(rename = "item.started")]
    ItemStarted(ItemStartedEvent),
    /// An item changed.
    #[serde(rename = "item.updated")]
    ItemUpdated(ItemUpdatedEvent),
    /// An item reached its final state.
    #[serde(rename = "item.completed")]
    ItemCompleted(ItemCompletedEvent),
    /// An error the stream reports outside any item, which need not end the turn.
    #[serde(rename = "error")]
    Error(ThreadErrorEvent),
    /// A `type` this mirror does not name.
    #[serde(other)]
    Unknown,
}

/// The thread the stream belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadStartedEvent {
    /// The thread's id.
    #[serde(
        default,
        deserialize_with = "forgiving",
        skip_serializing_if = "Option::is_none"
    )]
    pub thread_id: Option<String>,
}

/// A turn began; the event carries nothing else.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnStartedEvent {}

/// The turn finished.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnCompletedEvent {
    /// Tokens spent since the process started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

/// The turn ended with an error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnFailedEvent {
    /// What went wrong.
    #[serde(
        default,
        deserialize_with = "forgiving",
        skip_serializing_if = "Option::is_none"
    )]
    pub error: Option<ThreadErrorEvent>,
}

/// Token counts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[expect(
    clippy::struct_field_names,
    reason = "every field ends in `_tokens` because the fields are codex's own JSON keys, which \
              the oracle matches name for name"
)]
pub struct Usage {
    /// Input tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<i64>,
    /// Input tokens served from the cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<i64>,
    /// Input tokens written to the cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_input_tokens: Option<i64>,
    /// Output tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<i64>,
    /// Output tokens spent reasoning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_output_tokens: Option<i64>,
}

/// An item began.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemStartedEvent {
    /// The item.
    pub item: ThreadItem,
}

/// An item reached its final state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemCompletedEvent {
    /// The item.
    pub item: ThreadItem,
}

/// An item changed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemUpdatedEvent {
    /// The item.
    pub item: ThreadItem,
}

/// An error, as the stream and a failed turn report it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadErrorEvent {
    /// The error, in codex's words.
    #[serde(
        default,
        deserialize_with = "forgiving",
        skip_serializing_if = "Option::is_none"
    )]
    pub message: Option<String>,
}

/// One item of a turn.
///
/// Read a line as a [`serde_json::Value`] before reading it as this.
/// codex writes a web search's own id under `id` beside the item's, so its line carries the key
/// twice, which a typed read refuses and a JSON reader settles by keeping the last.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThreadItem {
    /// The item's id, which an older agentmux left out of a reply or a note of its own, or wrote
    /// as `null`.
    #[serde(
        default,
        deserialize_with = "forgiving",
        skip_serializing_if = "Option::is_none"
    )]
    pub id: Option<String>,
    /// What the item is.
    #[serde(flatten)]
    pub details: ThreadItemDetails,
}

/// What an item is, by its `type`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ThreadItemDetails {
    /// A message from the agent: the answer.
    AgentMessage(AgentMessageItem),
    /// A summary of the agent's reasoning.
    #[serde(deserialize_with = "forgiving")]
    Reasoning(ReasoningItem),
    /// A command the agent ran.
    #[serde(deserialize_with = "forgiving")]
    CommandExecution(CommandExecutionItem),
    /// Files the agent changed.
    #[serde(deserialize_with = "forgiving")]
    FileChange(FileChangeItem),
    /// A call to an MCP tool.
    #[serde(deserialize_with = "forgiving")]
    McpToolCall(McpToolCallItem),
    /// A call to a sub-agent tool.
    #[serde(deserialize_with = "forgiving")]
    CollabToolCall(CollabToolCallItem),
    /// A web search.
    #[serde(deserialize_with = "forgiving")]
    WebSearch(WebSearchItem),
    /// The agent's to-do list.
    #[serde(deserialize_with = "forgiving")]
    TodoList(TodoListItem),
    /// A warning that does not end the turn.
    Error(ErrorItem),
    /// A `type` this mirror does not name.
    #[serde(other)]
    Unknown,
}

/// A message from the agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMessageItem {
    /// What it said.
    ///
    /// Absent only from a codex that renamed it, which a reader counts rather than reading as
    /// an empty answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// A summary of the agent's reasoning.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReasoningItem {
    /// The summary.
    pub text: String,
}

/// Where a command stands.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandExecutionStatus {
    /// Running.
    #[default]
    InProgress,
    /// Finished.
    Completed,
    /// Ended badly.
    Failed,
    /// Refused before it ran.
    Declined,
    /// A value this mirror does not name.
    #[serde(other)]
    Unknown,
}

/// A command the agent ran.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CommandExecutionItem {
    /// The command line.
    pub command: String,
    /// What it printed, stdout and stderr together.
    pub aggregated_output: String,
    /// Its exit code, once it has one.
    pub exit_code: Option<i32>,
    /// Where it stands.
    pub status: CommandExecutionStatus,
}

/// One file a change touched.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FileUpdateChange {
    /// The file.
    pub path: String,
    /// What happened to it.
    pub kind: PatchChangeKind,
}

/// Where a change stands.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PatchApplyStatus {
    /// Being applied.
    InProgress,
    /// Applied.
    Completed,
    /// Not applied.
    Failed,
    /// A value this mirror does not name.
    #[default]
    #[serde(other)]
    Unknown,
}

/// Files the agent changed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FileChangeItem {
    /// The files.
    pub changes: Vec<FileUpdateChange>,
    /// Where the change stands.
    pub status: PatchApplyStatus,
}

/// What happened to one file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PatchChangeKind {
    /// Created.
    Add,
    /// Removed.
    Delete,
    /// Edited.
    Update,
    /// A value this mirror does not name.
    #[default]
    #[serde(other)]
    Unknown,
}

/// Where an MCP tool call stands.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpToolCallStatus {
    /// Running.
    #[default]
    InProgress,
    /// Answered.
    Completed,
    /// Ended badly.
    Failed,
    /// A value this mirror does not name.
    #[serde(other)]
    Unknown,
}

/// Where a sub-agent tool call stands.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CollabToolCallStatus {
    /// Running.
    #[default]
    InProgress,
    /// Answered.
    Completed,
    /// Ended badly.
    Failed,
    /// A value this mirror does not name.
    #[serde(other)]
    Unknown,
}

/// Which sub-agent tool was called.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CollabTool {
    /// Start an agent.
    SpawnAgent,
    /// Send an agent input.
    SendInput,
    /// Wait for agents.
    Wait,
    /// Stop an agent.
    CloseAgent,
    /// A value this mirror does not name.
    #[default]
    #[serde(other)]
    Unknown,
}

/// Where one sub-agent stands.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CollabAgentStatus {
    /// Starting.
    PendingInit,
    /// Working.
    Running,
    /// Stopped part-way.
    Interrupted,
    /// Done.
    Completed,
    /// Ended badly.
    Errored,
    /// Shut down.
    Shutdown,
    /// Not found.
    NotFound,
    /// A value this mirror does not name.
    #[default]
    #[serde(other)]
    Unknown,
}

/// One sub-agent's state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CollabAgentState {
    /// Where it stands.
    pub status: CollabAgentStatus,
    /// What it last said, if anything.
    pub message: Option<String>,
}

/// A call to a sub-agent tool.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CollabToolCallItem {
    /// The tool.
    pub tool: CollabTool,
    /// The thread that called it.
    pub sender_thread_id: String,
    /// The threads it addressed.
    pub receiver_thread_ids: Vec<String>,
    /// What it asked, if anything.
    pub prompt: Option<String>,
    /// Each addressed agent's state, by thread.
    ///
    /// Ordered, where codex's own map is not, so the same call is always written the same way.
    pub agents_states: BTreeMap<String, CollabAgentState>,
    /// Where the call stands.
    pub status: CollabToolCallStatus,
}

/// What an MCP tool answered.
///
/// The content is the tool's own JSON, which codex passes through unread too.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct McpToolCallItemResult {
    /// The answer's content blocks.
    pub content: Vec<Value>,
    /// The answer's metadata, when it carried any.
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
    /// The answer's structured content, when it carried any.
    pub structured_content: Option<Value>,
}

/// Why an MCP tool call failed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct McpToolCallItemError {
    /// The failure, in codex's words.
    pub message: String,
}

/// A call to an MCP tool.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct McpToolCallItem {
    /// The server.
    pub server: String,
    /// The tool.
    pub tool: String,
    /// The arguments, as the tool takes them.
    #[serde(default)]
    pub arguments: Value,
    /// What it answered.
    pub result: Option<McpToolCallItemResult>,
    /// Why it failed.
    pub error: Option<McpToolCallItemError>,
    /// Where the call stands.
    pub status: McpToolCallStatus,
}

/// A web search.
///
/// codex's own type also carries the search's id under `id`, which it writes beside the item's
/// own; a reader keeps only one of the two, so the mirror has only the item's (see
/// [`ThreadItem`]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WebSearchItem {
    /// What was searched for.
    pub query: String,
    /// What the search did.
    ///
    /// Read as [`WebSearchAction::Other`] when absent, as it is from a capture agentmux wrote
    /// before it recorded one; codex always writes it.
    #[serde(default)]
    pub action: WebSearchAction,
    /// What it found, as the search returned it, when it returned anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub results: Option<Vec<Value>>,
}

/// What a web search did, as codex's model protocol (`codex_protocol::models`) names it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WebSearchAction {
    /// Searched.
    Search {
        /// The query.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        query: Option<String>,
        /// The queries, when there were several.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        queries: Option<Vec<String>>,
    },
    /// Opened a page.
    OpenPage {
        /// The page.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
    },
    /// Searched within a page.
    FindInPage {
        /// The page.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        /// What was looked for.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pattern: Option<String>,
    },
    /// Anything else, which codex itself does not name.
    #[default]
    #[serde(other)]
    Other,
}

/// A warning that does not end the turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorItem {
    /// The warning, in codex's words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// One entry of the agent's to-do list.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TodoItem {
    /// The entry.
    pub text: String,
    /// Whether it is done.
    pub completed: bool,
}

/// The agent's to-do list.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TodoListItem {
    /// The entries.
    pub items: Vec<TodoItem>,
}
