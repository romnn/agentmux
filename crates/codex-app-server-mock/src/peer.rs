//! The server's end of one conversation, driven step by step by a test.

use std::io::{BufRead, Write};
use std::marker::PhantomData;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::Duration;

use codex_app_server_protocol::envelope::{EncodeError, Message, RequestId, RpcError};
use codex_app_server_protocol::{
    ClientNotification, ClientRequest, DecodeError, Notification, Request, ServerNotification,
    ServerRequest,
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
    #[error("the client sent nothing within {waited:?}; it had sent {}", transcript(.received))]
    Timeout {
        /// How long the peer waited.
        waited: Duration,
        /// Everything the client sent.
        received: Vec<String>,
    },
    /// The client closed its output.
    #[error("the client closed its output; it had sent {}", transcript(.received))]
    Closed {
        /// Everything the client sent.
        received: Vec<String>,
    },
    /// The client's output could not be read.
    #[error("cannot read the client: {reason}; it had sent {}", transcript(.received))]
    Read {
        /// What the read said.
        reason: String,
        /// Everything the client sent.
        received: Vec<String>,
    },
    /// The client sent something other than what the script expected next.
    #[error("expected {expected}, but the client sent {got}; it had sent {}", transcript(.received))]
    Unexpected {
        /// What the script expected.
        expected: String,
        /// The line that arrived instead.
        got: String,
        /// Everything the client sent.
        received: Vec<String>,
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
fn transcript(received: &[String]) -> String {
    if received.is_empty() {
        return "nothing".to_owned();
    }
    let lines: Vec<String> = received.iter().map(|line| format!("\n  {line}")).collect();
    lines.concat()
}

/// A request the client sent, decoded into its method's parameters.
#[derive(Debug)]
pub struct Received<M: Request> {
    /// The client's id for it, which its answer must carry.
    pub id: RequestId,
    /// Its parameters.
    pub params: M::Params,
}

impl<M: ClientRequest> Received<M> {
    /// Request `M`, from its id and its parameters as they arrived.
    ///
    /// # Errors
    ///
    /// Returns [`PeerError::Undecodable`] when the parameters do not match `M`'s.
    pub fn decode(id: RequestId, params: &Value) -> Result<Self, PeerError> {
        let params = M::params(params).map_err(|source| PeerError::Undecodable {
            method: M::METHOD.to_owned(),
            source,
        })?;
        Ok(Self { id, params })
    }
}

/// A request the peer sent, whose answer [`Peer::expect_answer`] waits for.
#[derive(Debug)]
pub struct Sent<M: Request> {
    /// The peer's id for it.
    pub id: RequestId,
    method: PhantomData<M>,
}

/// What the reading thread hands over.
enum Line {
    Text(String),
    Closed,
    Failed(String),
}

/// The reading thread, which reads one line each time it is asked to.
struct Reader {
    asks: Sender<()>,
    lines: Receiver<Line>,
    /// Whether a line has been asked for and has not arrived yet.
    asked: bool,
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
    received: Vec<String>,
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
        let (asks, asked) = channel::<()>();
        let (sender, lines) = channel();
        std::thread::spawn(move || {
            let mut from_client = from_client;
            for () in asked {
                let mut line = String::new();
                let read = match from_client.read_line(&mut line) {
                    Ok(0) => Line::Closed,
                    Ok(_) => Line::Text(line.trim_end_matches(['\n', '\r']).to_owned()),
                    Err(err) => Line::Failed(err.to_string()),
                };
                let last = !matches!(read, Line::Text(_));
                if sender.send(read).is_err() || last {
                    return;
                }
            }
        });
        Self {
            reader: Reader {
                asks,
                lines,
                asked: false,
            },
            to_client: Some(Box::new(to_client)),
            received: Vec::new(),
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
    pub fn received(&self) -> &[String] {
        &self.received
    }

    /// The client's next message.
    ///
    /// # Errors
    ///
    /// Returns [`PeerError::Timeout`] when none arrives in time, [`PeerError::Closed`] when the
    /// client closed its output, and [`PeerError::Unexpected`] for a line that is not JSON-RPC.
    pub fn recv(&mut self) -> Result<Message, PeerError> {
        match self.recv_within(self.timeout)? {
            Some(message) => Ok(message),
            None => Err(PeerError::Timeout {
                waited: self.timeout,
                received: self.received.clone(),
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
    pub fn recv_within(&mut self, wait: Duration) -> Result<Option<Message>, PeerError> {
        let closed = || PeerError::Closed {
            received: self.received.clone(),
        };
        if !self.reader.asked {
            // The reading thread has gone once the client closed its output.
            self.reader.asks.send(()).map_err(|_| closed())?;
            self.reader.asked = true;
        }
        let line = match self.reader.lines.recv_timeout(wait) {
            Ok(Line::Text(line)) => line,
            // Still asked for: the line is read when it comes, and taken by the next wait.
            Err(RecvTimeoutError::Timeout) => return Ok(None),
            Ok(Line::Closed) | Err(RecvTimeoutError::Disconnected) => return Err(closed()),
            Ok(Line::Failed(reason)) => {
                return Err(PeerError::Read {
                    reason,
                    received: self.received.clone(),
                });
            }
        };
        self.reader.asked = false;
        self.received.push(line.clone());
        match Message::parse(&line) {
            Some(message) => Ok(Some(message)),
            None => Err(self.unexpected("a JSON-RPC message", line)),
        }
    }

    /// Wait for the client to close its output, which is how it ends a conversation.
    ///
    /// # Errors
    ///
    /// Returns [`PeerError::Unexpected`] when it sends anything first, and
    /// [`PeerError::Timeout`] when it does neither in time.
    pub fn expect_closed(&mut self) -> Result<(), PeerError> {
        match self.recv() {
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
    pub fn wait_until_closed(&mut self) -> Result<(), PeerError> {
        loop {
            match self.recv() {
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
    /// its parameters do not match `M`'s, and the errors of [`Peer::recv`].
    pub fn expect<M: ClientRequest>(&mut self) -> Result<Received<M>, PeerError> {
        match self.recv()? {
            Message::Request { id, method, params } if method == M::METHOD => {
                Received::decode(id, &params)
            }
            other => Err(self.unexpected(&format!("a `{}` request", M::METHOD), other)),
        }
    }

    /// The client's next message, which must be notification `M`; returns its parameters.
    ///
    /// # Errors
    ///
    /// Returns [`PeerError::Unexpected`] for any other message, [`PeerError::Undecodable`] when
    /// its parameters do not match `M`'s, and the errors of [`Peer::recv`].
    pub fn expect_notification<M: ClientNotification>(&mut self) -> Result<M::Params, PeerError> {
        match self.recv()? {
            Message::Notification { method, params } if method == M::METHOD => {
                <M as Notification>::params(&params)
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
    /// the result does not match `M`'s, and the errors of [`Peer::recv`].
    pub fn expect_answer<M: ServerRequest>(
        &mut self,
        request: &Sent<M>,
    ) -> Result<Result<M::Response, RpcError>, PeerError> {
        match self.expect_raw_answer(&request.id)? {
            Ok(result) => M::response(&result)
                .map(Ok)
                .map_err(|source| PeerError::Undecodable {
                    method: M::METHOD.to_owned(),
                    source,
                }),
            Err(error) => Ok(Err(error)),
        }
    }

    /// The client's next message, which must answer request `id`, undecoded.
    ///
    /// # Errors
    ///
    /// Returns [`PeerError::Unexpected`] for any other message, and the errors of
    /// [`Peer::recv`].
    pub fn expect_raw_answer(
        &mut self,
        id: &RequestId,
    ) -> Result<Result<Value, RpcError>, PeerError> {
        match self.recv()? {
            Message::Response {
                id: answered,
                outcome,
            } if answered == *id => Ok(outcome),
            other => Err(self.unexpected(&format!("the answer to request {id}"), other)),
        }
    }

    /// Answer the client's request.
    ///
    /// # Errors
    ///
    /// Returns an error when the answer cannot be encoded or written.
    pub fn reply<M: ClientRequest>(
        &mut self,
        request: &Received<M>,
        response: &M::Response,
    ) -> Result<(), PeerError> {
        self.send(&Message::result::<M>(request.id.clone(), response)?)
    }

    /// Refuse the client's request with a JSON-RPC error.
    ///
    /// # Errors
    ///
    /// Returns [`PeerError::Write`] when the client cannot be written to.
    pub fn refuse<M: ClientRequest>(
        &mut self,
        request: &Received<M>,
        code: i64,
        message: &str,
    ) -> Result<(), PeerError> {
        self.send(&Message::error(request.id.clone(), code, message))
    }

    /// Send notification `M`.
    ///
    /// # Errors
    ///
    /// Returns an error when the notification cannot be encoded or written.
    pub fn notify<M: ServerNotification>(&mut self, params: &M::Params) -> Result<(), PeerError> {
        self.send_conforming(&Message::notification::<M>(params)?)
    }

    /// Send request `M` under `id`.
    ///
    /// # Errors
    ///
    /// Returns an error when the request cannot be encoded or written.
    pub fn request<M: ServerRequest>(
        &mut self,
        id: RequestId,
        params: &M::Params,
    ) -> Result<Sent<M>, PeerError> {
        self.send_conforming(&Message::request::<M>(id.clone(), params)?)?;
        Ok(Sent {
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
    pub fn close(&mut self) {
        self.to_client = None;
    }

    fn unexpected(&self, expected: &str, got: impl Into<Got>) -> PeerError {
        PeerError::Unexpected {
            expected: expected.to_owned(),
            got: got.into().0,
            received: self.received.clone(),
        }
    }
}

/// What arrived instead of what a script expected, as an error message names it.
struct Got(String);

impl From<Message> for Got {
    fn from(message: Message) -> Self {
        Self(message.to_line())
    }
}

impl From<String> for Got {
    fn from(line: String) -> Self {
        Self(line)
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::time::Duration;

    use codex_app_server_protocol::envelope::{Message, RequestId};
    use codex_app_server_protocol::methods as m;
    use googletest::prelude::*;

    use super::{Peer, PeerError};
    use crate::fixtures;

    /// A peer wired to pipes, with the client's ends.
    fn peer() -> Result<(Peer, std::io::PipeWriter, BufReader<std::io::PipeReader>)> {
        let (from_client, to_peer) = std::io::pipe().or_fail()?;
        let (from_peer, to_client) = std::io::pipe().or_fail()?;
        let peer =
            Peer::new(BufReader::new(from_client), to_client).with_timeout(Duration::from_secs(5));
        Ok((peer, to_peer, BufReader::new(from_peer)))
    }

    /// A request is received with its typed parameters, and answered under its own id.
    #[gtest]
    fn a_request_is_received_typed_and_answered_under_its_id() -> Result<()> {
        let (mut peer, mut client, mut answers) = peer()?;
        let params = fixtures::turn_interrupt_params("th", "tu");
        let line = Message::request::<m::TurnInterrupt>(RequestId::Integer(4), &params)
            .or_fail()?
            .to_line();
        writeln!(client, "{line}").or_fail()?;

        let received = peer.expect::<m::TurnInterrupt>().or_fail()?;
        assert_that!(received.params, eq(&params));
        peer.reply(&received, &fixtures::turn_interrupt_response())
            .or_fail()?;

        let mut answer = String::new();
        answers.read_line(&mut answer).or_fail()?;
        assert_that!(
            Message::parse(&answer),
            some(matches_pattern!(Message::Response {
                id: eq(&RequestId::Integer(4)),
                ..
            }))
        );
        Ok(())
    }

    /// A message other than the one expected fails the step and names both.
    #[gtest]
    fn an_unexpected_message_names_what_arrived() -> Result<()> {
        let (mut peer, mut client, _answers) = peer()?;
        writeln!(client, r#"{{"id":1,"method":"turn/steer","params":{{}}}}"#).or_fail()?;

        let refused = peer.expect::<m::TurnStart>().err().or_fail()?;
        assert_that!(
            refused,
            matches_pattern!(PeerError::Unexpected {
                expected: contains_substring("turn/start"),
                got: contains_substring("turn/steer"),
                ..
            })
        );
        Ok(())
    }

    /// A read that fails is an error with what was read before it, not the end of the
    /// conversation.
    #[gtest]
    fn a_read_that_fails_is_not_the_end() -> Result<()> {
        let (mut peer, mut client, _answers) = peer()?;
        client.write_all(b"\xff\xfe not text\n").or_fail()?;
        assert_that!(peer.recv(), err(matches_pattern!(PeerError::Read { .. })));
        Ok(())
    }

    /// A wait that ends empty leaves its read in progress, and the line that arrives after it is
    /// the next message.
    #[gtest]
    fn a_line_after_an_empty_wait_is_the_next_message() -> Result<()> {
        let (mut peer, mut client, _answers) = peer()?;
        assert_that!(
            peer.recv_within(Duration::from_millis(50)).or_fail()?,
            none()
        );
        writeln!(client, r#"{{"method":"initialized"}}"#).or_fail()?;
        assert_that!(
            peer.recv().or_fail()?,
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
        let (mut peer, client, _answers) = peer()?;
        drop(client);
        assert_that!(peer.expect_closed(), ok(anything()));
        Ok(())
    }
}
