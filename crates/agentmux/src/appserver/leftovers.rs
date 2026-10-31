//! Which processes a driven turn left running, decided from what the driver saw of its process
//! tree as it stopped the server.
//!
//! Kept apart from the reads and signals themselves, which need real processes, so the decision
//! can be tested on its own.
//!
//! The server's direct children are codex's own: the MCP servers it starts, at any time during
//! the turn, and the shells it runs commands in, which it stops itself as it shuts down.
//! What a delegate leaves behind sits below one of those shells, or was orphaned when the shell
//! exited.
//! So the turn's leftovers are what is still alive once the server has exited, less what was a
//! direct child of the server just before it was asked to leave.
//! A start time cannot draw that line instead: codex may start an MCP server after the turn has.

use crate::left_running::{LeftProcess, LeftRunning, ProcessFate};

/// A process recognised by its pid and its start time, so a later holder of the pid is told
/// apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ProcessIdentity {
    pub(super) pid: u32,
    pub(super) start_time_secs: u64,
}

/// A process one read of the process table found alive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SeenProcess {
    pub(super) identity: ProcessIdentity,
    /// Its parent's pid, when the read recorded one.
    pub(super) parent_pid: Option<u32>,
    /// Its process name, bounded as a record keeps it.
    pub(super) name: String,
}

/// The processes a turn left running, gathered as the sweep meets them.
#[derive(Debug)]
pub(super) struct Cohort {
    server_pid: u32,
    /// What ran directly below the server just before its input was closed.
    server_children: Vec<ProcessIdentity>,
    /// What the turn left running, in the order the sweep first met it.
    members: Vec<SeenProcess>,
}

impl Cohort {
    /// A cohort for the server `server_pid`, whose direct children are read from `before_eof`,
    /// the driver's descendants as they stood while the server's input was still open.
    pub(super) fn new(server_pid: u32, before_eof: &[SeenProcess]) -> Self {
        Self {
            server_pid,
            server_children: before_eof
                .iter()
                .filter(|process| process.parent_pid == Some(server_pid))
                .map(|process| process.identity)
                .collect(),
            members: Vec::new(),
        }
    }

    /// Take in what one read after the server's exit found alive: every process that is not
    /// the server and was not one of its direct children joins the cohort.
    ///
    /// A process forked after an earlier read joins when a later one meets it.
    pub(super) fn observe(&mut self, alive: &[SeenProcess]) {
        for process in alive {
            let identity = process.identity;
            let joins = identity.pid != self.server_pid
                && !self.server_children.contains(&identity)
                && !self
                    .members
                    .iter()
                    .any(|member| member.identity == identity);
            if joins {
                self.members.push(process.clone());
            }
        }
    }

    /// Whether the turn left nothing running.
    pub(super) fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    /// Who is in the cohort, to look at once more after the last signal.
    pub(super) fn identities(&self) -> Vec<ProcessIdentity> {
        self.members.iter().map(|member| member.identity).collect()
    }

    /// The report of the cohort, each member's fate read from `still_alive`, what the last look
    /// after the last signal found alive; `None` when the turn left nothing running.
    pub(super) fn report(self, still_alive: &[ProcessIdentity]) -> Option<LeftRunning> {
        let processes = self
            .members
            .into_iter()
            .map(|member| {
                let SeenProcess { identity, name, .. } = member;
                LeftProcess {
                    pid: identity.pid,
                    start_time_secs: identity.start_time_secs,
                    name,
                    fate: if still_alive.contains(&identity) {
                        ProcessFate::StillRunning
                    } else {
                        ProcessFate::NoLongerRunning
                    },
                }
            })
            .collect();
        LeftRunning::of(processes)
    }
}

#[cfg(test)]
mod tests {
    use googletest::prelude::*;

    use super::{Cohort, ProcessIdentity, SeenProcess};
    use crate::left_running::{
        LeftProcess, LeftRunning, MAX_PROCESSES, OmittedProcesses, ProcessFate,
    };

    const DRIVER: u32 = 10;
    const SERVER: u32 = 11;

    fn identity(pid: u32) -> ProcessIdentity {
        ProcessIdentity {
            pid,
            start_time_secs: 1_700_000_000 + u64::from(pid),
        }
    }

    fn seen(pid: u32, parent_pid: u32, name: &str) -> SeenProcess {
        SeenProcess {
            identity: identity(pid),
            parent_pid: Some(parent_pid),
            name: name.to_owned(),
        }
    }

    fn left(pid: u32, name: &str, fate: ProcessFate) -> LeftProcess {
        let ProcessIdentity {
            pid,
            start_time_secs,
        } = identity(pid);
        LeftProcess {
            pid,
            start_time_secs,
            name: name.to_owned(),
            fate,
        }
    }

    /// The server's own children — an MCP server, a shell running a command — are codex's, and
    /// never reported, though they survived the server; a job backgrounded below that shell, and
    /// one orphaned to the driver, are the turn's, and so is one forked during the sweep.
    #[gtest]
    fn the_cohort_is_what_survives_the_server_less_its_direct_children() {
        // Before the server's input closed: the server, its MCP server and shell, a job running
        // below the shell, and an orphan the driver adopted.
        let before_eof = [
            seen(SERVER, DRIVER, "codex"),
            seen(20, SERVER, "node"),
            seen(21, SERVER, "bash"),
            seen(30, 21, "python3"),
            seen(40, DRIVER, "sleep"),
        ];
        let mut cohort = Cohort::new(SERVER, &before_eof);

        // The first read after the server exited, with the shell's and MCP server's parents now
        // the driver, then a later read meeting a process forked meanwhile.
        cohort.observe(&[
            seen(20, DRIVER, "node"),
            seen(21, DRIVER, "bash"),
            seen(30, 21, "python3"),
            seen(40, DRIVER, "sleep"),
        ]);
        cohort.observe(&[seen(30, 21, "python3"), seen(50, 30, "worker")]);
        assert_that!(cohort.is_empty(), eq(false));
        assert_that!(
            cohort.identities(),
            elements_are![eq(&identity(30)), eq(&identity(40)), eq(&identity(50))]
        );

        // The last look after the last signal found only the forked worker alive.
        let report = cohort.report(&[identity(50), identity(21)]);
        assert_that!(
            report,
            some(eq(&LeftRunning {
                version: crate::left_running::VERSION,
                processes: vec![
                    left(50, "worker", ProcessFate::StillRunning),
                    left(30, "python3", ProcessFate::NoLongerRunning),
                    left(40, "sleep", ProcessFate::NoLongerRunning),
                ],
                omitted: OmittedProcesses::default(),
            }))
        );
    }

    /// A process whose pid a direct child of the server held before is a different process, and
    /// joins the cohort.
    #[gtest]
    fn a_reused_pid_is_not_the_server_child_it_replaced() {
        let mut cohort = Cohort::new(SERVER, &[seen(20, SERVER, "node")]);
        let reused = SeenProcess {
            identity: ProcessIdentity {
                pid: 20,
                start_time_secs: identity(20).start_time_secs + 5,
            },
            parent_pid: Some(DRIVER),
            name: "sh".to_owned(),
        };
        cohort.observe(&[reused]);
        assert_that!(cohort.identities(), len(eq(1)));
    }

    /// A turn that left nothing, or only codex's own processes, has no report.
    #[gtest]
    fn nothing_left_is_no_report() {
        let mut cohort = Cohort::new(SERVER, &[seen(20, SERVER, "node")]);
        cohort.observe(&[seen(SERVER, DRIVER, "codex"), seen(20, DRIVER, "node")]);
        assert_that!(cohort.is_empty(), eq(true));
        assert_that!(cohort.report(&[]), none());
    }

    /// A process with no recorded parent before the server's input closed — one only the
    /// once-a-second record saw — is not known to be the server's, so it is reported.
    #[gtest]
    fn a_process_of_unknown_parentage_is_reported() {
        let mut cohort = Cohort::new(SERVER, &[]);
        cohort.observe(&[SeenProcess {
            identity: identity(60),
            parent_pid: None,
            name: "python3".to_owned(),
        }]);
        assert_that!(
            cohort.report(&[identity(60)]),
            some(field!(
                LeftRunning.processes,
                elements_are![eq(&left(60, "python3", ProcessFate::StillRunning))]
            ))
        );
    }

    /// Past the cap the report counts what it does not name, by fate.
    #[gtest]
    fn a_large_cohort_is_capped() -> Result<()> {
        let mut cohort = Cohort::new(SERVER, &[]);
        let alive: Vec<SeenProcess> = (100..170).map(|pid| seen(pid, DRIVER, "sh")).collect();
        cohort.observe(&alive);
        let still: Vec<ProcessIdentity> = (100..110).map(identity).collect();
        let report = cohort.report(&still).or_fail()?;
        assert_that!(report.processes, len(eq(MAX_PROCESSES)));
        assert_that!(
            report.omitted,
            eq(OmittedProcesses {
                no_longer_running: 20,
                still_running: 0,
            })
        );
        assert_that!(report.count(), eq(70));
        assert_that!(report.still_running(), eq(10));
        Ok(())
    }
}
