//! What the two turn drivers share: [`crate::appserver`], which drives a Codex turn over
//! `codex app-server`, and [`crate::claude_stream`], which drives a Claude turn over stream-json
//! input.
//!
//! Each driver is the delegate the run store launches, and runs the vendor's CLI as a child of
//! its own with a pipe at each end.
//! Both stop what their CLI leaves running by reading the process table ([`sweep`]).
//! What a driver says to its CLI, and what it makes of the answers, is each driver's own.

use std::io::PipeWriter;
use std::path::Path;
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use crate::delegate::{Billing, scrub_billing_env};
use crate::launch::{LaunchError, spawn_program};
use crate::left_running::bounded_name;
use crate::run::layout::TurnDir;

/// How long a driver waits for its turn's launch record to name it before it gives the turn up.
///
/// The caller writes that record as soon as the spawn returns, so a driver normally finds it on
/// its first look, and one that never comes means the caller died in between.
/// Generous, because giving up breaks a turn that is otherwise sound, while waiting only delays a
/// driver whose caller is already gone.
pub(crate) const LAUNCH_RECORD_WAIT: Duration = Duration::from_secs(10);

/// How often the launch record is looked for while it is not there yet.
const LAUNCH_RECORD_POLL: Duration = Duration::from_millis(20);

/// Wait up to `wait` for the turn's launch record to name this process.
///
/// A caller that dies between spawning the driver and recording it leaves a directory that a
/// follow-up takes over as an abandoned claim and recreates under the same path.
/// A driver that went on regardless would claim the new turn's steers and write into its files,
/// so it stops unless the record names it.
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

/// A driver's CLI, started with a pipe at each end.
pub(crate) struct PipedChild {
    /// The CLI, whose stdout is piped.
    pub(crate) child: Child,
    /// The write end of its stdin: a pipe of the driver's own rather than the child's, so the
    /// driver can hold the input open through a second handle.
    pub(crate) input: PipeWriter,
}

/// Start a driver's CLI, `program` with `args`, in `cwd`.
///
/// Its stderr is the driver's own, which is the turn's `stderr.log`.
/// The environment is the driver's, which the launcher built; under a subscription it is
/// scrubbed again here, because this is where the process that could spend money is made.
/// It stays in the driver's process group, so a driver the store has to signal takes the CLI
/// with it.
/// The program is found as the launcher finds a delegate's ([`spawn_program`]).
pub(crate) fn spawn_piped(
    program: &str,
    args: &[String],
    cwd: &Path,
    billing: Billing,
) -> Result<PipedChild, LaunchError> {
    let mut env = crate::host_env();
    if billing == Billing::Subscription {
        scrub_billing_env(&mut env);
    }
    spawn_program(program, |program| {
        let spawned = std::io::pipe().and_then(|(stdin, input)| {
            std::process::Command::new(program)
                .args(args)
                .current_dir(cwd)
                .env_clear()
                .envs(&env)
                .stdin(Stdio::from(stdin))
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .map(|child| PipedChild { child, input })
        });
        spawned.map_err(|source| LaunchError::Spawn {
            program: program.to_owned(),
            source,
        })
    })
}

/// On Windows, put the driver into a job object that kills everything still in it when the
/// driver exits, so what its CLI leaves running goes with it.
///
/// Every process the driver or its descendants start joins the job, and none is ever looked up
/// by its parent: Windows keeps the parent pid a process was created with after that parent has
/// gone, and a pid of ours that reuses it would claim someone else's process.
/// A process the CLI deliberately starts outside its own job is let go, as the CLI intends.
/// The handle is never closed by hand, so the job outlives everything the driver does and ends
/// with its process.
/// A driver that cannot make the job says so on stderr, naming `program`, and stops only the CLI
/// itself.
pub(crate) fn contain_descendants(program: &str) {
    #[cfg(windows)]
    {
        let mut limits = win32job::ExtendedLimitInfo::new();
        limits.limit_kill_on_job_close().limit_breakaway_ok();
        let contained = win32job::Job::create_with_limit_info(&limits)
            .and_then(|job| job.assign_current_process().map(|()| job.into_handle()));
        if let Err(error) = contained {
            eprintln!("agentmux: cannot hold what {program} starts in a job: {error}");
        }
    }
    #[cfg(not(windows))]
    let _ = program;
}

/// How long what a driver stops is given between the polite signal and the forceful one.
const SWEEP_GRACE: Duration = Duration::from_secs(2);

/// How long, once the forceful signal is due, what is found below the driver keeps being killed
/// and looked for again.
///
/// A process forked after one read of the process table is only found by the next, and the
/// driver's exit would not stop it: an adopted orphan is handed on to another parent, not killed.
const KILL_GRACE: Duration = Duration::from_secs(1);

/// The longest [`sweep`] takes.
pub(crate) const SWEEP_BUDGET: Duration = SWEEP_GRACE.saturating_add(KILL_GRACE);

/// A process recognised by its pid and its start time, so a later holder of the pid is told
/// apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProcessIdentity {
    pub(crate) pid: u32,
    pub(crate) start_time_secs: u64,
}

/// A process one read of the process table found alive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SeenProcess {
    pub(crate) identity: ProcessIdentity,
    /// Its parent's pid, when the read recorded one.
    pub(crate) parent_pid: Option<u32>,
    /// Its process name, bounded as a record keeps it.
    pub(crate) name: String,
}

/// Stop every process of `known` that is still alive, and whatever is found below the driver,
/// within [`SWEEP_BUDGET`], showing `observe` what each read found alive.
///
/// Each process is asked to stop, made to once [`SWEEP_GRACE`] has passed, and recognised by its
/// pid and start time, so a pid reused meanwhile is left alone.
/// Once the polite signal's grace is over, every read for [`KILL_GRACE`] longer kills what it
/// finds, so a process forked during the sweep is still caught.
/// Returns whether nothing was left; `false` when the sweep gave up with something just killed.
pub(crate) fn sweep(known: &[ProcessIdentity], mut observe: impl FnMut(&[SeenProcess])) -> bool {
    let driver_pid = std::process::id();
    let polite_until = Instant::now() + SWEEP_GRACE;
    let forceful_until = polite_until + KILL_GRACE;
    let mut signalled_processes: Vec<ProcessIdentity> = Vec::new();
    loop {
        let mut processes_to_check = descendants(driver_pid);
        add_unseen(
            &mut processes_to_check,
            known.iter().chain(&signalled_processes).copied(),
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
        observe(&alive);
        if alive.is_empty() {
            return true;
        }
        if !polite && now >= forceful_until {
            return false;
        }
        add_unseen(
            &mut signalled_processes,
            alive.iter().map(|process| process.identity),
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Add to `known` each of `processes` it does not hold yet.
pub(crate) fn add_unseen(
    known: &mut Vec<ProcessIdentity>,
    processes: impl IntoIterator<Item = ProcessIdentity>,
) {
    for process in processes {
        if !known.contains(&process) {
            known.push(process);
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
pub(crate) fn process_tree(root: u32) -> Vec<SeenProcess> {
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

/// [`process_tree`], without what only the Codex driver's report needs.
pub(crate) fn descendants(root: u32) -> Vec<ProcessIdentity> {
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
pub(crate) fn observe_and_signal(
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
