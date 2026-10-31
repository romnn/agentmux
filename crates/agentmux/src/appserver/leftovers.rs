//! Which processes a driven turn left running, decided from what the driver saw of its process
//! tree as it stopped the server.
//!
//! Kept apart from the reads and signals themselves, which need real processes, so the decision
//! can be tested on its own.
//!
//! Everything below the server is codex's to stop: its MCP servers, which it may start at any
//! time during the turn, and the shells it runs commands in, whose sessions it stops as it shuts
//! down.
//! Codex signals an MCP server's process group without waiting for it, so a server started
//! through a wrapper can outlive codex for a moment, and is no leftover of the turn's.
//! What a delegate leaves behind that nothing stops is a job whose shell exited: an orphan,
//! already out of codex's process tree.
//! So the turn's leftovers are the processes the driver knew of before the server's input was
//! closed — its live descendants then, and what the once-a-second record saw — that were not
//! below the server then, and that were still alive after the server exited.
//! A start time cannot draw that line instead: codex may start an MCP server after the turn has.
//!
//! A job still below a live command shell when the turn ends is stopped, by codex or by the
//! sweep, and not reported; nor is a process first seen once the server is shutting down.

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
    /// What the driver knew of before the server's input closed that was not below the server
    /// then: the processes that may join.
    candidates: Vec<ProcessIdentity>,
    /// What the turn left running, in the order the sweep first met it.
    members: Vec<SeenProcess>,
}

impl Cohort {
    /// A cohort for the server `server_pid`, from what the driver knew of before the server's
    /// input closed: `before_eof`, its live descendants then, with their parents, and
    /// `recorded`, what the once-a-second record saw while the turn ran.
    ///
    /// Everything below the server in `before_eof`, and the server itself, is codex's.
    pub(super) fn new(
        server_pid: u32,
        before_eof: &[SeenProcess],
        recorded: &[ProcessIdentity],
    ) -> Self {
        // The server's subtree, grown from the server down through the recorded parents.
        let mut below_server: Vec<u32> = vec![server_pid];
        let mut grew = true;
        while grew {
            grew = false;
            for process in before_eof {
                let pid = process.identity.pid;
                if !below_server.contains(&pid)
                    && process
                        .parent_pid
                        .is_some_and(|parent| below_server.contains(&parent))
                {
                    below_server.push(pid);
                    grew = true;
                }
            }
        }
        let mut candidates: Vec<ProcessIdentity> = Vec::new();
        for identity in before_eof
            .iter()
            .map(|process| process.identity)
            .chain(recorded.iter().copied())
        {
            if !below_server.contains(&identity.pid) && !candidates.contains(&identity) {
                candidates.push(identity);
            }
        }
        Self {
            candidates,
            members: Vec::new(),
        }
    }

    /// Take in what one read after the server's exit found alive: each candidate it met joins
    /// the cohort.
    ///
    /// A process the driver first meets after the server's input closed never joins: it is
    /// swept, but it cannot be told from what codex started while shutting down.
    pub(super) fn observe(&mut self, alive: &[SeenProcess]) {
        for process in alive {
            let identity = process.identity;
            let joins = self.candidates.contains(&identity)
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

    /// Everything below the server before its input closed is codex's — an MCP server and the
    /// node it wraps, a shell and the job still running below it — and never reported, though it
    /// survived the server; an orphan the driver adopted is the turn's, and a process first met
    /// during the sweep is swept but not reported.
    #[gtest]
    fn the_cohort_is_what_was_already_outside_the_servers_tree() {
        // Before the server's input closed: the server, a wrapper and the MCP server it runs, a
        // shell with a job below it, and an orphan the driver adopted.
        let before_eof = [
            seen(SERVER, DRIVER, "codex"),
            seen(20, SERVER, "sh"),
            seen(21, 20, "node"),
            seen(22, SERVER, "bash"),
            seen(30, 22, "python3"),
            seen(40, DRIVER, "sleep"),
        ];
        let mut cohort = Cohort::new(SERVER, &before_eof, &[]);

        // The first read after the server exited, with everything that was below it now the
        // driver's, then a later read meeting a process forked meanwhile.
        cohort.observe(&[
            seen(21, DRIVER, "node"),
            seen(22, DRIVER, "bash"),
            seen(30, 22, "python3"),
            seen(40, DRIVER, "sleep"),
        ]);
        cohort.observe(&[seen(40, DRIVER, "sleep"), seen(50, 40, "worker")]);
        assert_that!(cohort.is_empty(), eq(false));
        assert_that!(cohort.identities(), elements_are![eq(&identity(40))]);

        // The last look after the last signal found the orphan still alive.
        let report = cohort.report(&[identity(40), identity(21)]);
        assert_that!(
            report,
            some(eq(&LeftRunning {
                version: crate::left_running::VERSION,
                processes: vec![left(40, "sleep", ProcessFate::StillRunning)],
                omitted: OmittedProcesses::default(),
            }))
        );
    }

    /// A process whose pid something below the server held before is a different process: the
    /// pid is codex's, so it is not reported, though it is swept.
    #[gtest]
    fn a_reused_pid_below_the_server_is_not_reported() {
        let mut cohort = Cohort::new(SERVER, &[seen(20, SERVER, "node")], &[]);
        let reused = SeenProcess {
            identity: ProcessIdentity {
                pid: 20,
                start_time_secs: identity(20).start_time_secs + 5,
            },
            parent_pid: Some(DRIVER),
            name: "sh".to_owned(),
        };
        cohort.observe(&[reused]);
        assert_that!(cohort.is_empty(), eq(true));
    }

    /// A turn that left nothing, or only codex's own processes, has no report.
    #[gtest]
    fn nothing_left_is_no_report() {
        let mut cohort = Cohort::new(SERVER, &[seen(20, SERVER, "node")], &[]);
        cohort.observe(&[seen(SERVER, DRIVER, "codex"), seen(20, DRIVER, "node")]);
        assert_that!(cohort.is_empty(), eq(true));
        assert_that!(cohort.report(&[]), none());
    }

    /// A process only the once-a-second record saw, and no longer below the driver before the
    /// server's input closed — an orphan the system took, where the driver cannot adopt — is
    /// reported once a read finds it alive; one the record saw below the server is not.
    #[gtest]
    fn a_recorded_process_reparented_away_is_reported() {
        let before_eof = [seen(20, SERVER, "bash")];
        let mut cohort = Cohort::new(SERVER, &before_eof, &[identity(60), identity(20)]);
        cohort.observe(&[
            SeenProcess {
                identity: identity(60),
                parent_pid: Some(1),
                name: "python3".to_owned(),
            },
            seen(20, 1, "bash"),
        ]);
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
        let alive: Vec<SeenProcess> = (100..170).map(|pid| seen(pid, DRIVER, "sh")).collect();
        let mut cohort = Cohort::new(SERVER, &alive, &[]);
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
