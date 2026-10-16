//! The steps every conversation with codex takes, answered as codex answers them.

use codex_app_server_protocol::envelope::Message;
use codex_app_server_protocol::methods::{self as m, AnyClientRequest as Any};
use codex_app_server_protocol::types::{
    InitializeParams, ThreadResumeParams, ThreadStartParams, TurnStartParams,
};

use crate::fixtures;
use crate::{Peer, PeerError, Received};

/// What a client asked for as it opened its thread.
#[derive(Debug, Clone, PartialEq)]
pub struct Opened {
    /// Its `initialize` parameters.
    pub initialize: InitializeParams,
    /// How it opened the thread.
    pub thread: OpenedThread,
}

/// Whether a client started a thread or resumed one, with what it asked for.
#[derive(Debug, Clone, PartialEq)]
pub enum OpenedThread {
    /// `thread/start`.
    Started(ThreadStartParams),
    /// `thread/resume`.
    Resumed(ThreadResumeParams),
}

impl Peer {
    /// Answer the handshake, then open thread `thread` with whichever of `thread/start` and
    /// `thread/resume` the client sends.
    ///
    /// # Errors
    ///
    /// Returns [`PeerError::Unexpected`] when the client strays from that order, and the errors of
    /// the steps it takes.
    pub fn open_thread(&mut self, thread: &str) -> Result<Opened, PeerError> {
        let initialize = self.expect::<m::Initialize>()?;
        self.reply(&initialize, &fixtures::initialize_response())?;
        self.expect_notification::<m::Initialized>()?;
        let message = self.recv()?;
        let opened = match &message {
            Message::Request { id, method, params } => match Any::classify(method, params) {
                Some(Any::ThreadStart(params)) => {
                    let start = Received::decode(id.clone(), params)?;
                    self.reply(&start, &fixtures::thread_start_response(thread))?;
                    Some(OpenedThread::Started(start.params))
                }
                Some(Any::ThreadResume(params)) => {
                    let resume = Received::decode(id.clone(), params)?;
                    self.reply(&resume, &fixtures::thread_resume_response(thread))?;
                    Some(OpenedThread::Resumed(resume.params))
                }
                _ => None,
            },
            _ => None,
        };
        let Some(opened) = opened else {
            return Err(PeerError::Unexpected {
                expected: "a `thread/start` or `thread/resume` request".to_owned(),
                got: message.to_line(),
                received: self.received().to_vec(),
            });
        };
        Ok(Opened {
            initialize: initialize.params,
            thread: opened,
        })
    }

    /// Expect `turn/start`, answer it with turn `turn` running, and announce the turn with
    /// `turn/started`; returns what the client asked.
    ///
    /// # Errors
    ///
    /// Returns the errors of the steps it takes.
    pub fn start_turn(&mut self, thread: &str, turn: &str) -> Result<TurnStartParams, PeerError> {
        let start = self.expect::<m::TurnStart>()?;
        self.reply(&start, &fixtures::turn_start_response(turn))?;
        self.notify::<m::TurnStarted>(&fixtures::turn_started(thread, turn))?;
        Ok(start.params)
    }
}
