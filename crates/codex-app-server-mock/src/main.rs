//! `codex`, faked: a [`Peer`] over the process's own stdin and stdout, playing the scenario its
//! configuration names (see [`codex_app_server_mock::scenario`]).
//!
//! It answers `codex app-server` and nothing else, and walks the conversation as codex does: the
//! handshake, the thread, one turn, and whatever ends it, since a client starts a server per turn.
//!
//! A turn's commands are what codex's sandbox would run: the fake starts them by running its own
//! executable again in a helper role, so they need nothing installed, and each records its own pid
//! once it is set up, so a test can see whether the client stopped them.

use std::io::{BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::Duration;

use codex_app_server_mock::fixtures::{self, THREAD_ID, TURN_ID};
use codex_app_server_mock::scenario::{CONFIG_FILE, Config, HELPER, Scenario, ServerExitMode};
use codex_app_server_mock::{Peer, PeerError, ReceivedRequest};
use codex_app_server_protocol::envelope::Message;
use codex_app_server_protocol::methods::{self, AnyClientRequest};
use codex_app_server_protocol::types::TurnStatus;

/// How long a command the fake starts runs unless something stops it.
const COMMAND_SECONDS: u64 = 300;

/// Why the fake stopped serving.
#[derive(Debug, thiserror::Error)]
enum Failure {
    #[error(transparent)]
    Peer(#[from] PeerError),
    #[error("cannot {operation}")]
    Io {
        operation: String,
        #[source]
        source: std::io::Error,
    },
}

impl Failure {
    fn io(operation: impl Into<String>) -> impl FnOnce(std::io::Error) -> Self {
        let operation = operation.into();
        move |source| Self::Io { operation, source }
    }
}

/// Run as `codex app-server`, or in one of the fake's own helper roles.
fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let outcome = match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        // What follows is the client's configuration of the server, which the fake does not use.
        ["app-server", ..] => serve(),
        [HELPER, role @ ..] => helper(role),
        _ => Err("only `app-server` is faked".to_owned()),
    };
    match outcome {
        Ok(code) => code,
        Err(reason) => {
            eprintln!("codex-app-server-mock: {reason}");
            ExitCode::from(2)
        }
    }
}

/// Serve the configured scenario on stdin and stdout until the client closes its output.
fn serve() -> Result<ExitCode, String> {
    let path = codex_home()?.join(CONFIG_FILE);
    let config = std::fs::read_to_string(&path)
        .map_err(|err| format!("cannot read {}: {err}", path.display()))
        .and_then(|json| {
            serde_json::from_str::<Config>(&json)
                .map_err(|err| format!("{} is not a scenario: {err}", path.display()))
        })?;
    if let Some(path) = &config.environment_dump_path {
        let environment: Vec<String> = std::env::vars()
            .map(|(name, value)| format!("{name}={value}\n"))
            .collect();
        std::fs::write(path, environment.concat())
            .map_err(|err| format!("cannot write {}: {err}", path.display()))?;
    }
    // Long enough for any test to steer a turn that is waiting for it.
    let peer = Peer::new(BufReader::new(std::io::stdin()), std::io::stdout())
        .with_timeout(Duration::from_secs(600));
    let mut server = Server {
        peer,
        config: &config,
        logged_line_count: 0,
    };
    let outcome = match &config.scenario {
        Scenario::Steerable => server.run_steerable_scenario(),
        Scenario::ServerExit {
            exit_mode,
            exit_trigger_path,
        } => server.run_exit_scenario(*exit_mode, exit_trigger_path),
        Scenario::OrphanedHelper {
            completion_trigger_path,
            helper_exit_trigger_path,
        } => server.run_orphaned_helper_scenario(completion_trigger_path, helper_exit_trigger_path),
    };
    // What the client said last is logged however the scenario ended.
    let outcome = server.append_received_lines().and(outcome);
    match outcome {
        Ok(code) => Ok(code),
        // The client ending the conversation is how a scenario ends.
        Err(Failure::Peer(PeerError::Closed { .. })) => Ok(ExitCode::SUCCESS),
        Err(Failure::Io { operation, source }) => Err(format!("cannot {operation}: {source}")),
        Err(Failure::Peer(err)) => Err(err.to_string()),
    }
}

struct Server<'a> {
    peer: Peer,
    config: &'a Config,
    /// How many of the client's lines are already in the log.
    logged_line_count: usize,
}

impl Server<'_> {
    /// Append what the client has sent since the last call to the log.
    fn append_received_lines(&mut self) -> Result<(), Failure> {
        if let Some(log) = &self.config.client_messages_path {
            let lines: Vec<String> = self
                .peer
                .received_lines()
                .iter()
                .skip(self.logged_line_count)
                .map(|line| format!("{line}\n"))
                .collect();
            append(log, &lines.concat()).map_err(Failure::io("append to the log"))?;
        }
        self.logged_line_count = self.peer.received_lines().len();
        Ok(())
    }

    /// Answer the handshake and open the thread, then wait for `turn/start`.
    fn setup_and_expect_turn_start(
        &mut self,
    ) -> Result<ReceivedRequest<methods::TurnStart>, Failure> {
        self.peer.open_thread(THREAD_ID)?;
        self.append_received_lines()?;
        let start = self.peer.expect_request::<methods::TurnStart>()?;
        self.append_received_lines()?;
        Ok(start)
    }

    fn end_turn(&mut self, status: TurnStatus) -> Result<(), PeerError> {
        self.peer
            .notify::<methods::TurnCompleted>(&fixtures::turn_completed(
                THREAD_ID,
                TURN_ID,
                status,
                Vec::new(),
            ))
    }

    fn run_steerable_scenario(&mut self) -> Result<ExitCode, Failure> {
        let start = self.setup_and_expect_turn_start()?;
        self.start_helper(&HelperBehavior::DetachedSleep)?;
        self.peer
            .reply(&start, &fixtures::turn_start_response(TURN_ID))?;
        self.peer
            .notify::<methods::TurnStarted>(&fixtures::turn_started(THREAD_ID, TURN_ID))?;
        // Ends once: what names the turn after that is refused, as codex refuses it.
        let mut running = true;
        loop {
            let message = self.peer.receive_message()?;
            self.append_received_lines()?;
            let Message::Request { id, method, params } = message else {
                continue;
            };
            match AnyClientRequest::classify(&method, &params) {
                Some(AnyClientRequest::TurnSteer(params)) => {
                    let steer = ReceivedRequest::decode(id, params)?;
                    let steer_params = &steer.params;
                    if !(running
                        && steer_params.thread_id == THREAD_ID
                        && steer_params.expected_turn_id == TURN_ID)
                    {
                        let refusal =
                            format!("no turn {} is running", steer_params.expected_turn_id);
                        self.peer.refuse(&steer, -32600, &refusal)?;
                        continue;
                    }
                    self.peer
                        .reply(&steer, &fixtures::turn_steer_response(TURN_ID))?;
                    let reply = fixtures::agent_message("i1", "STEERED: auth only");
                    self.peer
                        .notify::<methods::ItemCompleted>(&fixtures::item_completed(
                            THREAD_ID, TURN_ID, reply,
                        ))?;
                    self.end_turn(TurnStatus::Completed)?;
                    running = false;
                }
                Some(AnyClientRequest::TurnInterrupt(params)) => {
                    let interrupt = ReceivedRequest::decode(id, params)?;
                    let interrupt_params = &interrupt.params;
                    if !(running
                        && interrupt_params.thread_id == THREAD_ID
                        && interrupt_params.turn_id == TURN_ID)
                    {
                        let refusal = format!("no turn {} is running", interrupt_params.turn_id);
                        self.peer.refuse(&interrupt, -32600, &refusal)?;
                        continue;
                    }
                    self.peer
                        .reply(&interrupt, &fixtures::turn_interrupt_response())?;
                    self.end_turn(TurnStatus::Interrupted)?;
                    running = false;
                }
                _ => {}
            }
        }
    }

    fn run_exit_scenario(
        &mut self,
        exit_mode: ServerExitMode,
        exit_trigger_path: &Path,
    ) -> Result<ExitCode, Failure> {
        let start = self.setup_and_expect_turn_start()?;
        match exit_mode {
            ServerExitMode::Crash => self.start_helper(&HelperBehavior::DetachedSleep)?,
            ServerExitMode::LeaveOutputOpen => {
                self.peer
                    .reply(&start, &fixtures::turn_start_response(TURN_ID))?;
                self.start_helper(&HelperBehavior::HoldOutputOpen)?;
            }
            ServerExitMode::LeaveWarningWriter => {
                self.peer
                    .reply(&start, &fixtures::turn_start_response(TURN_ID))?;
                self.start_helper(&HelperBehavior::WriteWarnings)?;
            }
        }
        wait_until_path_exists(exit_trigger_path);
        Ok(match exit_mode {
            ServerExitMode::Crash => ExitCode::FAILURE,
            ServerExitMode::LeaveOutputOpen | ServerExitMode::LeaveWarningWriter => {
                ExitCode::SUCCESS
            }
        })
    }

    fn run_orphaned_helper_scenario(
        &mut self,
        completion_trigger_path: &Path,
        helper_exit_trigger_path: &Path,
    ) -> Result<ExitCode, Failure> {
        let start = self.setup_and_expect_turn_start()?;
        self.peer
            .reply(&start, &fixtures::turn_start_response(TURN_ID))?;
        let mut fork = self.spawn_helper(&HelperBehavior::SpawnOrphanHelper {
            exit_trigger_path: helper_exit_trigger_path.to_path_buf(),
        })?;
        // The fork exits as soon as its helper is started, which orphans the helper.
        fork.wait().map_err(Failure::io("wait for the fork"))?;
        wait_until_path_exists(completion_trigger_path);
        self.end_turn(TurnStatus::Completed)?;
        self.peer.drain_until_closed()?;
        Ok(ExitCode::SUCCESS)
    }

    /// Start a command in `role`, which records its own pid once it is set up.
    fn start_helper(&self, role: &HelperBehavior) -> Result<(), Failure> {
        self.spawn_helper(role).map(drop)
    }

    fn spawn_helper(&self, role: &HelperBehavior) -> Result<std::process::Child, Failure> {
        let executable_path =
            std::env::current_exe().map_err(Failure::io("find the fake's executable"))?;
        let mut command = Command::new(executable_path);
        command
            .arg(HELPER)
            .args(role.args(&self.config.command_pids_path))
            .stdin(Stdio::null());
        // A detached command holds nothing of the server's; the others keep its output open.
        if matches!(
            role,
            HelperBehavior::DetachedSleep | HelperBehavior::SpawnOrphanHelper { .. }
        ) {
            command.stdout(Stdio::null()).stderr(Stdio::null());
        }
        command.spawn().map_err(Failure::io("start a command"))
    }
}

/// What a command the fake starts does.
enum HelperBehavior {
    /// Sleeps in a session of its own, holding nothing of the server's.
    DetachedSleep,
    /// Sleeps, holding the server's output open.
    HoldOutputOpen,
    /// Writes a valid notification to the server's output every 20 ms until nobody reads it.
    WriteWarnings,
    /// Starts a helper that waits for `exit_trigger_path`, and exits at once, orphaning it.
    SpawnOrphanHelper { exit_trigger_path: PathBuf },
}

impl HelperBehavior {
    /// The helper's arguments: its role, and the file it records its pid in.
    fn args(&self, command_pids_path: &Path) -> Vec<String> {
        let command_pids_path = command_pids_path.to_string_lossy().into_owned();
        match self {
            Self::DetachedSleep => vec!["detached".to_owned(), command_pids_path],
            Self::HoldOutputOpen => vec!["linger".to_owned(), command_pids_path],
            Self::WriteWarnings => vec!["chatter".to_owned(), command_pids_path],
            Self::SpawnOrphanHelper { exit_trigger_path } => vec![
                "fork".to_owned(),
                command_pids_path,
                exit_trigger_path.to_string_lossy().into_owned(),
            ],
        }
    }
}

/// Run in a helper role, as the fake's own commands do.
fn helper(role: &[&str]) -> Result<ExitCode, String> {
    let record = |command_pids_path: &str| {
        append(
            Path::new(command_pids_path),
            &format!("{}\n", std::process::id()),
        )
        .map_err(|err| format!("cannot record the command: {err}"))
    };
    let sleep = || std::thread::sleep(Duration::from_secs(COMMAND_SECONDS));
    match role {
        ["detached", command_pids_path] => {
            // A session of its own, as codex's sandbox gives every command, which no signal to
            // the server's process group reaches; recorded only once it is in it.
            #[cfg(unix)]
            nix::unistd::setsid().map_err(|err| format!("setsid: {err}"))?;
            record(command_pids_path)?;
            sleep();
        }
        ["linger", command_pids_path] => {
            record(command_pids_path)?;
            sleep();
        }
        ["chatter", command_pids_path] => {
            record(command_pids_path)?;
            let warning = Message::notification::<methods::Warning>(&fixtures::warning(
                THREAD_ID,
                "still here",
            ))
            .map_err(|err| err.to_string())?
            .to_line();
            let mut stdout = std::io::stdout();
            while writeln!(stdout, "{warning}")
                .and_then(|()| stdout.flush())
                .is_ok()
            {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        ["fork", command_pids_path, exit_trigger_path] => {
            let executable_path = std::env::current_exe().map_err(|err| err.to_string())?;
            Command::new(executable_path)
                .args([HELPER, "helper", command_pids_path, exit_trigger_path])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|err| format!("cannot start the helper: {err}"))?;
        }
        ["helper", command_pids_path, exit_trigger_path] => {
            record(command_pids_path)?;
            wait_until_path_exists(Path::new(exit_trigger_path));
        }
        _ => return Err(format!("no helper role {role:?}")),
    }
    Ok(ExitCode::SUCCESS)
}

/// Where codex keeps its configuration: `$CODEX_HOME`, or `.codex` in the home directory.
fn codex_home() -> Result<PathBuf, String> {
    if let Some(home) = std::env::var_os("CODEX_HOME") {
        return Ok(PathBuf::from(home));
    }
    ["HOME", "USERPROFILE"]
        .into_iter()
        .find_map(std::env::var_os)
        .map(|home| PathBuf::from(home).join(".codex"))
        .ok_or_else(|| "neither CODEX_HOME nor a home directory is set".to_owned())
}

/// Wait for `path` to exist, for at most as long as a command runs.
fn wait_until_path_exists(path: &Path) {
    let deadline = std::time::Instant::now() + Duration::from_secs(COMMAND_SECONDS);
    while !path.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn append(path: &Path, text: &str) -> std::io::Result<()> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(text.as_bytes())
}
