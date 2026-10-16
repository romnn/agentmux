//! Payloads as codex sends them, built from the generated types, and held to codex's own schema by
//! this crate's conformance test, since the types alone do not express every constraint.
//!
//! Each builder fills what codex's schema requires and leaves every optional member out, which is
//! the least a client can rely on.

use codex_app_server_protocol::types::{
    AgentMessageThreadItem, AgentMessageThreadItemType, ApprovalsReviewer, AskForApproval,
    AskForApproval0, CommandExecutionRequestApprovalParams, CommandExecutionStatus,
    CommandExecutionThreadItem, CommandExecutionThreadItemType, ErrorNotification,
    IdleThreadStatus, IdleThreadStatusType, InitializeResponse, ItemCompletedNotification,
    PlanThreadItem, PlanThreadItemType, ReadOnlySandboxPolicy, ReadOnlySandboxPolicyType,
    ReasoningThreadItem, ReasoningThreadItemType, SandboxPolicy, SessionSource, SessionSource0,
    Thread, ThreadItem, ThreadResumeResponse, ThreadStartResponse, ThreadStatus, ThreadTokenUsage,
    ThreadTokenUsageUpdatedNotification, TokenUsageBreakdown, Turn, TurnCompletedNotification,
    TurnError, TurnInterruptParams, TurnInterruptResponse, TurnStartResponse,
    TurnStartedNotification, TurnStatus, TurnSteerResponse, WarningNotification,
};

/// The thread a fake server opens, unless a test names another.
pub const THREAD: &str = "th-fake";

/// The turn a fake server starts, unless a test names another.
pub const TURN: &str = "tu-1";

/// The model a fake server reports.
pub const MODEL: &str = "gpt-6-astra";

/// What `initialize` answers.
#[must_use]
pub fn initialize_response() -> InitializeResponse {
    InitializeResponse {
        codex_home: "/home/dev/.codex".to_owned(),
        platform_family: "unix".to_owned(),
        platform_os: "linux".to_owned(),
        user_agent: "codex-app-server-mock".to_owned(),
    }
}

/// A thread with no turns, as `thread/start` and `thread/resume` describe it.
#[must_use]
pub fn thread(id: &str) -> Thread {
    Thread {
        agent_nickname: None,
        agent_role: None,
        cli_version: "0.162.1".to_owned(),
        created_at: 1_790_000_000,
        cwd: "/work/project".to_owned(),
        ephemeral: false,
        forked_from_id: None,
        git_info: None,
        history_mode: None,
        id: id.to_owned(),
        model: None,
        model_provider: "openai".to_owned(),
        name: None,
        originator: None,
        parent_thread_id: None,
        path: None,
        preview: String::new(),
        project_id: None,
        reasoning_effort: None,
        recency_at: None,
        section: None,
        section_entered_at: None,
        session_id: "se-fake".to_owned(),
        source: SessionSource::SessionSource0(Box::new(SessionSource0::AppServer)),
        status: ThreadStatus::IdleThreadStatus(Box::new(IdleThreadStatus {
            type_: IdleThreadStatusType::Idle,
        })),
        thread_source: None,
        turns: Vec::new(),
        updated_at: 1_790_000_000,
    }
}

/// `never`, the approval policy a thread nobody watches runs under.
fn never_ask() -> AskForApproval {
    AskForApproval::AskForApproval0(Box::new(AskForApproval0::Never))
}

fn read_only() -> SandboxPolicy {
    SandboxPolicy::ReadOnlySandboxPolicy(Box::new(ReadOnlySandboxPolicy {
        network_access: None,
        type_: ReadOnlySandboxPolicyType::ReadOnly,
    }))
}

/// What `thread/start` answers, for thread `id`.
#[must_use]
pub fn thread_start_response(id: &str) -> ThreadStartResponse {
    ThreadStartResponse {
        approval_policy: never_ask(),
        approvals_reviewer: ApprovalsReviewer::User,
        cwd: "/work/project".to_owned(),
        disabled_plugin_ids: None,
        instruction_sources: None,
        model: MODEL.to_owned(),
        model_provider: "openai".to_owned(),
        reasoning_effort: None,
        sandbox: read_only(),
        service_tier: None,
        thread: thread(id),
    }
}

/// What `thread/resume` answers, for thread `id`.
#[must_use]
pub fn thread_resume_response(id: &str) -> ThreadResumeResponse {
    ThreadResumeResponse {
        approval_policy: never_ask(),
        approvals_reviewer: ApprovalsReviewer::User,
        collaboration_mode: None,
        cwd: "/work/project".to_owned(),
        disabled_plugin_ids: None,
        instruction_sources: None,
        items_backwards_cursor: None,
        model: MODEL.to_owned(),
        model_provider: "openai".to_owned(),
        reasoning_effort: None,
        sandbox: read_only(),
        service_tier: None,
        thread: thread(id),
        turns_backwards_cursor: None,
    }
}

/// Turn `id` in `status`, holding `items`.
#[must_use]
pub fn turn(id: &str, status: TurnStatus, items: Vec<ThreadItem>) -> Turn {
    Turn {
        completed_at: None,
        duration_ms: None,
        error: None,
        id: id.to_owned(),
        items,
        items_view: None,
        root_turn_id: None,
        started_at: None,
        status,
    }
}

/// What `turn/start` answers: turn `id`, running.
#[must_use]
pub fn turn_start_response(id: &str) -> TurnStartResponse {
    TurnStartResponse {
        turn: turn(id, TurnStatus::InProgress, Vec::new()),
    }
}

/// `turn/started` for turn `turn` of thread `thread`.
#[must_use]
pub fn turn_started(thread: &str, turn_id: &str) -> TurnStartedNotification {
    TurnStartedNotification {
        thread_id: thread.to_owned(),
        turn: turn(turn_id, TurnStatus::InProgress, Vec::new()),
    }
}

/// `turn/completed` for turn `turn` of thread `thread`, ended in `status` holding `items`.
#[must_use]
pub fn turn_completed(
    thread: &str,
    turn_id: &str,
    status: TurnStatus,
    items: Vec<ThreadItem>,
) -> TurnCompletedNotification {
    TurnCompletedNotification {
        thread_id: thread.to_owned(),
        turn: turn(turn_id, status, items),
    }
}

/// What `turn/steer` answers when it takes the instruction.
#[must_use]
pub fn turn_steer_response(turn_id: &str) -> TurnSteerResponse {
    TurnSteerResponse {
        turn_id: turn_id.to_owned(),
    }
}

/// What `turn/interrupt` answers.
#[must_use]
pub fn turn_interrupt_response() -> TurnInterruptResponse {
    TurnInterruptResponse::default()
}

/// `turn/interrupt`'s parameters, as a client sends them.
#[must_use]
pub fn turn_interrupt_params(thread: &str, turn_id: &str) -> TurnInterruptParams {
    TurnInterruptParams {
        thread_id: thread.to_owned(),
        turn_id: turn_id.to_owned(),
    }
}

/// A reply of the delegate's.
#[must_use]
pub fn agent_message(id: &str, text: &str) -> ThreadItem {
    ThreadItem::AgentMessageThreadItem(Box::new(AgentMessageThreadItem {
        delivery: None,
        id: id.to_owned(),
        memory_citation: None,
        phase: None,
        questions: None,
        text: text.to_owned(),
        type_: AgentMessageThreadItemType::AgentMessage,
    }))
}

/// A plan of the delegate's.
#[must_use]
pub fn plan(id: &str, text: &str) -> ThreadItem {
    ThreadItem::PlanThreadItem(Box::new(PlanThreadItem {
        id: id.to_owned(),
        text: text.to_owned(),
        type_: PlanThreadItemType::Plan,
    }))
}

/// A reasoning summary.
#[must_use]
pub fn reasoning(id: &str, summary: &[&str]) -> ThreadItem {
    ThreadItem::ReasoningThreadItem(Box::new(ReasoningThreadItem {
        content: Some(Vec::new()),
        id: id.to_owned(),
        summary: Some(summary.iter().map(|&line| line.to_owned()).collect()),
        type_: ReasoningThreadItemType::Reasoning,
    }))
}

/// A command that ran to completion.
#[must_use]
pub fn command(id: &str, command: &str, output: &str, exit_code: i64) -> ThreadItem {
    ThreadItem::CommandExecutionThreadItem(Box::new(CommandExecutionThreadItem {
        aggregated_output: Some(output.to_owned()),
        command: command.to_owned(),
        command_actions: Vec::new(),
        cwd: "/work".to_owned(),
        duration_ms: Some(3),
        exit_code: Some(exit_code),
        id: id.to_owned(),
        plugin_id: None,
        process_id: None,
        script_path: None,
        source: None,
        status: CommandExecutionStatus::Completed,
        type_: CommandExecutionThreadItemType::CommandExecution,
    }))
}

/// `item/completed` for `item`, in turn `turn` of thread `thread`.
#[must_use]
pub fn item_completed(thread: &str, turn_id: &str, item: ThreadItem) -> ItemCompletedNotification {
    ItemCompletedNotification {
        completed_at_ms: 1,
        item,
        thread_id: thread.to_owned(),
        turn_id: turn_id.to_owned(),
    }
}

/// Token counts, as `thread/tokenUsage/updated` reports each figure.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tokens {
    /// Input tokens.
    pub input: i64,
    /// Input tokens served from the cache.
    pub cached_input: i64,
    /// Output tokens.
    pub output: i64,
    /// Output tokens spent reasoning.
    pub reasoning_output: i64,
}

impl Tokens {
    fn breakdown(self) -> TokenUsageBreakdown {
        TokenUsageBreakdown {
            cache_write_input_tokens: None,
            cached_input_tokens: self.cached_input,
            input_tokens: self.input,
            output_tokens: self.output,
            reasoning_output_tokens: self.reasoning_output,
            total_tokens: self.input + self.output,
        }
    }
}

/// `thread/tokenUsage/updated` for turn `turn` of thread `thread`.
#[must_use]
pub fn token_usage(
    thread: &str,
    turn_id: &str,
    last: Tokens,
    total: Tokens,
) -> ThreadTokenUsageUpdatedNotification {
    ThreadTokenUsageUpdatedNotification {
        thread_id: thread.to_owned(),
        token_usage: ThreadTokenUsage {
            last: last.breakdown(),
            model_context_window: None,
            total: total.breakdown(),
        },
        turn_id: turn_id.to_owned(),
    }
}

/// A `warning` about thread `thread`.
#[must_use]
pub fn warning(thread: &str, message: &str) -> WarningNotification {
    WarningNotification {
        message: message.to_owned(),
        thread_id: Some(thread.to_owned()),
    }
}

/// An `error` in turn `turn` of thread `thread`.
#[must_use]
pub fn error(thread: &str, turn_id: &str, will_retry: bool, message: &str) -> ErrorNotification {
    ErrorNotification {
        error: TurnError {
            additional_details: None,
            codex_error_info: None,
            message: message.to_owned(),
            misalignment: None,
        },
        thread_id: thread.to_owned(),
        turn_id: turn_id.to_owned(),
        will_retry,
    }
}

/// `item/commandExecution/requestApproval` for item `item` of turn `turn` in thread `thread`.
#[must_use]
pub fn command_approval(
    thread: &str,
    turn_id: &str,
    item: &str,
) -> CommandExecutionRequestApprovalParams {
    CommandExecutionRequestApprovalParams {
        approval_id: None,
        command: Some("rm -rf build".to_owned()),
        command_actions: None,
        cwd: None,
        environment_id: None,
        item_id: item.to_owned(),
        kind: None,
        network_approval_context: None,
        proposed_execpolicy_amendment: None,
        proposed_network_policy_amendments: None,
        reason: None,
        started_at_ms: 1,
        thread_id: thread.to_owned(),
        turn_id: turn_id.to_owned(),
    }
}
