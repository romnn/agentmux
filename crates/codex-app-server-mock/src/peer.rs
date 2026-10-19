//! The server's end of one conversation, driven step by step by a test.

use std::io::{BufRead, Write};
use std::marker::PhantomData;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::Duration;

use codex_app_server_protocol::envelope::{EncodeError, Message, RequestId, RpcError};
use codex_app_server_protocol::{
    ClientNotification, ClientRequest, DecodeError, Method, RawParams, ServerNotification,
    ServerRequest, TypedParams, TypedResponse,
};
use serde_json::Value;

use crate::conformance::{self, Side};

/// How long a peer waits for the client before it gives up, unless told otherwise.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);

/// Why a step of a script could not be taken.
///
/// Each variant that waited on the client carries every line the client had sent by then, so a
/// failing test says what the conversation was.
#[derive(Debug, thiserror::Error)]
pub enum PeerError {
    /// The client sent nothing in time.
    #[error(
        "the client sent nothing within {waited:?}; it had sent {}",
        format_received_lines(.received_lines)
    )]
    Timeout {
        /// How long the peer waited.
        waited: Duration,
        /// Everything the client sent.
        received_lines: Vec<String>,
    },
    /// The client closed its output.
    #[error(
        "the client closed its output; it had sent {}",
        format_received_lines(.received_lines)
    )]
    Closed {
        /// Everything the client sent.
        received_lines: Vec<String>,
    },
    /// The client's output could not be read.
    #[error(
        "cannot read the client: {reason}; it had sent {}",
        format_received_lines(.received_lines)
    )]
    Read {
        /// What the read said.
        reason: String,
        /// Everything the client sent.
        received_lines: Vec<String>,
    },
    /// The client sent something other than what the script expected next.
    #[error(
        "expected {expected}, but the client sent {actual_text}; it had sent {}",
        format_received_lines(.received_lines)
    )]
    Unexpected {
        /// What the script expected.
        expected: String,
        /// The line that arrived instead.
        actual_text: String,
        /// Everything the client sent.
        received_lines: Vec<String>,
    },
    /// The client's payload does not match the type generated for it.
    #[error("the client's `{method}` does not match the protocol")]
    Undecodable {
        /// The method whose payload it was.
        method: String,
        /// Why.
        #[source]
        source: DecodeError,
    },
    /// The peer's own typed message breaks codex's schema, which a fixture must not.
    #[error("the peer's own `{method}` breaks codex's schema: {reason}")]
    Nonconforming {
        /// The message's method.
        method: String,
        /// How it breaks the schema.
        reason: String,
    },
    /// The peer's own payload could not be encoded.
    #[error(transparent)]
    Encode(#[from] EncodeError),
    /// The client could not be written to.
    #[error("cannot write to the client")]
    Write(#[source] std::io::Error),
}

/// Every line the client sent, as an error message lists them.
fn format_received_lines(received_lines: &[String]) -> String {
    if received_lines.is_empty() {
        return "nothing".to_owned();
    }
    let lines: Vec<String> = received_lines
        .iter()
        .map(|line| format!("\n  {line}"))
        .collect();
    lines.concat()
}

/// A request the client sent, decoded into its method's parameters.
#[derive(Debug)]
pub struct ReceivedRequest<M: TypedParams> {
    /// The client's id for it, which its answer must carry.
    pub id: RequestId,
    /// Its parameters.
    pub params: M::Params,
}

impl<M: ClientRequest + TypedParams> ReceivedRequest<M> {
    /// Request `M`, from its id and its parameters as they arrived.
    ///
    /// # Errors
    ///
    /// Returns [`PeerError::Undecodable`] when the parameters do not match `M`'s.
    pub fn decode(id: RequestId, params: RawParams<'_, M>) -> Result<Self, PeerError> {
        let params = params.decode().map_err(|source| PeerError::Undecodable {
            method: M::METHOD.to_owned(),
            source,
        })?;
        Ok(Self { id, params })
    }
}

/// A request the peer sent, whose answer [`Peer::expect_response`] waits for.
#[derive(Debug)]
pub struct SentRequest<M: Method> {
    /// The peer's id for it.
    pub id: RequestId,
    method: PhantomData<M>,
}

/// What the reading thread hands over.
enum LineReadResult {
    Text(String),
    Closed,
    Failed(String),
}

/// The reading thread, which reads one line each time it is asked to.
struct Reader {
    requests: Sender<()>,
    results: Receiver<LineReadResult>,
    /// Whether a line has been asked for and has not arrived yet.
    request_pending: bool,
}

/// The server's end of a conversation over newline-delimited JSON-RPC, scripted step by step.
///
/// Every step that waits on the client waits at most the peer's timeout, so a client that stops
/// talking fails the test rather than hanging it.
/// The peer reads only when its script waits for a message, so a script that stops asking is a
/// server that stops reading, and what the client writes then fills the pipe.
/// Typed steps build and check payloads through the generated protocol types, and what the peer
/// sends through them is checked against codex's own schema as well (see [`conformance`]);
/// [`Peer::send`] and [`Peer::send_bytes`] send anything at all, for a test of how the client
/// takes what codex does not send.
pub struct Peer {
    reader: Reader,
    to_client: Option<Box<dyn Write + Send>>,
    received_lines: Vec<String>,
    timeout: Duration,
}

impl Peer {
    /// A peer reading the client's messages from `from_client` and writing to `to_client`.
    ///
    /// The reading happens on a thread of its own, one line whenever the script waits for one,
    /// and ends when the client closes its output.
    pub fn new(
        from_client: impl BufRead + Send + 'static,
        to_client: impl Write + Send + 'static,
    ) -> Self {
        let (read_requests_sender, read_requests_receiver) = channel::<()>();
        let (read_results_sender, read_results_receiver) = channel();
        std::thread::spawn(move || {
            let mut from_client = from_client;
            for () in read_requests_receiver {
                let mut line = String::new();
                let read = match from_client.read_line(&mut line) {
                    Ok(0) => LineReadResult::Closed,
                    Ok(_) => LineReadResult::Text(line.trim_end_matches(['\n', '\r']).to_owned()),
                    Err(err) => LineReadResult::Failed(err.to_string()),
                };
                let last = !matches!(read, LineReadResult::Text(_));
                if read_results_sender.send(read).is_err() || last {
                    return;
                }
            }
        });
        Self {
            reader: Reader {
                requests: read_requests_sender,
                results: read_results_receiver,
                request_pending: false,
            },
            to_client: Some(Box::new(to_client)),
            received_lines: Vec::new(),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// The same peer, waiting at most `timeout` for each message.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Every line the client has sent so far, in order.
    #[must_use]
    pub fn received_lines(&self) -> &[String] {
        &self.received_lines
    }

    /// The client's next message.
    ///
    /// # Errors
    ///
    /// Returns [`PeerError::Timeout`] when none arrives in time, [`PeerError::Closed`] when the
    /// client closed its output, and [`PeerError::Unexpected`] for a line that is not JSON-RPC.
    pub fn receive_message(&mut self) -> Result<Message, PeerError> {
        match self.receive_message_within(self.timeout)? {
            Some(message) => Ok(message),
            None => Err(PeerError::Timeout {
                waited: self.timeout,
                received_lines: self.received_lines.clone(),
            }),
        }
    }

    /// The client's next message, or `None` when it sends none within `wait`.
    ///
    /// A wait that ends empty leaves its read in progress: the next line the client sends is read
    /// all the same, and taken by the next wait, so a peer that has waited once is reading until
    /// that line arrives.
    ///
    /// # Errors
    ///
    /// Returns [`PeerError::Closed`] when the client closed its output, and
    /// [`PeerError::Unexpected`] for a line that is not JSON-RPC.
    pub fn receive_message_within(&mut self, wait: Duration) -> Result<Option<Message>, PeerError> {
        let closed = || PeerError::Closed {
            received_lines: self.received_lines.clone(),
        };
        if !self.reader.request_pending {
            // The reading thread has gone once the client closed its output.
            self.reader.requests.send(()).map_err(|_| closed())?;
            self.reader.request_pending = true;
        }
        let line = match self.reader.results.recv_timeout(wait) {
            Ok(LineReadResult::Text(line)) => line,
            // Still asked for: the line is read when it comes, and taken by the next wait.
            Err(RecvTimeoutError::Timeout) => return Ok(None),
            Ok(LineReadResult::Closed) | Err(RecvTimeoutError::Disconnected) => {
                return Err(closed());
            }
            Ok(LineReadResult::Failed(reason)) => {
                return Err(PeerError::Read {
                    reason,
                    received_lines: self.received_lines.clone(),
                });
            }
        };
        self.reader.request_pending = false;
        self.received_lines.push(line.clone());
        match Message::parse(&line) {
            Ok(message) => Ok(Some(message)),
            Err(_) => Err(self.unexpected("a JSON-RPC message", line)),
        }
    }

    /// Wait for the client to close its output, which is how it ends a conversation.
    ///
    /// # Errors
    ///
    /// Returns [`PeerError::Unexpected`] when it sends anything first, and
    /// [`PeerError::Timeout`] when it does neither in time.
    pub fn expect_closed(&mut self) -> Result<(), PeerError> {
        match self.receive_message() {
            Err(PeerError::Closed { .. }) => Ok(()),
            Ok(message) => Err(self.unexpected("the client to close its output", message)),
            Err(err) => Err(err),
        }
    }

    /// Read and record whatever the client still sends until it closes its output, as a server
    /// that has nothing more to say does.
    ///
    /// # Errors
    ///
    /// Returns [`PeerError::Timeout`] when the client neither sends nor closes in time.
    pub fn drain_until_closed(&mut self) -> Result<(), PeerError> {
        loop {
            match self.receive_message() {
                Ok(_) | Err(PeerError::Unexpected { .. }) => {}
                Err(PeerError::Closed { .. }) => return Ok(()),
                Err(err) => return Err(err),
            }
        }
    }

    /// The client's next message, which must be request `M`.
    ///
    /// # Errors
    ///
    /// Returns [`PeerError::Unexpected`] for any other message, [`PeerError::Undecodable`] when
    /// its parameters do not match `M`'s, and the errors of [`Peer::receive_message`].
    pub fn expect_request<M: ClientRequest + TypedParams>(
        &mut self,
    ) -> Result<ReceivedRequest<M>, PeerError> {
        match self.receive_message()? {
            Message::Request { id, method, params } if method == M::METHOD => {
                ReceivedRequest::decode(id, RawParams::new(&params))
            }
            other => Err(self.unexpected(&format!("a `{}` request", M::METHOD), other)),
        }
    }

    /// The client's next message, which must be notification `M`; returns its parameters.
    ///
    /// # Errors
    ///
    /// Returns [`PeerError::Unexpected`] for any other message, [`PeerError::Undecodable`] when
    /// its parameters do not match `M`'s, and the errors of [`Peer::receive_message`].
    pub fn expect_notification<M: ClientNotification + TypedParams>(
        &mut self,
    ) -> Result<M::Params, PeerError> {
        match self.receive_message()? {
            Message::Notification { method, params } if method == M::METHOD => {
                <M as TypedParams>::decode_params(&params)
                    .map_err(|source| PeerError::Undecodable { method, source })
            }
            other => Err(self.unexpected(&format!("a `{}` notification", M::METHOD), other)),
        }
    }

    /// The client's next message, which must answer `request`; returns its result, decoded, or
    /// the error it answered with.
    ///
    /// # Errors
    ///
    /// Returns [`PeerError::Unexpected`] for any other message, [`PeerError::Undecodable`] when
    /// the result does not match `M`'s, and the errors of [`Peer::receive_message`].
    pub fn expect_response<M: ServerRequest + TypedResponse>(
        &mut self,
        request: &SentRequest<M>,
    ) -> Result<Result<M::Response, RpcError>, PeerError> {
        match self.expect_raw_response(&request.id)? {
            Ok(result) => {
                M::decode_response(&result)
                    .map(Ok)
                    .map_err(|source| PeerError::Undecodable {
                        method: M::METHOD.to_owned(),
                        source,
                    })
            }
            Err(error) => Ok(Err(error)),
        }
    }

    /// The client's next message, which must answer request `id`, undecoded.
    ///
    /// # Errors
    ///
    /// Returns [`PeerError::Unexpected`] for any other message, and the errors of
    /// [`Peer::receive_message`].
    pub fn expect_raw_response(
        &mut self,
        id: &RequestId,
    ) -> Result<Result<Value, RpcError>, PeerError> {
        match self.receive_message()? {
            Message::Response {
                id: response_id,
                outcome,
            } if response_id == *id => Ok(outcome),
            other => Err(self.unexpected(&format!("the answer to request {id}"), other)),
        }
    }

    /// Answer the client's request.
    ///
    /// # Errors
    ///
    /// Returns an error when the answer cannot be encoded or written.
    pub fn reply<M: ClientRequest + TypedResponse>(
        &mut self,
        request: &ReceivedRequest<M>,
        response: &M::Response,
    ) -> Result<(), PeerError> {
        self.send(&Message::success_response::<M>(
            request.id.clone(),
            response,
        )?)
    }

    /// Refuse the client's request with a JSON-RPC error.
    ///
    /// # Errors
    ///
    /// Returns [`PeerError::Write`] when the client cannot be written to.
    pub fn refuse<M: ClientRequest + TypedParams>(
        &mut self,
        request: &ReceivedRequest<M>,
        code: i64,
        message: &str,
    ) -> Result<(), PeerError> {
        self.send(&Message::error_response(request.id.clone(), code, message))
    }

    /// Send notification `M`.
    ///
    /// # Errors
    ///
    /// Returns an error when the notification cannot be encoded or written.
    pub fn notify<M: ServerNotification + TypedParams>(
        &mut self,
        params: &M::Params,
    ) -> Result<(), PeerError> {
        self.send_conforming(&Message::notification::<M>(params)?)
    }

    /// Send request `M` under `id`.
    ///
    /// # Errors
    ///
    /// Returns an error when the request cannot be encoded or written.
    pub fn request<M: ServerRequest + TypedParams>(
        &mut self,
        id: RequestId,
        params: &M::Params,
    ) -> Result<SentRequest<M>, PeerError> {
        self.send_conforming(&Message::request::<M>(id.clone(), params)?)?;
        Ok(SentRequest {
            id,
            method: PhantomData,
        })
    }

    /// Send a message the peer built through a method binding, once codex's own schema says it is
    /// one codex could send.
    fn send_conforming(&mut self, message: &Message) -> Result<(), PeerError> {
        let value = message.to_value();
        if let Err(reason) = conformance::check(Side::Server, &value) {
            return Err(PeerError::Nonconforming {
                method: value
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                reason,
            });
        }
        self.send(message)
    }

    /// Send any message, whatever its method or payload.
    ///
    /// # Errors
    ///
    /// Returns [`PeerError::Write`] when the client cannot be written to.
    pub fn send(&mut self, message: &Message) -> Result<(), PeerError> {
        let mut line = message.to_line().into_bytes();
        line.push(b'\n');
        self.send_bytes(&line)
    }

    /// Write bytes exactly as given, with no newline added: half a message, two at once, or
    /// something that is not JSON at all.
    ///
    /// # Errors
    ///
    /// Returns [`PeerError::Write`] when the client cannot be written to, or the peer's output
    /// is already closed.
    pub fn send_bytes(&mut self, bytes: &[u8]) -> Result<(), PeerError> {
        let Some(to_client) = self.to_client.as_mut() else {
            return Err(PeerError::Write(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "the peer closed its output",
            )));
        };
        to_client
            .write_all(bytes)
            .and_then(|()| to_client.flush())
            .map_err(PeerError::Write)
    }

    /// Close the peer's output, as a server that exits does; the client reads the end of it.
    pub fn close_output(&mut self) {
        self.to_client = None;
    }

    fn unexpected(&self, expected: &str, actual: impl Into<ReceivedText>) -> PeerError {
        PeerError::Unexpected {
            expected: expected.to_owned(),
            actual_text: actual.into().into_inner(),
            received_lines: self.received_lines.clone(),
        }
    }
}

/// What arrived instead of what a script expected, as an error message names it.
struct ReceivedText(String);

impl ReceivedText {
    fn into_inner(self) -> String {
        self.0
    }
}

impl From<Message> for ReceivedText {
    fn from(message: Message) -> Self {
        Self(message.to_line())
    }
}

impl From<String> for ReceivedText {
    fn from(line: String) -> Self {
        Self(line)
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::time::Duration;

    use codex_app_server_protocol::envelope::{Message, RequestId};
    use codex_app_server_protocol::methods;
    use googletest::prelude::*;

    use super::{Peer, PeerError};
    use crate::fixtures;

    struct Connection {
        peer: Peer,
        client_writer: std::io::PipeWriter,
        client_reader: BufReader<std::io::PipeReader>,
    }

    /// A peer wired to pipes, with the client's ends.
    fn test_connection() -> Result<Connection> {
        let (from_client, to_peer) = std::io::pipe().or_fail()?;
        let (from_peer, to_client) = std::io::pipe().or_fail()?;
        let peer =
            Peer::new(BufReader::new(from_client), to_client).with_timeout(Duration::from_secs(5));
        Ok(Connection {
            peer,
            client_writer: to_peer,
            client_reader: BufReader::new(from_peer),
        })
    }

    /// A request is received with its typed parameters, and answered under its own id.
    #[gtest]
    fn a_request_is_received_typed_and_answered_under_its_id() -> Result<()> {
        let Connection {
            mut peer,
            mut client_writer,
            mut client_reader,
        } = test_connection()?;
        let params = fixtures::turn_interrupt_params("th", "tu");
        let line = Message::request::<methods::TurnInterrupt>(RequestId::Integer(4), &params)
            .or_fail()?
            .to_line();
        writeln!(client_writer, "{line}").or_fail()?;

        let received = peer.expect_request::<methods::TurnInterrupt>().or_fail()?;
        assert_that!(received.params, eq(&params));
        peer.reply(&received, &fixtures::turn_interrupt_response())
            .or_fail()?;

        let mut answer = String::new();
        client_reader.read_line(&mut answer).or_fail()?;
        assert_that!(
            Message::parse(&answer),
            ok(matches_pattern!(Message::Response {
                id: eq(&RequestId::Integer(4)),
                ..
            }))
        );
        Ok(())
    }

    /// A message other than the one expected fails the step and names both.
    #[gtest]
    fn an_unexpected_message_names_what_arrived() -> Result<()> {
        let Connection {
            mut peer,
            mut client_writer,
            client_reader: _client_reader,
        } = test_connection()?;
        writeln!(
            client_writer,
            r#"{{"id":1,"method":"turn/steer","params":{{}}}}"#
        )
        .or_fail()?;

        let unexpected_message_error = peer
            .expect_request::<methods::TurnStart>()
            .err()
            .or_fail()?;
        assert_that!(
            unexpected_message_error,
            matches_pattern!(PeerError::Unexpected {
                expected: contains_substring("turn/start"),
                actual_text: contains_substring("turn/steer"),
                ..
            })
        );
        Ok(())
    }

    /// A read that fails is an error with what was read before it, not the end of the
    /// conversation.
    #[gtest]
    fn a_read_that_fails_is_not_the_end() -> Result<()> {
        let Connection {
            mut peer,
            mut client_writer,
            client_reader: _client_reader,
        } = test_connection()?;
        client_writer.write_all(b"\xff\xfe not text\n").or_fail()?;
        assert_that!(
            peer.receive_message(),
            err(matches_pattern!(PeerError::Read { .. }))
        );
        Ok(())
    }

    /// A wait that ends empty leaves its read in progress, and the line that arrives after it is
    /// the next message.
    #[gtest]
    fn a_line_after_an_empty_wait_is_the_next_message() -> Result<()> {
        let Connection {
            mut peer,
            mut client_writer,
            client_reader: _client_reader,
        } = test_connection()?;
        assert_that!(
            peer.receive_message_within(Duration::from_millis(50))
                .or_fail()?,
            none()
        );
        writeln!(client_writer, r#"{{"method":"initialized"}}"#).or_fail()?;
        assert_that!(
            peer.receive_message().or_fail()?,
            matches_pattern!(Message::Notification {
                method: eq("initialized"),
                ..
            })
        );
        Ok(())
    }

    /// A client that closes its output is seen to.
    #[gtest]
    fn a_client_that_closes_is_seen_to() -> Result<()> {
        let Connection {
            mut peer,
            client_writer,
            client_reader: _client_reader,
        } = test_connection()?;
        drop(client_writer);
        assert_that!(peer.expect_closed(), ok(anything()));
        Ok(())
    }
}
