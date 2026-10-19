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
use mirror::exec as mirror_exec;

/// One event.
#[must_use]
pub fn mirror_event(event: codex::ThreadEvent) -> mirror_exec::ThreadEvent {
    match event {
        codex::ThreadEvent::ThreadStarted(codex::ThreadStartedEvent { thread_id }) => {
            mirror_exec::ThreadEvent::ThreadStarted(mirror_exec::ThreadStartedEvent {
                thread_id: Some(thread_id),
            })
        }
        codex::ThreadEvent::TurnStarted(codex::TurnStartedEvent {}) => {
            mirror_exec::ThreadEvent::TurnStarted(mirror_exec::TurnStartedEvent {})
        }
        codex::ThreadEvent::TurnCompleted(codex::TurnCompletedEvent { usage }) => {
            mirror_exec::ThreadEvent::TurnCompleted(mirror_exec::TurnCompletedEvent {
                usage: Some(self::mirror_usage(usage)),
            })
        }
        codex::ThreadEvent::TurnFailed(codex::TurnFailedEvent { error }) => {
            mirror_exec::ThreadEvent::TurnFailed(mirror_exec::TurnFailedEvent {
                error: Some(self::mirror_error(error)),
            })
        }
        codex::ThreadEvent::ItemStarted(codex::ItemStartedEvent { item }) => {
            mirror_exec::ThreadEvent::ItemStarted(mirror_exec::ItemStartedEvent {
                item: self::mirror_item(item),
            })
        }
        codex::ThreadEvent::ItemUpdated(codex::ItemUpdatedEvent { item }) => {
            mirror_exec::ThreadEvent::ItemUpdated(mirror_exec::ItemUpdatedEvent {
                item: self::mirror_item(item),
            })
        }
        codex::ThreadEvent::ItemCompleted(codex::ItemCompletedEvent { item }) => {
            mirror_exec::ThreadEvent::ItemCompleted(mirror_exec::ItemCompletedEvent {
                item: self::mirror_item(item),
            })
        }
        codex::ThreadEvent::Error(error) => {
            mirror_exec::ThreadEvent::Error(self::mirror_error(error))
        }
    }
}

fn mirror_usage(usage: codex::Usage) -> mirror_exec::Usage {
    let codex::Usage {
        input_tokens,
        cached_input_tokens,
        cache_write_input_tokens,
        output_tokens,
        reasoning_output_tokens,
    } = usage;
    mirror_exec::Usage {
        input_tokens: Some(input_tokens),
        cached_input_tokens: Some(cached_input_tokens),
        cache_write_input_tokens: Some(cache_write_input_tokens),
        output_tokens: Some(output_tokens),
        reasoning_output_tokens: Some(reasoning_output_tokens),
    }
}

fn mirror_error(error: codex::ThreadErrorEvent) -> mirror_exec::ThreadErrorEvent {
    let codex::ThreadErrorEvent { message } = error;
    mirror_exec::ThreadErrorEvent {
        message: Some(message),
    }
}

fn mirror_item(item: codex::ExecThreadItem) -> mirror_exec::ThreadItem {
    let codex::ExecThreadItem { id, details } = item;
    // codex writes a web search's own id after the item's, under the same key, and a reader of
    // the line keeps the later one.
    let id = match &details {
        codex::ThreadItemDetails::WebSearch(search) => search.id.clone(),
        _ => id,
    };
    mirror_exec::ThreadItem {
        id: Some(id),
        details: self::mirror_item_details(details),
    }
}

fn mirror_item_details(details: codex::ThreadItemDetails) -> mirror_exec::ThreadItemDetails {
    use codex::ThreadItemDetails as CodexItemDetails;
    use mirror_exec::ThreadItemDetails as MirrorItemDetails;
    match details {
        CodexItemDetails::AgentMessage(codex::AgentMessageItem { text }) => {
            MirrorItemDetails::AgentMessage(mirror_exec::AgentMessageItem { text: Some(text) })
        }
        CodexItemDetails::Reasoning(codex::ReasoningItem { text }) => {
            MirrorItemDetails::Reasoning(mirror_exec::ReasoningItem { text })
        }
        CodexItemDetails::CommandExecution(codex::CommandExecutionItem {
            command,
            aggregated_output,
            exit_code,
            status,
        }) => MirrorItemDetails::CommandExecution(mirror_exec::CommandExecutionItem {
            command,
            aggregated_output,
            exit_code,
            status: match status {
                codex::CommandExecutionStatus::InProgress => {
                    mirror_exec::CommandExecutionStatus::InProgress
                }
                codex::CommandExecutionStatus::Completed => {
                    mirror_exec::CommandExecutionStatus::Completed
                }
                codex::CommandExecutionStatus::Failed => {
                    mirror_exec::CommandExecutionStatus::Failed
                }
                codex::CommandExecutionStatus::Declined => {
                    mirror_exec::CommandExecutionStatus::Declined
                }
            },
        }),
        CodexItemDetails::FileChange(codex::FileChangeItem { changes, status }) => {
            MirrorItemDetails::FileChange(mirror_exec::FileChangeItem {
                changes: changes
                    .into_iter()
                    .map(
                        |codex::FileUpdateChange { path, kind }| mirror_exec::FileUpdateChange {
                            path,
                            kind: match kind {
                                codex::PatchChangeKind::Add => mirror_exec::PatchChangeKind::Add,
                                codex::PatchChangeKind::Delete => {
                                    mirror_exec::PatchChangeKind::Delete
                                }
                                codex::PatchChangeKind::Update => {
                                    mirror_exec::PatchChangeKind::Update
                                }
                            },
                        },
                    )
                    .collect(),
                status: match status {
                    codex::PatchApplyStatus::InProgress => {
                        mirror_exec::PatchApplyStatus::InProgress
                    }
                    codex::PatchApplyStatus::Completed => mirror_exec::PatchApplyStatus::Completed,
                    codex::PatchApplyStatus::Failed => mirror_exec::PatchApplyStatus::Failed,
                },
            })
        }
        CodexItemDetails::McpToolCall(codex::McpToolCallItem {
            server,
            tool,
            arguments,
            result,
            error,
            status,
        }) => MirrorItemDetails::McpToolCall(mirror_exec::McpToolCallItem {
            server,
            tool,
            arguments,
            result: result.map(
                |codex::McpToolCallItemResult {
                     content,
                     meta,
                     structured_content,
                 }| mirror_exec::McpToolCallItemResult {
                    content,
                    meta,
                    structured_content,
                },
            ),
            error: error.map(|codex::McpToolCallItemError { message }| {
                mirror_exec::McpToolCallItemError { message }
            }),
            status: match status {
                codex::McpToolCallStatus::InProgress => mirror_exec::McpToolCallStatus::InProgress,
                codex::McpToolCallStatus::Completed => mirror_exec::McpToolCallStatus::Completed,
                codex::McpToolCallStatus::Failed => mirror_exec::McpToolCallStatus::Failed,
            },
        }),
        CodexItemDetails::CollabToolCall(codex::CollabToolCallItem {
            tool,
            sender_thread_id,
            receiver_thread_ids,
            prompt,
            agents_states,
            status,
        }) => MirrorItemDetails::CollabToolCall(mirror_exec::CollabToolCallItem {
            tool: match tool {
                codex::CollabTool::SpawnAgent => mirror_exec::CollabTool::SpawnAgent,
                codex::CollabTool::SendInput => mirror_exec::CollabTool::SendInput,
                codex::CollabTool::Wait => mirror_exec::CollabTool::Wait,
                codex::CollabTool::CloseAgent => mirror_exec::CollabTool::CloseAgent,
            },
            sender_thread_id,
            receiver_thread_ids,
            prompt,
            agents_states: agents_states
                .into_iter()
                .map(|(thread, codex::CollabAgentState { status, message })| {
                    let status = match status {
                        codex::CollabAgentStatus::PendingInit => {
                            mirror_exec::CollabAgentStatus::PendingInit
                        }
                        codex::CollabAgentStatus::Running => {
                            mirror_exec::CollabAgentStatus::Running
                        }
                        codex::CollabAgentStatus::Interrupted => {
                            mirror_exec::CollabAgentStatus::Interrupted
                        }
                        codex::CollabAgentStatus::Completed => {
                            mirror_exec::CollabAgentStatus::Completed
                        }
                        codex::CollabAgentStatus::Errored => {
                            mirror_exec::CollabAgentStatus::Errored
                        }
                        codex::CollabAgentStatus::Shutdown => {
                            mirror_exec::CollabAgentStatus::Shutdown
                        }
                        codex::CollabAgentStatus::NotFound => {
                            mirror_exec::CollabAgentStatus::NotFound
                        }
                    };
                    (thread, mirror_exec::CollabAgentState { status, message })
                })
                .collect(),
            status: match status {
                codex::CollabToolCallStatus::InProgress => {
                    mirror_exec::CollabToolCallStatus::InProgress
                }
                codex::CollabToolCallStatus::Completed => {
                    mirror_exec::CollabToolCallStatus::Completed
                }
                codex::CollabToolCallStatus::Failed => mirror_exec::CollabToolCallStatus::Failed,
            },
        }),
        // The search's own id shares the item's key on the wire; see `mirror_item`.
        CodexItemDetails::WebSearch(codex::WebSearchItem {
            id: _,
            query,
            action,
            results,
        }) => MirrorItemDetails::WebSearch(mirror_exec::WebSearchItem {
            query,
            action: match action {
                WebSearchAction::Search { query, queries } => {
                    mirror_exec::WebSearchAction::Search { query, queries }
                }
                WebSearchAction::OpenPage { url } => mirror_exec::WebSearchAction::OpenPage { url },
                WebSearchAction::FindInPage { url, pattern } => {
                    mirror_exec::WebSearchAction::FindInPage { url, pattern }
                }
                WebSearchAction::Other => mirror_exec::WebSearchAction::Other,
            },
            results,
        }),
        CodexItemDetails::TodoList(codex::TodoListItem { items }) => {
            MirrorItemDetails::TodoList(mirror_exec::TodoListItem {
                items: items
                    .into_iter()
                    .map(
                        |codex::TodoItem { text, completed }| mirror_exec::TodoItem {
                            text,
                            completed,
                        },
                    )
                    .collect(),
            })
        }
        CodexItemDetails::Error(codex::ErrorItem { message }) => {
            MirrorItemDetails::Error(mirror_exec::ErrorItem {
                message: Some(message),
            })
        }
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
        let item = |details| codex::ExecThreadItem {
            id: "item_1".to_owned(),
            details,
        };
        let command = |status, exit_code| {
            codex::ThreadItemDetails::CommandExecution(codex::CommandExecutionItem {
                command: "ls".to_owned(),
                aggregated_output: "out".to_owned(),
                exit_code,
                status,
            })
        };
        let mcp_tool_call = |status, has_result: bool| {
            codex::ThreadItemDetails::McpToolCall(codex::McpToolCallItem {
                server: "files".to_owned(),
                tool: "read".to_owned(),
                arguments: json!({"path": "a"}),
                result: has_result.then(|| codex::McpToolCallItemResult {
                    content: vec![json!({"type": "text", "text": "x"})],
                    meta: (status == codex::McpToolCallStatus::Completed).then(|| json!({"k": 1})),
                    structured_content: (status == codex::McpToolCallStatus::InProgress)
                        .then(|| json!({"rows": 1})),
                }),
                error: (!has_result).then(|| codex::McpToolCallItemError {
                    message: "no".to_owned(),
                }),
                status,
            })
        };
        let collaboration_tool_call = |tool, status, agent_status| {
            codex::ThreadItemDetails::CollabToolCall(codex::CollabToolCallItem {
                tool,
                sender_thread_id: "t0".to_owned(),
                receiver_thread_ids: vec!["t1".to_owned()],
                prompt: Some("go".to_owned()),
                agents_states: HashMap::from([(
                    "t1".to_owned(),
                    codex::CollabAgentState {
                        status: agent_status,
                        message: None,
                    },
                )]),
                status,
            })
        };
        let search = |action| {
            codex::ThreadItemDetails::WebSearch(codex::WebSearchItem {
                id: "ws_1".to_owned(),
                query: "rust".to_owned(),
                action,
                results: Some(vec![json!({"url": "u"})]),
            })
        };

        let details = vec![
            codex::ThreadItemDetails::AgentMessage(codex::AgentMessageItem {
                text: "hi".to_owned(),
            }),
            codex::ThreadItemDetails::Reasoning(codex::ReasoningItem {
                text: "thinking".to_owned(),
            }),
            command(codex::CommandExecutionStatus::InProgress, None),
            command(codex::CommandExecutionStatus::Completed, Some(0)),
            command(codex::CommandExecutionStatus::Failed, Some(1)),
            command(codex::CommandExecutionStatus::Declined, None),
            codex::ThreadItemDetails::FileChange(codex::FileChangeItem {
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
            codex::ThreadItemDetails::FileChange(codex::FileChangeItem {
                changes: Vec::new(),
                status: codex::PatchApplyStatus::Completed,
            }),
            codex::ThreadItemDetails::FileChange(codex::FileChangeItem {
                changes: Vec::new(),
                status: codex::PatchApplyStatus::Failed,
            }),
            mcp_tool_call(codex::McpToolCallStatus::InProgress, true),
            mcp_tool_call(codex::McpToolCallStatus::Completed, true),
            mcp_tool_call(codex::McpToolCallStatus::Failed, false),
            collaboration_tool_call(
                codex::CollabTool::SpawnAgent,
                codex::CollabToolCallStatus::InProgress,
                codex::CollabAgentStatus::PendingInit,
            ),
            collaboration_tool_call(
                codex::CollabTool::SendInput,
                codex::CollabToolCallStatus::Completed,
                codex::CollabAgentStatus::Running,
            ),
            collaboration_tool_call(
                codex::CollabTool::Wait,
                codex::CollabToolCallStatus::Failed,
                codex::CollabAgentStatus::Interrupted,
            ),
            collaboration_tool_call(
                codex::CollabTool::CloseAgent,
                codex::CollabToolCallStatus::Completed,
                codex::CollabAgentStatus::Completed,
            ),
            collaboration_tool_call(
                codex::CollabTool::Wait,
                codex::CollabToolCallStatus::Completed,
                codex::CollabAgentStatus::Errored,
            ),
            collaboration_tool_call(
                codex::CollabTool::Wait,
                codex::CollabToolCallStatus::Completed,
                codex::CollabAgentStatus::Shutdown,
            ),
            collaboration_tool_call(
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
            codex::ThreadItemDetails::TodoList(codex::TodoListItem {
                items: vec![codex::TodoItem {
                    text: "do".to_owned(),
                    completed: false,
                }],
            }),
            codex::ThreadItemDetails::Error(codex::ErrorItem {
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
            events.push(codex::ThreadEvent::ItemCompleted(
                codex::ItemCompletedEvent {
                    item: item(details),
                },
            ));
        }
        events
    }

    /// The mirror writes every one of codex's values exactly as codex writes it, and reads it
    /// back as the value it was converted to.
    #[gtest]
    fn the_mirror_writes_and_reads_what_codex_writes() -> Result<()> {
        for sample in samples() {
            let written = serde_json::to_value(&sample).or_fail()?;
            let mirrored = super::mirror_event(sample);
            expect_that!(serde_json::to_value(&mirrored).or_fail()?, eq(&written));
            expect_that!(
                serde_json::from_value::<mirror::exec::ThreadEvent>(written).or_fail()?,
                eq(&mirrored)
            );
        }
        Ok(())
    }
}
