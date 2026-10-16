//! The steps every conversation with codex takes, answered as codex answers them.

use codex_app_server_protocol::Request as _;
use codex_app_server_protocol::envelope::Message;
use codex_app_server_protocol::methods as m;
use codex_app_server_protocol::types::{
    InitializeParams, ThreadResumeParams, ThreadStartParams, TurnStartParams,
};

use crate::fixtures;
use crate::{Peer, PeerError};

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
        let opened = match self.recv()? {
            Message::Request { id, method, params } if method == m::ThreadStart::METHOD => {
                let params = m::ThreadStart::params(&params)
                    .map_err(|source| PeerError::Undecodable { method, source })?;
                self.send(&Message::result::<m::ThreadStart>(
                    id,
                    &fixtures::thread_start_response(thread),
                )?)?;
                OpenedThread::Started(params)
            }
            Message::Request { id, method, params } if method == m::ThreadResume::METHOD => {
                let params = m::ThreadResume::params(&params)
                    .map_err(|source| PeerError::Undecodable { method, source })?;
                self.send(&Message::result::<m::ThreadResume>(
                    id,
                    &fixtures::thread_resume_response(thread),
                )?)?;
                OpenedThread::Resumed(params)
            }
            other => {
                return Err(PeerError::Unexpected {
                    expected: "a `thread/start` or `thread/resume` request".to_owned(),
                    got: other.to_line(),
                    received: self.received().to_vec(),
                });
            }
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
