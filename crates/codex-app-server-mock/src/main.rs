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

use codex_app_server_mock::fixtures::{self, THREAD, TURN};
use codex_app_server_mock::scenario::{CONFIG_FILE, Config, Death, HELPER, Scenario};
use codex_app_server_mock::{Peer, PeerError, Received};
use codex_app_server_protocol::envelope::Message;
use codex_app_server_protocol::methods::{self as m, AnyClientRequest as Any};
use codex_app_server_protocol::types::TurnStatus;

/// How long a command the fake starts runs unless something stops it.
const COMMAND_SECONDS: u64 = 300;

/// Why the fake stopped serving.
#[derive(Debug, thiserror::Error)]
enum Failure {
    #[error(transparent)]
    Peer(#[from] PeerError),
    #[error("cannot {what}")]
    Io {
        what: String,
        #[source]
        source: std::io::Error,
    },
}

impl Failure {
    fn io(what: impl Into<String>) -> impl FnOnce(std::io::Error) -> Self {
        let what = what.into();
        move |source| Self::Io { what, source }
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
    if let Some(path) = &config.environment {
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
        logged: 0,
    };
    let outcome = match &config.scenario {
        Scenario::Steerable => server.steerable(),
        Scenario::Dying { death, release } => server.dying(*death, release),
        Scenario::Forking {
            release,
            helper_release,
        } => server.forking(release, helper_release),
    };
    // What the client said last is logged however the scenario ended.
    let outcome = server.log().and(outcome);
    match outcome {
        Ok(code) => Ok(code),
        // The client ending the conversation is how a scenario ends.
        Err(Failure::Peer(PeerError::Closed { .. })) => Ok(ExitCode::SUCCESS),
        Err(Failure::Io { what, source }) => Err(format!("cannot {what}: {source}")),
        Err(Failure::Peer(err)) => Err(err.to_string()),
    }
}

struct Server<'a> {
    peer: Peer,
    config: &'a Config,
    /// How many of the client's lines are already in the log.
    logged: usize,
}

impl Server<'_> {
    /// Append what the client has sent since the last call to the log.
    fn log(&mut self) -> Result<(), Failure> {
        if let Some(log) = &self.config.log {
            let lines: Vec<String> = self
                .peer
                .received()
                .iter()
                .skip(self.logged)
                .map(|line| format!("{line}\n"))
                .collect();
            append(log, &lines.concat()).map_err(Failure::io("append to the log"))?;
        }
        self.logged = self.peer.received().len();
        Ok(())
    }

    /// Answer the handshake and open the thread, then wait for `turn/start`.
    fn until_turn_start(&mut self) -> Result<Received<m::TurnStart>, Failure> {
        self.peer.open_thread(THREAD)?;
        self.log()?;
        let start = self.peer.expect::<m::TurnStart>()?;
        self.log()?;
        Ok(start)
    }

    fn end_turn(&mut self, status: TurnStatus) -> Result<(), PeerError> {
        self.peer
            .notify::<m::TurnCompleted>(&fixtures::turn_completed(THREAD, TURN, status, Vec::new()))
    }

    fn steerable(&mut self) -> Result<ExitCode, Failure> {
        let start = self.until_turn_start()?;
        self.start(&Role::Detached)?;
        self.peer
            .reply(&start, &fixtures::turn_start_response(TURN))?;
        self.peer
            .notify::<m::TurnStarted>(&fixtures::turn_started(THREAD, TURN))?;
        // Ends once: what names the turn after that is refused, as codex refuses it.
        let mut running = true;
        loop {
            let message = self.peer.recv()?;
            self.log()?;
            let Message::Request { id, method, params } = message else {
                continue;
            };
            match Any::classify(&method, &params) {
                Some(Any::TurnSteer(params)) => {
                    let steer = Received::decode(id, params)?;
                    let named = &steer.params;
                    if !(running && named.thread_id == THREAD && named.expected_turn_id == TURN) {
                        let refusal = format!("no turn {} is running", named.expected_turn_id);
                        self.peer.refuse(&steer, -32600, &refusal)?;
                        continue;
                    }
                    self.peer
                        .reply(&steer, &fixtures::turn_steer_response(TURN))?;
                    let reply = fixtures::agent_message("i1", "STEERED: auth only");
                    self.peer
                        .notify::<m::ItemCompleted>(&fixtures::item_completed(
                            THREAD, TURN, reply,
                        ))?;
                    self.end_turn(TurnStatus::Completed)?;
                    running = false;
                }
                Some(Any::TurnInterrupt(params)) => {
                    let interrupt = Received::decode(id, params)?;
                    let named = &interrupt.params;
                    if !(running && named.thread_id == THREAD && named.turn_id == TURN) {
                        let refusal = format!("no turn {} is running", named.turn_id);
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

    fn dying(&mut self, death: Death, release: &Path) -> Result<ExitCode, Failure> {
        let start = self.until_turn_start()?;
        match death {
            Death::Crash => self.start(&Role::Detached)?,
            Death::Linger => {
                self.peer
                    .reply(&start, &fixtures::turn_start_response(TURN))?;
                self.start(&Role::Linger)?;
            }
            Death::Chatter => {
                self.peer
                    .reply(&start, &fixtures::turn_start_response(TURN))?;
                self.start(&Role::Chatter)?;
            }
        }
        wait_for(release);
        Ok(match death {
            Death::Crash => ExitCode::FAILURE,
            Death::Linger | Death::Chatter => ExitCode::SUCCESS,
        })
    }

    fn forking(&mut self, release: &Path, helper_release: &Path) -> Result<ExitCode, Failure> {
        let start = self.until_turn_start()?;
        self.peer
            .reply(&start, &fixtures::turn_start_response(TURN))?;
        let mut fork = self.spawn(&Role::Fork {
            release: helper_release.to_path_buf(),
        })?;
        // The fork exits as soon as its helper is started, which orphans the helper.
        fork.wait().map_err(Failure::io("wait for the fork"))?;
        wait_for(release);
        self.end_turn(TurnStatus::Completed)?;
        self.peer.wait_until_closed()?;
        Ok(ExitCode::SUCCESS)
    }

    /// Start a command in `role`, which records its own pid once it is set up.
    fn start(&self, role: &Role) -> Result<(), Failure> {
        self.spawn(role).map(drop)
    }

    fn spawn(&self, role: &Role) -> Result<std::process::Child, Failure> {
        let exe = std::env::current_exe().map_err(Failure::io("find the fake's executable"))?;
        let mut command = Command::new(exe);
        command
            .arg(HELPER)
            .args(role.args(&self.config.commands))
            .stdin(Stdio::null());
        // A detached command holds nothing of the server's; the others keep its output open.
        if matches!(role, Role::Detached | Role::Fork { .. }) {
            command.stdout(Stdio::null()).stderr(Stdio::null());
        }
        command.spawn().map_err(Failure::io("start a command"))
    }
}

/// What a command the fake starts does.
enum Role {
    /// Sleeps in a session of its own, holding nothing of the server's.
    Detached,
    /// Sleeps, holding the server's output open.
    Linger,
    /// Writes a valid notification to the server's output every 20 ms until nobody reads it.
    Chatter,
    /// Starts a helper that waits for `release`, and exits at once, orphaning it.
    Fork { release: PathBuf },
}

impl Role {
    /// The helper's arguments: its role, and the file it records its pid in.
    fn args(&self, commands: &Path) -> Vec<String> {
        let commands = commands.to_string_lossy().into_owned();
        match self {
            Self::Detached => vec!["detached".to_owned(), commands],
            Self::Linger => vec!["linger".to_owned(), commands],
            Self::Chatter => vec!["chatter".to_owned(), commands],
            Self::Fork { release } => vec![
                "fork".to_owned(),
                commands,
                release.to_string_lossy().into_owned(),
            ],
        }
    }
}

/// Run in a helper role, as the fake's own commands do.
fn helper(role: &[&str]) -> Result<ExitCode, String> {
    let record = |commands: &str| {
        append(Path::new(commands), &format!("{}\n", std::process::id()))
            .map_err(|err| format!("cannot record the command: {err}"))
    };
    let sleep = || std::thread::sleep(Duration::from_secs(COMMAND_SECONDS));
    match role {
        ["detached", commands] => {
            // A session of its own, as codex's sandbox gives every command, which no signal to
            // the server's process group reaches; recorded only once it is in it.
            #[cfg(unix)]
            nix::unistd::setsid().map_err(|err| format!("setsid: {err}"))?;
            record(commands)?;
            sleep();
        }
        ["linger", commands] => {
            record(commands)?;
            sleep();
        }
        ["chatter", commands] => {
            record(commands)?;
            let warning =
                Message::notification::<m::Warning>(&fixtures::warning(THREAD, "still here"))
                    .map_err(|err| err.to_string())?
                    .to_line();
            let mut out = std::io::stdout();
            while writeln!(out, "{warning}")
                .and_then(|()| out.flush())
                .is_ok()
            {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        ["fork", commands, release] => {
            let exe = std::env::current_exe().map_err(|err| err.to_string())?;
            Command::new(exe)
                .args([HELPER, "helper", commands, release])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|err| format!("cannot start the helper: {err}"))?;
        }
        ["helper", commands, release] => {
            record(commands)?;
            wait_for(Path::new(release));
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
fn wait_for(path: &Path) {
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
