//! The JSON-RPC envelope around the payloads: one message per line, in either direction.
//!
//! codex writes JSON-RPC without the `"jsonrpc"` member, so an incoming message is classified by
//! which of `id` and `method` it carries rather than by that; a message encoded here carries it,
//! which codex accepts.
//! Classification is deliberately lenient about the payload: a message is kept with its `params`
//! or `result` as raw JSON, and decoding that into a generated type is a separate step whose
//! failure the caller can record as drift rather than lose the message.

use std::fmt;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{Answered, Notification, Request, Typed};

/// JSON-RPC's code for a method the receiver does not offer.
pub const METHOD_NOT_FOUND: i64 = -32601;

/// JSON-RPC's code for a failure inside the receiver.
pub const INTERNAL_ERROR: i64 = -32603;

/// The id that pairs a request with its answer: an integer or a string, echoed back verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    /// An integer id, as agentmux's driver numbers its requests.
    Integer(i64),
    /// A string id.
    String(String),
}

impl From<i64> for RequestId {
    fn from(id: i64) -> Self {
        Self::Integer(id)
    }
}

impl From<&str> for RequestId {
    fn from(id: &str) -> Self {
        Self::String(id.to_owned())
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Integer(id) => write!(f, "{id}"),
            Self::String(id) => write!(f, "{id:?}"),
        }
    }
}

/// A JSON-RPC error answer.
#[derive(Debug, Clone, PartialEq)]
pub struct RpcError {
    /// The error code; `0` when the sender gave none.
    pub code: i64,
    /// What the sender said went wrong; empty when it said nothing.
    pub message: String,
    /// Anything else the sender attached.
    pub data: Option<Value>,
}

/// One JSON-RPC message.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    /// A request, which the receiver must answer with the same id.
    Request {
        /// The sender's id for it.
        id: RequestId,
        /// The method.
        method: String,
        /// The parameters, `null` when absent.
        params: Value,
    },
    /// A notification, which is not answered.
    Notification {
        /// The method.
        method: String,
        /// The parameters, `null` when absent.
        params: Value,
    },
    /// The answer to a request.
    Response {
        /// The id of the request it answers.
        id: RequestId,
        /// The result, or the error.
        outcome: Result<Value, RpcError>,
    },
}

impl Message {
    /// Classify one line.
    ///
    /// An `id` of `null` counts as absent.
    ///
    /// # Errors
    ///
    /// Returns [`EnvelopeError`] for a line that is no message at all, because nothing could
    /// answer or act on it: one that is not JSON, an id that is neither an integer nor a string, a
    /// `method` that is not a string, an answer that does not carry exactly one of a `result` and
    /// an `error` object, or a line with neither an id nor a method.
    pub fn parse(line: &str) -> Result<Self, EnvelopeError> {
        let message: Value = serde_json::from_str(line).map_err(EnvelopeError::NotJson)?;
        let id = match message.get("id") {
            None | Some(Value::Null) => None,
            Some(id) => Some(RequestId::deserialize(id).map_err(|_| EnvelopeError::Id)?),
        };
        let method = match message.get("method") {
            None => None,
            Some(method) => Some(method.as_str().ok_or(EnvelopeError::Method)?.to_owned()),
        };
        let params = || message.get("params").cloned().unwrap_or(Value::Null);
        match (id, method) {
            (Some(id), Some(method)) => Ok(Self::Request {
                id,
                method,
                params: params(),
            }),
            (Some(id), None) => {
                let outcome = match (message.get("result"), message.get("error")) {
                    (None, Some(Value::Object(error))) => Err(RpcError {
                        code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
                        message: error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        data: error.get("data").cloned(),
                    }),
                    (Some(result), None) => Ok(result.clone()),
                    _ => return Err(EnvelopeError::Answer),
                };
                Ok(Self::Response { id, outcome })
            }
            (None, Some(method)) => Ok(Self::Notification {
                method,
                params: params(),
            }),
            (None, None) => Err(EnvelopeError::Neither),
        }
    }

    /// The request for method `M`.
    ///
    /// # Errors
    ///
    /// Returns [`EncodeError`] when the parameters cannot be encoded as JSON.
    pub fn request<M: Request + Typed>(
        id: RequestId,
        params: &M::Params,
    ) -> Result<Self, EncodeError> {
        Ok(Self::Request {
            id,
            method: M::METHOD.to_owned(),
            params: encode(M::METHOD, params)?,
        })
    }

    /// The notification for method `M`.
    ///
    /// # Errors
    ///
    /// Returns [`EncodeError`] when the parameters cannot be encoded as JSON.
    pub fn notification<M: Notification + Typed>(params: &M::Params) -> Result<Self, EncodeError> {
        Ok(Self::Notification {
            method: M::METHOD.to_owned(),
            params: encode(M::METHOD, params)?,
        })
    }

    /// The successful answer to request `id` of method `M`.
    ///
    /// # Errors
    ///
    /// Returns [`EncodeError`] when the result cannot be encoded as JSON.
    pub fn result<M: Answered>(id: RequestId, response: &M::Response) -> Result<Self, EncodeError> {
        Ok(Self::Response {
            id,
            outcome: Ok(encode(M::METHOD, response)?),
        })
    }

    /// The error answer to request `id`.
    #[must_use]
    pub fn error(id: RequestId, code: i64, message: &str) -> Self {
        Self::Response {
            id,
            outcome: Err(RpcError {
                code,
                message: message.to_owned(),
                data: None,
            }),
        }
    }

    /// The message as the line the receiver reads, without its newline.
    #[must_use]
    pub fn to_line(&self) -> String {
        self.to_value().to_string()
    }

    /// The message as the JSON object [`Message::to_line`] writes.
    ///
    /// `params` that are `null` are left out, which is how a method without parameters is sent.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut line = serde_json::Map::new();
        line.insert("jsonrpc".to_owned(), json!("2.0"));
        match self {
            Self::Request { id, method, params } => {
                line.insert("id".to_owned(), json!(id));
                line.insert("method".to_owned(), json!(method));
                if !params.is_null() {
                    line.insert("params".to_owned(), params.clone());
                }
            }
            Self::Notification { method, params } => {
                line.insert("method".to_owned(), json!(method));
                if !params.is_null() {
                    line.insert("params".to_owned(), params.clone());
                }
            }
            Self::Response { id, outcome } => {
                line.insert("id".to_owned(), json!(id));
                let (key, value) = match outcome {
                    Ok(result) => ("result", result.clone()),
                    Err(RpcError {
                        code,
                        message,
                        data,
                    }) => {
                        let mut error = serde_json::Map::new();
                        error.insert("code".to_owned(), json!(code));
                        error.insert("message".to_owned(), json!(message));
                        if let Some(data) = data {
                            error.insert("data".to_owned(), data.clone());
                        }
                        ("error", Value::Object(error))
                    }
                };
                line.insert(key.to_owned(), value);
            }
        }
        Value::Object(line)
    }
}

/// Why a line is not a JSON-RPC message.
#[derive(Debug, thiserror::Error)]
pub enum EnvelopeError {
    /// It is not JSON.
    #[error("the line is not JSON")]
    NotJson(#[source] serde_json::Error),
    /// Its id is neither an integer nor a string.
    #[error("its id is neither an integer nor a string")]
    Id,
    /// Its method is not a string.
    #[error("its method is not a string")]
    Method,
    /// It answers a request without exactly one of a `result` and an `error` object.
    #[error("it answers a request without exactly one of a result and an error object")]
    Answer,
    /// It has neither an id nor a method.
    #[error("it has neither an id nor a method")]
    Neither,
}

/// A payload that could not be encoded as JSON.
#[derive(Debug, thiserror::Error)]
#[error("cannot encode the `{method}` payload")]
pub struct EncodeError {
    /// The method whose payload it was.
    pub method: &'static str,
    /// Why.
    #[source]
    pub source: serde_json::Error,
}

/// A payload that does not match the type generated for it, which means the sender speaks a
/// protocol other than the one the types were generated from.
#[derive(Debug, thiserror::Error)]
#[error(
    "codex sent a {type_name} that does not match the schema agentmux was generated from: \
     {reason}"
)]
pub struct DecodeError {
    /// The generated type's own name, without its path.
    pub type_name: &'static str,
    /// Why, which the message includes, because the reason is what a transcript records.
    pub reason: serde_json::Error,
}

fn encode(method: &'static str, payload: &impl Serialize) -> Result<Value, EncodeError> {
    serde_json::to_value(payload).map_err(|source| EncodeError { method, source })
}

/// Decode a payload into the type generated for it.
///
/// # Errors
///
/// Returns [`DecodeError`], naming the type, when the payload does not match it.
pub fn decode<T: DeserializeOwned>(payload: &Value) -> Result<T, DecodeError> {
    T::deserialize(payload).map_err(|reason| {
        let name = std::any::type_name::<T>();
        DecodeError {
            type_name: name.rsplit("::").next().unwrap_or(name),
            reason,
        }
    })
}

#[cfg(test)]
mod tests {
    use googletest::prelude::*;
    use serde_json::{Value, json};

    use super::{Message, RequestId, RpcError};
    use crate::{Answered, methods, types};

    /// Responses, notifications and requests are told apart by `id` and `method`, with or
    /// without the `jsonrpc` member codex leaves out.
    #[gtest]
    fn messages_are_classified_by_their_members() {
        assert_that!(
            Message::parse(r#"{"id":3,"result":{"turnId":"tu-2"}}"#),
            ok(eq(&Message::Response {
                id: RequestId::Integer(3),
                outcome: Ok(json!({"turnId": "tu-2"})),
            }))
        );
        assert_that!(
            Message::parse(r#"{"id":4,"error":{"code":-32600,"message":"no active turn"}}"#),
            ok(eq(&Message::Response {
                id: RequestId::Integer(4),
                outcome: Err(RpcError {
                    code: -32600,
                    message: "no active turn".to_owned(),
                    data: None,
                }),
            }))
        );
        assert_that!(
            Message::parse(r#"{"method":"turn/completed","params":{"threadId":"t"}}"#),
            ok(eq(&Message::Notification {
                method: "turn/completed".to_owned(),
                params: json!({"threadId": "t"}),
            }))
        );
        assert_that!(
            Message::parse(
                r#"{"jsonrpc":"2.0","id":"srv-1","method":"item/fileChange/requestApproval","params":{}}"#
            ),
            ok(eq(&Message::Request {
                id: RequestId::String("srv-1".to_owned()),
                method: "item/fileChange/requestApproval".to_owned(),
                params: json!({}),
            }))
        );
        // A null id is no id, so this is a notification.
        assert_that!(
            Message::parse(r#"{"id":null,"method":"warning"}"#),
            ok(eq(&Message::Notification {
                method: "warning".to_owned(),
                params: Value::Null,
            }))
        );
        // A result of `null` is still a result.
        assert_that!(
            Message::parse(r#"{"id":5,"result":null}"#),
            ok(eq(&Message::Response {
                id: RequestId::Integer(5),
                outcome: Ok(Value::Null),
            }))
        );
        // Lines that are not JSON-RPC messages, including answers nothing could act on
        for line in [
            "warning: something",
            r#"{"neither":true}"#,
            r#"{"id":{"nested":1},"result":{}}"#,
            r#"{"id":4}"#,
            r#"{"id":4,"error":"no"}"#,
            r#"{"id":4,"result":{},"error":{"code":1,"message":"both"}}"#,
            r#"{"id":4,"method":5}"#,
            r#"{"method":["turn/completed"]}"#,
        ] {
            expect_that!(Message::parse(line), err(anything()), "{line}");
        }
    }

    /// A request is encoded with its method and its parameters, on one line.
    #[gtest]
    fn a_request_is_one_line_with_its_method() -> Result<()> {
        let params = types::TurnInterruptParams {
            thread_id: "th-1".to_owned(),
            turn_id: "tu-2".to_owned(),
        };
        let line = Message::request::<methods::TurnInterrupt>(RequestId::Integer(7), &params)
            .or_fail()?
            .to_line();

        assert_that!(
            serde_json::from_str::<Value>(&line).or_fail()?,
            eq(&json!({
                "jsonrpc": "2.0",
                "id": 7,
                "method": "turn/interrupt",
                "params": {"threadId": "th-1", "turnId": "tu-2"},
            }))
        );
        assert_that!(line.contains('\n'), eq(false));
        Ok(())
    }

    /// A method without parameters is sent without `params`.
    #[gtest]
    fn a_parameterless_notification_carries_no_params() -> Result<()> {
        let line = Message::notification::<methods::Initialized>(&())
            .or_fail()?
            .to_line();
        assert_that!(line, eq(r#"{"jsonrpc":"2.0","method":"initialized"}"#));
        Ok(())
    }

    /// Every message encodes to a line that parses back to the same message.
    #[gtest]
    fn encoding_and_parsing_round_trip() {
        let messages = [
            Message::Request {
                id: RequestId::String("s".to_owned()),
                method: "m".to_owned(),
                params: json!({"a": 1}),
            },
            Message::Notification {
                method: "n".to_owned(),
                params: json!([1]),
            },
            Message::Response {
                id: RequestId::Integer(9),
                outcome: Ok(json!({})),
            },
            Message::Response {
                id: RequestId::Integer(10),
                outcome: Err(RpcError {
                    code: -1,
                    message: "no".to_owned(),
                    data: Some(json!({"why": "because"})),
                }),
            },
        ];
        for message in messages {
            expect_that!(Message::parse(&message.to_line()), ok(eq(&message)));
        }
    }

    /// A payload that no longer matches its type names the type it failed to decode into and
    /// what did not match.
    #[gtest]
    fn an_undecodable_payload_names_its_type_and_why() {
        let decoded = methods::TurnStart::response(
            &json!({"turn": {"id": "tu", "items": [], "status": "sideways"}}),
        );
        assert_that!(
            decoded,
            err(displays_as(all![
                starts_with("codex sent a TurnStartResponse that does not match"),
                contains_substring("unknown variant `sideways`"),
            ]))
        );
    }
}
