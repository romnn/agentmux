//! What every agentmux driver does as a process, whatever it says to the CLI it drives.
//!
//! A driver is the `agentmux` executable launched as a turn's delegate in its CLI's place: it
//! runs the CLI as its own child, the *server*, and speaks to it over stdio.
//! [`crate::appserver`] drives Codex over `codex app-server`, and [`crate::vendor::pi::driver`] drives
//! pi over `pi --mode rpc`.
//! What a driver asks of its server and what it records of the turn are its own; the process
//! around that conversation is here:
//!
//! - Nothing starts until the turn's launch record names the driver, so a driver whose caller
//!   died before recording it never takes over the turn that replaced it.
//! - The server runs in the driver's process group, so a driver the store has to signal takes the
//!   server with it, and under a subscription its environment is scrubbed again where it is
//!   spawned.
//! - When the turn is over the driver closes the server's input, gives the server a few seconds,
//!   and then stops whatever the server left running: codex's sandbox starts each command in a
//!   session of its own, which no signal to a process group reaches, so the driver reads the
//!   server's process tree while it still holds the server's input open, and signals what
//!   survives by pid and start time.
//!   On Linux it adopts what the server orphans, so a command a crashed server left behind is
//!   still found below it, and it waits for each adopted orphan that exits so none lingers in
//!   the process table.
//!   Elsewhere it records that tree every second while the turn runs instead, which is best
//!   effort: on macOS, any process started and orphaned between two records is out of reach,
//!   whether or not the server crashed — a job a command backgrounds with `nohup … &` is
//!   orphaned as soon as its shell exits.
//!   On Windows the driver holds everything below it in a kill-on-close job object instead, and
//!   reads no process tree, because a recorded parent pid proves nothing there; a process the
//!   server deliberately starts outside the job is let go.
//!   All of it is bounded and best effort: the sweep gives up at its deadline, and a driver that
//!   cannot adopt or cannot make the job says so on stderr and contains less.
//! - What the turn left running is what the driver knew of before closing the server's input
//!   that was already outside the server's process tree — an orphan whose shell exited, as a
//!   job backgrounded with `nohup … &` is — and that the sweep still found alive once the server
//!   had exited.
//!   Everything below the server then is the server's to stop — for codex, its MCP servers and
//!   its command shells with whatever still runs below them — and is swept but not reported.
//!   The driver looks once more after its last signal, and keeps each such process's pid, start
//!   time, name and whether it still runs in the turn's `left-running.json` before it exits, so
//!   the transcript and `status` can say that work in them may be incomplete
//!   ([`crate::left_running`]).
//!   Nothing is kept for a cancelled turn, nor on Windows, where nothing is read.
//!
//! Steered messages reach a driver through the turn's [`mailbox`].

mod leftovers;
pub mod mailbox;

use std::io::{PipeWriter, Write};
use std::path::Path;
use std::process::{Child, Stdio};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use crate::delegate::{Billing, scrub_billing_env};
use crate::left_running::{LeftRunning, bounded_name};
use crate::run::layout::TurnDir;
use leftovers::{Cohort, ProcessIdentity, SeenProcess};

/// How often the cancel marker, the mailbox and the server process are looked at while a turn
/// runs.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// How long an interrupted turn is given to close before the driver leaves anyway, unless the
/// driver's timeouts say otherwise, as [`crate::appserver::DriverTimeouts`] can.
pub(crate) const INTERRUPT_GRACE: Duration = Duration::from_secs(8);

/// How long the server is given to exit once its stdin is closed.
///
/// It saves the session on the way out, which a follow-up resumes from, so it is asked to leave
/// by end of input and only then by signal.
const SERVER_EXIT_GRACE: Duration = Duration::from_secs(5);

/// How long what the server left running is given between the polite signal and the forceful one.
const SWEEP_GRACE: Duration = Duration::from_secs(2);

/// How long, once the forceful signal is due, what is found below the driver keeps being killed
/// and looked for again.
///
/// A process forked after one read of the process table is only found by the next, and the
/// driver's exit would not stop it: an adopted orphan is handed on to another parent, not killed.
const KILL_GRACE: Duration = Duration::from_secs(1);

/// How long, once the sweep has given up, what it killed is given before the driver looks at it
/// a last time to report what became of it.
const REPORT_SETTLE: Duration = Duration::from_millis(100);

/// The longest a cancelled driver takes to leave once it has seen the cancellation: the
/// interrupt, which covers waiting for a turn being started, the server's own exit, and the sweep
/// of whatever it left behind, both signals and the last look included.
///
/// The store's `cancel` waits longer than this before it signals the driver, so a driver that
/// honours the cancellation is never killed in the middle of cleaning up.
pub const STOP_BUDGET: Duration = INTERRUPT_GRACE
    .saturating_add(SERVER_EXIT_GRACE)
    .saturating_add(SWEEP_GRACE)
    .saturating_add(KILL_GRACE)
    .saturating_add(REPORT_SETTLE);

/// How long the driver waits on the server at each point where it waits for an answer.
///
/// [`DriverTimeouts::default`] is what every driven turn runs with; a test shortens them to reach
/// the paths a deadline ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriverTimeouts {
    /// How long one setup request may go unanswered.
    ///
    /// Generous, because resuming a long session reads all of it before the server answers.
    pub setup_response: Duration,
    /// How long an interrupted turn, or a cancelled one still being started, is given to close
    /// before the driver leaves anyway.
    pub interrupt_completion: Duration,
    /// How long, once the turn has ended, a steer the server has not answered yet is waited for.
    pub steer_settlement: Duration,
}

impl Default for DriverTimeouts {
    fn default() -> Self {
        Self {
            setup_response: Duration::from_secs(120),
            interrupt_completion: INTERRUPT_GRACE,
            steer_settlement: Duration::from_secs(2),
        }
    }
}

/// How a driven turn ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriverOutcome {
    /// The turn closed and the capture ends in the terminal event that says how.
    TurnEnded,
    /// The caller cancelled the turn; the capture carries no terminal event, so the store's own
    /// record of the cancellation decides it.
    Cancelled,
    /// The turn never reached an end the capture could record: the server could not be started,
    /// refused to set the turn up, went away part-way through, or the capture could not be
    /// written.
    Failed(String),
}

impl DriverOutcome {
    /// The driver's exit status: zero when the capture or the store says how the turn ended,
    /// non-zero when only stderr does.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::TurnEnded | Self::Cancelled => 0,
            Self::Failed(_) => 1,
        }
    }
}

/// How long a driver waits for its turn's launch record to name it before it gives the turn up.
///
/// The caller writes that record as soon as the spawn returns, so a driver normally finds it on
/// its first look, and one that never comes means the caller died in between.
/// Generous, because giving up breaks a turn that is otherwise sound, while waiting only delays a
/// driver whose caller is already gone.
pub(crate) const LAUNCH_RECORD_WAIT: Duration = Duration::from_secs(10);

/// How often the launch record is looked for while it is not there yet.
const LAUNCH_RECORD_POLL: Duration = Duration::from_millis(20);

/// Keep what the turn left running in its `left-running.json`, once, unless it left nothing or
/// the turn is being cancelled.
///
/// Written before the driver exits, so a reader that sees the driver gone finds the record
/// complete or finds none, ever.
/// A cancelled turn's leftovers are the cancellation's doing, and its transcript never shows
/// them, so nothing is kept for one.
/// The `cancelling` marker is looked at last, which spares a turn already being cancelled a
/// record nobody reads; a record kept just before `cancel` marks the turn is left to the fold,
/// which reads no record once the marker exists.
/// Only Unix gets a record: on Windows the job object stops what is left, and nothing is read.
pub(crate) fn keep_left_running(turn: &TurnDir, left_running: Option<LeftRunning>) {
    let Some(left_running) = left_running else {
        return;
    };
    if turn.cancelling().exists() {
        return;
    }
    let path = turn.left_running();
    let kept = serde_json::to_vec(&left_running)
        .map_err(std::io::Error::other)
        .and_then(|json| crate::atomic_file::create_once(&path, &json));
    match kept {
        Ok(true) => {}
        Ok(false) => eprintln!(
            "agentmux: {} already exists, so what this turn left running is not kept",
            path.display()
        ),
        Err(err) => eprintln!(
            "agentmux: cannot keep what this turn left running in {}: {err}",
            path.display()
        ),
    }
}

/// Wait up to `wait` for the turn's launch record to name this process.
///
/// A caller that dies between spawning the driver and recording it leaves a directory that a
/// follow-up takes over as an abandoned claim and recreates under the same path.
/// A driver that went on regardless would claim the new turn's steers and overwrite its closing
/// message, so it stops unless the record names it.
/// Returns why the turn is not this driver's: the record names another process, cannot be read,
/// or never appeared.
pub(crate) fn confirm_launch(turn: &TurnDir, wait: Duration) -> Result<(), String> {
    let driver_pid = std::process::id();
    let deadline = Instant::now() + wait;
    loop {
        match turn.read_launch_identity() {
            Ok(Some(launched)) if launched.pid == driver_pid => return Ok(()),
            Ok(Some(launched)) => {
                return Err(format!(
                    "the turn at {} was launched as process {}, not as this driver ({driver_pid})",
                    turn.root.display(),
                    launched.pid
                ));
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(LAUNCH_RECORD_POLL),
            Ok(None) => {
                return Err(format!(
                    "no launch record for the turn at {} appeared within {wait:?}",
                    turn.root.display()
                ));
            }
            Err(err) => return Err(format!("cannot read the turn's launch record: {err}")),
        }
    }
}

/// Write each queued line to the server, and close its input once the queue is closed and empty.
pub(crate) fn write_server(mut to_server: impl Write, queue: &Receiver<String>) {
    for line in queue {
        if writeln!(to_server, "{line}")
            .and_then(|()| to_server.flush())
            .is_err()
        {
            return;
        }
    }
}

/// A server the driver spawned, with the write end of its input.
pub(crate) struct Server {
    pub(crate) child: Child,
    pub(crate) to_server: PipeWriter,
}

/// Start `program` with `args` in `cwd` as the driver's server.
///
/// Its stderr is the driver's own, which is the turn's `stderr.log`.
/// The environment is the driver's, which the launcher built; under a subscription it is
/// scrubbed again here, because this is where the process that could spend money is made.
/// It stays in the driver's process group, so a driver the store has to signal takes the server
/// with it.
/// Returns the server and the write end of its input, a pipe of the driver's own so the driver
/// can hold the input open through a second handle.
pub(crate) fn spawn_server(
    program: &str,
    args: &[String],
    cwd: &Path,
    billing: Billing,
) -> std::io::Result<Server> {
    let mut env: std::collections::BTreeMap<String, String> = crate::host_env();
    if billing == Billing::Subscription {
        scrub_billing_env(&mut env);
    }
    // The driver's own stdio is the turn's capture files, which the server must not hold beyond
    // what it is given.
    crate::launch::keep_std_handles_from_children();
    let spawn = |program: &str| -> std::io::Result<Server> {
        let (input, to_server) = std::io::pipe()?;
        let child = std::process::Command::new(program)
            .args(args)
            .current_dir(cwd)
            .env_clear()
            .envs(&env)
            .stdin(Stdio::from(input))
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        Ok(Server { child, to_server })
    };
    match spawn(program) {
        // An npm-installed CLI on Windows is a `.cmd` shim, which `Command` does not find by its
        // bare name.
        #[cfg(windows)]
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            spawn(&format!("{program}.cmd")).map_err(|_| error)
        }
        spawned => spawned,
    }
}

/// Close the server's input, wait for it to leave, and stop whatever it left running.
///
/// The commands a turn runs are the server's descendants, and codex's sandbox starts each in a
/// session of its own, which no process-group signal reaches and which outlives the server as an
/// orphan.
/// `input` is a handle that holds the server's input open, and dropping it closes that input.
/// So what runs below the driver is read while `input` still holds the server open, and read again
/// until nothing is left; `recorded_descendants` is what [`DescendantMonitor`] recorded while the
/// turn ran, which reaches a command whose server crashed before then.
/// On Linux the driver adopts the server's orphans (see [`adopt_orphans`]), so such a command, or
/// one a command started while it was being stopped, is still found below it.
/// Each process is asked to stop and then made to, recognised by its pid and start time so a pid
/// reused meanwhile is left alone.
/// Elsewhere an orphan neither reading saw is out of reach: on macOS, any process started and
/// orphaned between two of the once-a-second records.
/// Once the polite signal's grace is over, every read for [`KILL_GRACE`] longer kills what it
/// finds, so a process forked during the sweep is still caught.
///
/// Returns what the turn left running, as a [`Cohort`] decides it from these reads, and what
/// became of each once it was signalled; `None` when it left nothing.
pub(crate) fn stop_server(
    child: &mut Child,
    input: impl Sized,
    recorded_descendants: Vec<ProcessIdentity>,
) -> Option<LeftRunning> {
    let driver_pid = std::process::id();
    // The server is stopped and waited for through `child`; once waited for, its pid is free
    // for another process, which a start time to the second cannot tell apart.
    let server_pid = child.id();
    // Read with each process's parent, which is what tells what runs below the server — its MCP
    // servers and command shells, which codex stops itself — from what had already left its
    // tree.
    let before_eof = process_tree(driver_pid);
    let mut cohort = Cohort::new(server_pid, &before_eof, &recorded_descendants);
    let mut known_descendants = recorded_descendants;
    add_unseen(
        &mut known_descendants,
        before_eof.iter().map(|process| process.identity),
    );
    known_descendants.retain(|process| process.pid != server_pid);
    drop(input);
    let deadline = Instant::now() + SERVER_EXIT_GRACE;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => break,
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    let _ = child.kill();
    let _ = child.wait();

    let polite_until = Instant::now() + SWEEP_GRACE;
    let forceful_until = polite_until + KILL_GRACE;
    let mut signalled_processes: Vec<ProcessIdentity> = Vec::new();
    loop {
        let mut processes_to_check = descendants(driver_pid);
        add_unseen(
            &mut processes_to_check,
            known_descendants
                .iter()
                .chain(&signalled_processes)
                .copied(),
        );
        let now = Instant::now();
        let polite = now < polite_until;
        // Every pass after the polite one kills all it finds, so what was forked since the last
        // read is killed by the next one.
        let alive = observe_and_signal(&processes_to_check, |process| {
            if !polite {
                Some(sysinfo::Signal::Kill)
            } else if signalled_processes.contains(&process) {
                None
            } else {
                Some(sysinfo::Signal::Term)
            }
        });
        cohort.observe(&alive);
        if alive.is_empty() {
            return cohort.report(&[]);
        }
        if !polite && now >= forceful_until {
            break;
        }
        add_unseen(
            &mut signalled_processes,
            alive.iter().map(|process| process.identity),
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // The sweep gave up with something just killed: one last look says what it became.
    if cohort.is_empty() {
        return None;
    }
    std::thread::sleep(REPORT_SETTLE);
    let still_alive: Vec<ProcessIdentity> = observe_and_signal(&cohort.identities(), |_| None)
        .into_iter()
        .map(|process| process.identity)
        .collect();
    cohort.report(&still_alive)
}

/// Add to `known` each of `processes` it does not hold yet.
fn add_unseen(
    known: &mut Vec<ProcessIdentity>,
    processes: impl IntoIterator<Item = ProcessIdentity>,
) {
    for process in processes {
        if !known.contains(&process) {
            known.push(process);
        }
    }
}

/// Make the driver the parent of whatever its descendants orphan, so [`stop_server`] can still
/// find a command whose server exited before it.
///
/// Linux only, where the kernel offers it; elsewhere an orphan goes to the system, and nothing
/// below the driver says it was there.
/// Returns whether the driver now adopts.
pub(crate) fn adopt_orphans() -> bool {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    match nix::sys::prctl::set_child_subreaper(true) {
        Ok(()) => return true,
        Err(error) => {
            eprintln!("agentmux: cannot adopt what the server orphans: {error}");
        }
    }
    false
}

/// Watches what runs below the driver while the turn runs.
///
/// Where the driver cannot adopt, it records the driver's descendants every few ticks, so the
/// sweep in [`stop_server`] can still stop a command whose server crashed and left it to the
/// system.
/// Where it adopts, such a command stays below the driver, and the watch instead waits for each
/// adopted orphan as it exits.
/// An exited process stays in the process table until its parent waits for it, and a long turn
/// whose commands keep forking short-lived helpers would otherwise fill that table with them,
/// until the consultation can start no process at all.
/// The server is never waited for here, so its exit status stays where `Child` reads it.
/// Stopped before the sweep: an orphan nobody waited for keeps its pid, so the pid the sweep read
/// cannot pass to another process before it is signalled.
pub(crate) struct DescendantMonitor {
    stopping: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: std::thread::JoinHandle<Vec<ProcessIdentity>>,
}

impl DescendantMonitor {
    /// How many ticks pass between two records of the driver's descendants.
    const DESCENDANT_SNAPSHOT_TICKS: u32 = 10;

    pub(crate) fn start(server_pid: u32, adopting: bool) -> Self {
        let stopping = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&stopping);
        let driver_pid = std::process::id();
        let thread = std::thread::spawn(move || {
            let mut recorded_descendants: Vec<ProcessIdentity> = Vec::new();
            let mut ticks = 0_u32;
            while !flag.load(std::sync::atomic::Ordering::Acquire) {
                if adopting {
                    reap_exited_orphans(server_pid);
                } else if ticks.is_multiple_of(Self::DESCENDANT_SNAPSHOT_TICKS) {
                    add_unseen(&mut recorded_descendants, descendants(driver_pid));
                }
                ticks = ticks.wrapping_add(1);
                std::thread::sleep(POLL_INTERVAL);
            }
            recorded_descendants
        });
        Self { stopping, thread }
    }

    /// Stop watching, and return every descendant it recorded.
    pub(crate) fn stop(self) -> Vec<ProcessIdentity> {
        self.stopping
            .store(true, std::sync::atomic::Ordering::Release);
        self.thread.join().unwrap_or_default()
    }
}

/// Wait for every child of the driver's that has exited, except the server.
///
/// An exited child is looked at before it is waited for, so the server's own exit is left
/// alone; an orphan behind an exited server is left for the system, which waits for it once the
/// driver, which leaves soon after its server, has gone.
fn reap_exited_orphans(server_pid: u32) {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        use nix::sys::wait::{Id, WaitPidFlag, waitid, waitpid};

        let Ok(server_pid) = i32::try_from(server_pid).map(nix::unistd::Pid::from_raw) else {
            return;
        };
        let exited = WaitPidFlag::WEXITED | WaitPidFlag::WNOHANG | WaitPidFlag::WNOWAIT;
        while let Ok(status) = waitid(Id::All, exited)
            && let Some(pid) = status.pid()
            && pid != server_pid
        {
            let _ = waitpid(pid, Some(WaitPidFlag::WNOHANG));
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let _ = server_pid;
}

/// On Windows, put the driver into a job object that kills everything still in it when the
/// driver exits, so what the server leaves running goes with it.
///
/// Every process the driver or its descendants start joins the job, and none is ever looked up
/// by its parent: Windows keeps the parent pid a process was created with after that parent has
/// gone, and a pid of ours that reuses it would claim someone else's process.
/// A process the server deliberately starts outside the job is let go, as the server intends.
/// The handle is never closed by hand, so the job outlives everything the driver does and ends
/// with its process.
/// A driver that cannot make the job says so on stderr, and stops only the server itself.
pub(crate) fn contain_descendants() {
    #[cfg(windows)]
    {
        let mut limits = win32job::ExtendedLimitInfo::new();
        limits.limit_kill_on_job_close().limit_breakaway_ok();
        let contained = win32job::Job::create_with_limit_info(&limits)
            .and_then(|job| job.assign_current_process().map(|()| job.into_handle()));
        if let Err(error) = contained {
            eprintln!("agentmux: cannot hold what the server starts in a job: {error}");
        }
    }
}

/// A fresh read of the process table, with each process's parent and start time: of `pids`, or
/// of every process when `pids` is `None`.
///
/// `None` where the table cannot be read at all, which reads as no process found.
///
/// Threads are left out: Linux lists each as a child of its process, and a signal to one is a
/// signal to the whole process — the driver's own included.
fn process_table(pids: Option<&[sysinfo::Pid]>) -> Option<sysinfo::System> {
    let mut system = sysinfo::System::new().ok()?;
    system.refresh_processes_specifics(
        pids.map_or(
            sysinfo::ProcessesToUpdate::All,
            sysinfo::ProcessesToUpdate::Some,
        ),
        true,
        sysinfo::ProcessRefreshKind::nothing().without_tasks(),
    );
    Some(system)
}

/// Every process below `root`, with the start time that tells it apart from a later holder of
/// its pid, its parent and its name.
///
/// A process counts as a child only if it started no earlier than its parent, so a pid reused
/// since cannot claim an older process.
/// Always empty on Windows, where the job object of [`contain_descendants`] stops them instead
/// and a recorded parent pid proves nothing.
fn process_tree(root: u32) -> Vec<SeenProcess> {
    if cfg!(windows) {
        return Vec::new();
    }
    let Some(system) = process_table(None) else {
        return Vec::new();
    };
    let Some(root_started) = system
        .process(sysinfo::Pid::from_u32(root))
        .map(sysinfo::Process::start_time)
    else {
        return Vec::new();
    };
    let mut found: Vec<SeenProcess> = Vec::new();
    let mut parents = vec![ProcessIdentity {
        pid: root,
        start_time_secs: root_started,
    }];
    while let Some(parent) = parents.pop() {
        for (pid, process) in system.processes() {
            let pid = pid.as_u32();
            let start_time_secs = process.start_time();
            if process.parent().map(sysinfo::Pid::as_u32) == Some(parent.pid)
                && start_time_secs >= parent.start_time_secs
                && pid != root
                && !found.iter().any(|known| known.identity.pid == pid)
            {
                let identity = ProcessIdentity {
                    pid,
                    start_time_secs,
                };
                found.push(SeenProcess {
                    identity,
                    parent_pid: Some(parent.pid),
                    name: process_name(process),
                });
                parents.push(identity);
            }
        }
    }
    found
}

/// [`process_tree`], without what only the sweep's report needs.
fn descendants(root: u32) -> Vec<ProcessIdentity> {
    process_tree(root)
        .into_iter()
        .map(|process| process.identity)
        .collect()
}

/// A process's name, bounded as a report keeps it; never its arguments, which can carry secrets.
fn process_name(process: &sysinfo::Process) -> String {
    bounded_name(&process.name().to_string_lossy())
}

/// Returns every process of `tree` that is still alive, after sending each the signal `signal`
/// chooses for it, if any.
///
/// A process counts as alive while its pid still has the start time recorded for it and it has
/// not exited.
/// One read decides both, so no process is reported alive that the signal chosen for it missed.
///
/// A pid whose start time no longer matches belongs to someone else and is left alone, and a
/// platform without the polite signal gets the forceful one.
fn observe_and_signal(
    tree: &[ProcessIdentity],
    signal: impl Fn(ProcessIdentity) -> Option<sysinfo::Signal>,
) -> Vec<SeenProcess> {
    if tree.is_empty() {
        return Vec::new();
    }
    let pids: Vec<sysinfo::Pid> = tree
        .iter()
        .map(|process| sysinfo::Pid::from_u32(process.pid))
        .collect();
    let Some(system) = process_table(Some(&pids)) else {
        return Vec::new();
    };
    let mut alive = Vec::new();
    for &identity in tree {
        let Some(process) = system.process(sysinfo::Pid::from_u32(identity.pid)) else {
            continue;
        };
        let exited = matches!(
            process.status(),
            sysinfo::ProcessStatus::Zombie | sysinfo::ProcessStatus::Dead
        );
        if process.start_time() != identity.start_time_secs || exited {
            continue;
        }
        if let Some(signal) = signal(identity)
            && process.kill_with(signal).is_none()
        {
            process.kill();
        }
        alive.push(SeenProcess {
            identity,
            parent_pid: process.parent().map(sysinfo::Pid::as_u32),
            name: process_name(process),
        });
    }
    alive
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use googletest::prelude::*;

    use super::confirm_launch;
    use crate::launch::Launched;
    use crate::run::layout::TurnDir;

    /// Record `pid` as the turn's child, whole, as the store does once the spawn returns.
    fn record_launch(turn: &TurnDir, pid: u32) -> Result<()> {
        let record = serde_json::json!({
            "launched": Launched {
                pid,
                process_group: Some(pid),
                started: None,
            },
            "started_at": "2026-10-19T12:00:00Z",
        });
        crate::atomic_file::write_whole(&turn.launch(), record.to_string().as_bytes()).or_fail()
    }

    /// A driver goes on only when its turn's launch record names it: a record naming another
    /// process, one that cannot be parsed, and one that never appears all stop it.
    #[gtest]
    fn a_driver_runs_only_the_turn_its_launch_record_names() -> Result<()> {
        let dir = tempfile::tempdir().or_fail()?;
        let turn = TurnDir {
            root: dir.path().to_path_buf(),
        };
        let wait = Duration::from_millis(50);

        let missing = confirm_launch(&turn, wait);
        assert_that!(missing, err(contains_substring("no launch record")));

        record_launch(&turn, std::process::id().wrapping_add(1))?;
        let other = confirm_launch(&turn, wait);
        assert_that!(other, err(contains_substring("not as this driver")));

        std::fs::write(turn.launch(), "{").or_fail()?;
        let unreadable = confirm_launch(&turn, wait);
        assert_that!(unreadable, err(contains_substring("cannot read")));

        record_launch(&turn, std::process::id())?;
        assert_that!(confirm_launch(&turn, wait), ok(()));

        // A record written after the driver first looks is still found.
        std::fs::remove_file(turn.launch()).or_fail()?;
        let recorder = std::thread::spawn({
            let turn = turn.clone();
            move || {
                std::thread::sleep(Duration::from_millis(100));
                record_launch(&turn, std::process::id())
            }
        });
        assert_that!(confirm_launch(&turn, Duration::from_secs(10)), ok(()));
        recorder.join().ok().or_fail()??;
        Ok(())
    }
}
