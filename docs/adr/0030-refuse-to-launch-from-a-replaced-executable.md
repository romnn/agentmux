# 0030. A store refuses to launch once the executable it launches from has been replaced

Status: Accepted

## Context

Every turn runs under `agentmux supervise` ([0025](0025-eager-settlement-and-terminal-waiting.md)),
and a driven Codex turn under `agentmux codex-app-server`
([0018](0018-codex-app-server-driver.md)), both launched from the path of the executable the
launching process runs. The launching process writes the plan the child reads, so the two must be
the same build. A short-lived command always is. The MCP server is not: it runs for as long as its
host does, and an upgrade replaces the file at its path underneath it. Each launch it makes
afterwards starts the new build with a plan the old one wrote, and when the plan's shape has
changed the supervisor cannot parse it. The consultation then fails as a launch that never started,
reported only once something reads it, long after `start` returned an id.

## Decision

**A store remembers each agentmux executable it was given as the file was then, and launches no
turn once the file has changed.** `RunStore::with_supervisor` and `RunStore::with_codex_app_server`
record the file's size and modification time (`crates/agentmux/src/run/executable.rs`). As the
launch decides what to run, after the delegate CLI's version probe, which can take seconds, it
compares them with the file as it is now, and refuses with `RunError::ExecutableReplaced` when
either differs. `start` and `follow_up` therefore fail at once, giving back the claimed turn and
leaving no consultation behind, and the MCP server tells the calling agent to
have the user restart it. An automatic resume
([0027](0027-automatic-resume-after-overload.md)) is refused the same way by the supervisor that
would launch it, which was given the executable when it started.

A process that outlived an upgrade from a build without this check still launches the new
supervisor with its old plan. The supervisor cannot prevent that, but it can name it: plans are
written whole, so one it cannot parse was most likely written by another build rather than damaged,
and it says which version it is and to restart the process that launched it, which reaches the
turn's failure.

## Consequences

- Restarting the MCP server is the whole remedy, and consultations started before an upgrade can
  still be waited for, read and cancelled meanwhile, because reading launches nothing.
- An automatic resume whose supervisor outlived an upgrade during its wait is refused and left
  abandoned, and restarting the server does not bring it back: a `follow_up` continues the
  consultation.
- Any change to the file refuses launches, a reinstall of the same version included; the check
  cannot tell builds that agree on the plan from builds that do not, and a restart is cheap.
- Supervisor and driver plans carry no version and no older plan shape is read: the launching
  process and the child it launches are kept to one build instead.
- A file replaced with one of the same size and modification time is not noticed. Installers
  write a new file, so in practice this is a copy that preserved both on purpose.
