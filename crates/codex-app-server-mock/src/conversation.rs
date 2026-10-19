//! The steps every conversation with codex takes, answered as codex answers them.

use codex_app_server_protocol::envelope::Message;
use codex_app_server_protocol::methods::{self, AnyClientRequest};
use codex_app_server_protocol::types::{
    InitializeParams, ThreadResumeParams, ThreadStartParams, TurnStartParams,
};

use crate::fixtures;
use crate::{Peer, PeerError, ReceivedRequest};

/// What a client asked for as it opened its thread.
#[derive(Debug, Clone, PartialEq)]
pub struct ThreadSetupParams {
    /// Its `initialize` parameters.
    pub initialize_params: InitializeParams,
    /// How it opened the thread.
    pub thread_params: ThreadOpenParams,
}

/// Whether a client started a thread or resumed one, with what it asked for.
#[derive(Debug, Clone, PartialEq)]
pub enum ThreadOpenParams {
    /// `thread/start`.
    Start(ThreadStartParams),
    /// `thread/resume`.
    Resume(ThreadResumeParams),
}

impl Peer {
    /// Answer the handshake, then open thread `thread` with whichever of `thread/start` and
    /// `thread/resume` the client sends.
    ///
    /// # Errors
    ///
    /// Returns [`PeerError::Unexpected`] when the client strays from that order, and the errors of
    /// the steps it takes.
    pub fn open_thread(&mut self, thread_id: &str) -> Result<ThreadSetupParams, PeerError> {
        let initialize = self.expect_request::<methods::Initialize>()?;
        self.reply(&initialize, &fixtures::initialize_response())?;
        self.expect_notification::<methods::Initialized>()?;
        let message = self.receive_message()?;
        let opened = match &message {
            Message::Request { id, method, params } => {
                match AnyClientRequest::classify(method, params) {
                    Some(AnyClientRequest::ThreadStart(params)) => {
                        let start = ReceivedRequest::decode(id.clone(), params)?;
                        self.reply(&start, &fixtures::thread_start_response(thread_id))?;
                        Some(ThreadOpenParams::Start(start.params))
                    }
                    Some(AnyClientRequest::ThreadResume(params)) => {
                        let resume = ReceivedRequest::decode(id.clone(), params)?;
                        self.reply(&resume, &fixtures::thread_resume_response(thread_id))?;
                        Some(ThreadOpenParams::Resume(resume.params))
                    }
                    _ => None,
                }
            }
            _ => None,
        };
        let Some(opened) = opened else {
            return Err(PeerError::Unexpected {
                expected: "a `thread/start` or `thread/resume` request".to_owned(),
                actual_text: message.to_line(),
                received_lines: self.received_lines().to_vec(),
            });
        };
        Ok(ThreadSetupParams {
            initialize_params: initialize.params,
            thread_params: opened,
        })
    }

    /// Expect `turn/start`, answer it with turn `turn` running, and announce the turn with
    /// `turn/started`; returns what the client asked.
    ///
    /// # Errors
    ///
    /// Returns the errors of the steps it takes.
    pub fn start_turn(
        &mut self,
        thread_id: &str,
        turn_id: &str,
    ) -> Result<TurnStartParams, PeerError> {
        let start = self.expect_request::<methods::TurnStart>()?;
        self.reply(&start, &fixtures::turn_start_response(turn_id))?;
        self.notify::<methods::TurnStarted>(&fixtures::turn_started(thread_id, turn_id))?;
        Ok(start.params)
    }
}
