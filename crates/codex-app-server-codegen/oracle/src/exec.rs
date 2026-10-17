//! Checks the mirror of the `codex exec --json` stream (`codex_app_server_protocol::exec`) against
//! codex's own types at the pinned release.
//!
//! Compiling this module is most of the check.
//! Each function converts one of codex's types into its mirror, destructuring codex's value with
//! every field named and building the mirror's with a struct literal, which must name every field
//! too.
//! A field either side adds, drops, renames or retypes, or a variant codex adds, is a build
//! error.
//! What the types cannot say — the names the fields and variants take on the wire — the test
//! below checks by serialising each of codex's values both directly and through its mirror.

use codex_exec as codex;
use codex_protocol::models::WebSearchAction;
use mirror::exec as ours;

/// One event.
#[must_use]
pub fn event(event: codex::ThreadEvent) -> ours::ThreadEvent {
    match event {
        codex::ThreadEvent::ThreadStarted(codex::ThreadStartedEvent { thread_id }) => {
            ours::ThreadEvent::ThreadStarted(ours::ThreadStartedEvent {
                thread_id: Some(thread_id),
            })
        }
        codex::ThreadEvent::TurnStarted(codex::TurnStartedEvent {}) => {
            ours::ThreadEvent::TurnStarted(ours::TurnStartedEvent {})
        }
        codex::ThreadEvent::TurnCompleted(codex::TurnCompletedEvent { usage }) => {
            ours::ThreadEvent::TurnCompleted(ours::TurnCompletedEvent {
                usage: Some(self::usage(usage)),
            })
        }
        codex::ThreadEvent::TurnFailed(codex::TurnFailedEvent { error }) => {
            ours::ThreadEvent::TurnFailed(ours::TurnFailedEvent {
                error: Some(self::error(error)),
            })
        }
        codex::ThreadEvent::ItemStarted(codex::ItemStartedEvent { item }) => {
            ours::ThreadEvent::ItemStarted(ours::ItemStartedEvent {
                item: self::item(item),
            })
        }
        codex::ThreadEvent::ItemUpdated(codex::ItemUpdatedEvent { item }) => {
            ours::ThreadEvent::ItemUpdated(ours::ItemUpdatedEvent {
                item: self::item(item),
            })
        }
        codex::ThreadEvent::ItemCompleted(codex::ItemCompletedEvent { item }) => {
            ours::ThreadEvent::ItemCompleted(ours::ItemCompletedEvent {
                item: self::item(item),
            })
        }
        codex::ThreadEvent::Error(error) => ours::ThreadEvent::Error(self::error(error)),
    }
}

fn usage(usage: codex::Usage) -> ours::Usage {
    let codex::Usage {
        input_tokens,
        cached_input_tokens,
        cache_write_input_tokens,
        output_tokens,
        reasoning_output_tokens,
    } = usage;
    ours::Usage {
        input_tokens: Some(input_tokens),
        cached_input_tokens: Some(cached_input_tokens),
        cache_write_input_tokens: Some(cache_write_input_tokens),
        output_tokens: Some(output_tokens),
        reasoning_output_tokens: Some(reasoning_output_tokens),
    }
}

fn error(error: codex::ThreadErrorEvent) -> ours::ThreadErrorEvent {
    let codex::ThreadErrorEvent { message } = error;
    ours::ThreadErrorEvent {
        message: Some(message),
    }
}

fn item(item: codex::ExecThreadItem) -> ours::ThreadItem {
    let codex::ExecThreadItem { id, details } = item;
    // codex writes a web search's own id after the item's, under the same key, and a reader of
    // the line keeps the later one.
    let id = match &details {
        codex::ThreadItemDetails::WebSearch(search) => search.id.clone(),
        _ => id,
    };
    ours::ThreadItem {
        id: Some(id),
        details: self::details(details),
    }
}

fn details(details: codex::ThreadItemDetails) -> ours::ThreadItemDetails {
    use codex::ThreadItemDetails as Codex;
    use ours::ThreadItemDetails as Ours;
    match details {
        Codex::AgentMessage(codex::AgentMessageItem { text }) => {
            Ours::AgentMessage(ours::AgentMessageItem { text: Some(text) })
        }
        Codex::Reasoning(codex::ReasoningItem { text }) => {
            Ours::Reasoning(ours::ReasoningItem { text })
        }
        Codex::CommandExecution(codex::CommandExecutionItem {
            command,
            aggregated_output,
            exit_code,
            status,
        }) => Ours::CommandExecution(ours::CommandExecutionItem {
            command,
            aggregated_output,
            exit_code,
            status: match status {
                codex::CommandExecutionStatus::InProgress => {
                    ours::CommandExecutionStatus::InProgress
                }
                codex::CommandExecutionStatus::Completed => ours::CommandExecutionStatus::Completed,
                codex::CommandExecutionStatus::Failed => ours::CommandExecutionStatus::Failed,
                codex::CommandExecutionStatus::Declined => ours::CommandExecutionStatus::Declined,
            },
        }),
        Codex::FileChange(codex::FileChangeItem { changes, status }) => {
            Ours::FileChange(ours::FileChangeItem {
                changes: changes
                    .into_iter()
                    .map(|codex::FileUpdateChange { path, kind }| ours::FileUpdateChange {
                        path,
                        kind: match kind {
                            codex::PatchChangeKind::Add => ours::PatchChangeKind::Add,
                            codex::PatchChangeKind::Delete => ours::PatchChangeKind::Delete,
                            codex::PatchChangeKind::Update => ours::PatchChangeKind::Update,
                        },
                    })
                    .collect(),
                status: match status {
                    codex::PatchApplyStatus::InProgress => ours::PatchApplyStatus::InProgress,
                    codex::PatchApplyStatus::Completed => ours::PatchApplyStatus::Completed,
                    codex::PatchApplyStatus::Failed => ours::PatchApplyStatus::Failed,
                },
            })
        }
        Codex::McpToolCall(codex::McpToolCallItem {
            server,
            tool,
            arguments,
            result,
            error,
            status,
        }) => Ours::McpToolCall(ours::McpToolCallItem {
            server,
            tool,
            arguments,
            result: result.map(
                |codex::McpToolCallItemResult {
                     content,
                     meta,
                     structured_content,
                 }| ours::McpToolCallItemResult {
                    content,
                    meta,
                    structured_content,
                },
            ),
            error: error.map(|codex::McpToolCallItemError { message }| {
                ours::McpToolCallItemError { message }
            }),
            status: match status {
                codex::McpToolCallStatus::InProgress => ours::McpToolCallStatus::InProgress,
                codex::McpToolCallStatus::Completed => ours::McpToolCallStatus::Completed,
                codex::McpToolCallStatus::Failed => ours::McpToolCallStatus::Failed,
            },
        }),
        Codex::CollabToolCall(codex::CollabToolCallItem {
            tool,
            sender_thread_id,
            receiver_thread_ids,
            prompt,
            agents_states,
            status,
        }) => Ours::CollabToolCall(ours::CollabToolCallItem {
            tool: match tool {
                codex::CollabTool::SpawnAgent => ours::CollabTool::SpawnAgent,
                codex::CollabTool::SendInput => ours::CollabTool::SendInput,
                codex::CollabTool::Wait => ours::CollabTool::Wait,
                codex::CollabTool::CloseAgent => ours::CollabTool::CloseAgent,
            },
            sender_thread_id,
            receiver_thread_ids,
            prompt,
            agents_states: agents_states
                .into_iter()
                .map(|(thread, codex::CollabAgentState { status, message })| {
                    let status = match status {
                        codex::CollabAgentStatus::PendingInit => {
                            ours::CollabAgentStatus::PendingInit
                        }
                        codex::CollabAgentStatus::Running => ours::CollabAgentStatus::Running,
                        codex::CollabAgentStatus::Interrupted => {
                            ours::CollabAgentStatus::Interrupted
                        }
                        codex::CollabAgentStatus::Completed => ours::CollabAgentStatus::Completed,
                        codex::CollabAgentStatus::Errored => ours::CollabAgentStatus::Errored,
                        codex::CollabAgentStatus::Shutdown => ours::CollabAgentStatus::Shutdown,
                        codex::CollabAgentStatus::NotFound => ours::CollabAgentStatus::NotFound,
                    };
                    (thread, ours::CollabAgentState { status, message })
                })
                .collect(),
            status: match status {
                codex::CollabToolCallStatus::InProgress => ours::CollabToolCallStatus::InProgress,
                codex::CollabToolCallStatus::Completed => ours::CollabToolCallStatus::Completed,
                codex::CollabToolCallStatus::Failed => ours::CollabToolCallStatus::Failed,
            },
        }),
        // The search's own id shares the item's key on the wire; see `item`.
        Codex::WebSearch(codex::WebSearchItem {
            id: _,
            query,
            action,
            results,
        }) => Ours::WebSearch(ours::WebSearchItem {
            query,
            action: match action {
                WebSearchAction::Search { query, queries } => {
                    ours::WebSearchAction::Search { query, queries }
                }
                WebSearchAction::OpenPage { url } => ours::WebSearchAction::OpenPage { url },
                WebSearchAction::FindInPage { url, pattern } => {
                    ours::WebSearchAction::FindInPage { url, pattern }
                }
                WebSearchAction::Other => ours::WebSearchAction::Other,
            },
            results,
        }),
        Codex::TodoList(codex::TodoListItem { items }) => Ours::TodoList(ours::TodoListItem {
            items: items
                .into_iter()
                .map(|codex::TodoItem { text, completed }| ours::TodoItem { text, completed })
                .collect(),
        }),
        Codex::Error(codex::ErrorItem { message }) => Ours::Error(ours::ErrorItem {
            message: Some(message),
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use codex_exec as codex;
    use codex_protocol::models::WebSearchAction;
    use googletest::prelude::*;
    use serde_json::json;

    /// One of codex's values for every event, every item and every value of every enum, each with
    /// its optional fields both set and unset somewhere.
    fn samples() -> Vec<codex::ThreadEvent> {
        use codex::ThreadItemDetails as D;

        let item = |details| codex::ExecThreadItem {
            id: "item_1".to_owned(),
            details,
        };
        let command = |status, exit_code| {
            D::CommandExecution(codex::CommandExecutionItem {
                command: "ls".to_owned(),
                aggregated_output: "out".to_owned(),
                exit_code,
                status,
            })
        };
        let mcp = |status, answered: bool| {
            D::McpToolCall(codex::McpToolCallItem {
                server: "files".to_owned(),
                tool: "read".to_owned(),
                arguments: json!({"path": "a"}),
                result: answered.then(|| codex::McpToolCallItemResult {
                    content: vec![json!({"type": "text", "text": "x"})],
                    meta: (status == codex::McpToolCallStatus::Completed).then(|| json!({"k": 1})),
                    structured_content: (status == codex::McpToolCallStatus::InProgress)
                        .then(|| json!({"rows": 1})),
                }),
                error: (!answered).then(|| codex::McpToolCallItemError {
                    message: "no".to_owned(),
                }),
                status,
            })
        };
        let collab = |tool, status, agent| {
            D::CollabToolCall(codex::CollabToolCallItem {
                tool,
                sender_thread_id: "t0".to_owned(),
                receiver_thread_ids: vec!["t1".to_owned()],
                prompt: Some("go".to_owned()),
                agents_states: HashMap::from([(
                    "t1".to_owned(),
                    codex::CollabAgentState {
                        status: agent,
                        message: None,
                    },
                )]),
                status,
            })
        };
        let search = |action| {
            D::WebSearch(codex::WebSearchItem {
                id: "ws_1".to_owned(),
                query: "rust".to_owned(),
                action,
                results: Some(vec![json!({"url": "u"})]),
            })
        };

        let details = vec![
            D::AgentMessage(codex::AgentMessageItem {
                text: "hi".to_owned(),
            }),
            D::Reasoning(codex::ReasoningItem {
                text: "thinking".to_owned(),
            }),
            command(codex::CommandExecutionStatus::InProgress, None),
            command(codex::CommandExecutionStatus::Completed, Some(0)),
            command(codex::CommandExecutionStatus::Failed, Some(1)),
            command(codex::CommandExecutionStatus::Declined, None),
            D::FileChange(codex::FileChangeItem {
                changes: vec![
                    codex::FileUpdateChange {
                        path: "a".to_owned(),
                        kind: codex::PatchChangeKind::Add,
                    },
                    codex::FileUpdateChange {
                        path: "b".to_owned(),
                        kind: codex::PatchChangeKind::Delete,
                    },
                    codex::FileUpdateChange {
                        path: "c".to_owned(),
                        kind: codex::PatchChangeKind::Update,
                    },
                ],
                status: codex::PatchApplyStatus::InProgress,
            }),
            D::FileChange(codex::FileChangeItem {
                changes: Vec::new(),
                status: codex::PatchApplyStatus::Completed,
            }),
            D::FileChange(codex::FileChangeItem {
                changes: Vec::new(),
                status: codex::PatchApplyStatus::Failed,
            }),
            mcp(codex::McpToolCallStatus::InProgress, true),
            mcp(codex::McpToolCallStatus::Completed, true),
            mcp(codex::McpToolCallStatus::Failed, false),
            collab(
                codex::CollabTool::SpawnAgent,
                codex::CollabToolCallStatus::InProgress,
                codex::CollabAgentStatus::PendingInit,
            ),
            collab(
                codex::CollabTool::SendInput,
                codex::CollabToolCallStatus::Completed,
                codex::CollabAgentStatus::Running,
            ),
            collab(
                codex::CollabTool::Wait,
                codex::CollabToolCallStatus::Failed,
                codex::CollabAgentStatus::Interrupted,
            ),
            collab(
                codex::CollabTool::CloseAgent,
                codex::CollabToolCallStatus::Completed,
                codex::CollabAgentStatus::Completed,
            ),
            collab(
                codex::CollabTool::Wait,
                codex::CollabToolCallStatus::Completed,
                codex::CollabAgentStatus::Errored,
            ),
            collab(
                codex::CollabTool::Wait,
                codex::CollabToolCallStatus::Completed,
                codex::CollabAgentStatus::Shutdown,
            ),
            collab(
                codex::CollabTool::Wait,
                codex::CollabToolCallStatus::Completed,
                codex::CollabAgentStatus::NotFound,
            ),
            search(WebSearchAction::Search {
                query: Some("rust".to_owned()),
                queries: Some(vec!["rust".to_owned()]),
            }),
            search(WebSearchAction::OpenPage { url: None }),
            search(WebSearchAction::FindInPage {
                url: Some("u".to_owned()),
                pattern: Some("p".to_owned()),
            }),
            search(WebSearchAction::Other),
            D::TodoList(codex::TodoListItem {
                items: vec![codex::TodoItem {
                    text: "do".to_owned(),
                    completed: false,
                }],
            }),
            D::Error(codex::ErrorItem {
                message: "careful".to_owned(),
            }),
        ];

        let mut events = vec![
            codex::ThreadEvent::ThreadStarted(codex::ThreadStartedEvent {
                thread_id: "t0".to_owned(),
            }),
            codex::ThreadEvent::TurnStarted(codex::TurnStartedEvent {}),
            codex::ThreadEvent::TurnCompleted(codex::TurnCompletedEvent {
                usage: codex::Usage {
                    input_tokens: 1,
                    cached_input_tokens: 2,
                    cache_write_input_tokens: 3,
                    output_tokens: 4,
                    reasoning_output_tokens: 5,
                },
            }),
            codex::ThreadEvent::TurnFailed(codex::TurnFailedEvent {
                error: codex::ThreadErrorEvent {
                    message: "bad".to_owned(),
                },
            }),
            codex::ThreadEvent::Error(codex::ThreadErrorEvent {
                message: "retrying".to_owned(),
            }),
        ];
        for details in details {
            events.push(codex::ThreadEvent::ItemStarted(codex::ItemStartedEvent {
                item: item(details.clone()),
            }));
            events.push(codex::ThreadEvent::ItemUpdated(codex::ItemUpdatedEvent {
                item: item(details.clone()),
            }));
            events.push(codex::ThreadEvent::ItemCompleted(codex::ItemCompletedEvent {
                item: item(details),
            }));
        }
        events
    }

    /// The mirror writes every one of codex's values exactly as codex writes it, and reads it
    /// back as the value it was converted to.
    #[gtest]
    fn the_mirror_writes_and_reads_what_codex_writes() -> Result<()> {
        for sample in samples() {
            let written = serde_json::to_value(&sample).or_fail()?;
            let mirrored = super::event(sample);
            expect_that!(serde_json::to_value(&mirrored).or_fail()?, eq(&written));
            expect_that!(
                serde_json::from_value::<mirror::exec::ThreadEvent>(written).or_fail()?,
                eq(&mirrored)
            );
        }
        Ok(())
    }
}
