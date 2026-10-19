# Agent guidelines

`agentmux` lets a coding agent running in one vendor's harness (Claude Code, Codex) delegate a
question to the *other* vendor's model by spawning that vendor's CLI. It hands back the **whole
transcript**, never just the final message — the interesting failure modes (a blocking hook that
reopens a finished turn and blanks the result, a warning event that merely looks terminal) live
entirely in the messages a last-message-only API throws away. Same-vendor subagents are already
native to each harness; agentmux exists only for the cross-vendor direction.

## Crate layout

| Crate                 | What it is                                                                    |
| --------------------- | ----------------------------------------------------------------------------- |
| `crates/agentmux`     | library: `config`, `delegate`, `stream/{claude,codex}`, `transcript`, `launch`, `quota`, `run`, `appserver` |
| `crates/agentmux-mcp` | library: the `rmcp` stdio server and its tools                                 |
| `crates/agentmux-cli` | binary `agentmux`: `agentmux mcp` serves, the other subcommands are a unified CLI over both vendors |
| `crates/codex-app-server-protocol` | library: the `codex app-server` protocol — generated payload `types` and `methods` bindings, the hand-written JSON-RPC `envelope` |
| `crates/codex-app-server-mock` | test support: a scriptable `Peer` playing the server's end, typed `fixtures`, and a `codex` binary that plays a `scenario` as a process |
| `crates/codex-app-server-codegen` | tool: generates the protocol crate's `schema.rs` and `methods.rs` from the vendored codex schema |

## House style

- `indoc!` for every multi-line string literal, prompt scaffolding included; leading indentation
  would otherwise leak into what the delegate reads. A literal whose newlines are written `\n` is
  a multi-line string too — `indoc!` it, and reach for `formatdoc!` where the text interpolates.
- `googletest` for test assertions: `use googletest::prelude::*;` then
  `assert_that!(actual, eq(expected))`. `std::assert_eq!` is banned in `clippy.toml`.
- `thiserror` for library error types. `color-eyre` is for `agentmux-cli`, which installs its
  report hook in `main`; the library crates do not depend on it.
- Test helpers return `googletest::Result` and convert with `.or_fail()?` rather than
  `unwrap`/`expect`. The workspace denies both, and `clippy.toml`'s test exemption covers only
  code inside a test-attributed function — a free helper in a `tests/` file is not exempt.
- **Account aliases are opaque too, and so are quota payloads.** The machine's `agentmux.toml` is
  the only authority on which accounts exist; agentmux keeps no roster and never ranks them. A
  vendor's usage figures are passed through exactly as they arrived, because a percentage means
  nothing without the plan tier behind it and every absolute figure is null on a subscription.
- **A failed quota probe is never zero usage.** `Observation::Unavailable` exists so that an
  account which could not be asked cannot be confused with an idle one; an unauthenticated Claude
  config directory reports 0% used, so anything ranking on "least used" would prefer whichever
  account is broken.
- **Isolation is imposed by argument, never by environment.** `--setting-sources ""`,
  `--strict-mcp-config` and Codex's `--ignore-user-config` are what suppress settings, hooks and
  MCP servers; no environment variable can switch them on or off. `Isolation::Inherit` drops
  exactly those flags and nothing else — plan mode and the read-only tool list are a separate axis.
- **A server denies its own harness's vendor, and the store is what enforces it.**
  `agentmux mcp --deny <vendor>` — `claude-code` is an accepted spelling of `claude` — sets
  `RunStore::with_denied_vendors`, checked beside the recursion refusal in `launch_turn`, which is
  the one place `start` and `follow_up` both pass through: a consultation begun in a terminal is
  not a way to drive a denied vendor from a server. The MCP layer only *advertises* the narrower choice, rewriting the
  `delegate` enum of every tool that requires one, and a schema whose shape has moved leaves the
  list wide rather than hiding a vendor that works — the refusal is the gate, the schema is the
  saved call. `quota` keeps both vendors because reading a usage window launches nothing. Denying
  every vendor is refused at startup, where a host surfaces it, rather than at every call, where
  a host swallows it.
- **Every delegate carries `AGENTMUX_DELEGATE=1`, and agentmux refuses to launch a delegate — or
  to serve MCP at all — when it finds itself inside one.** An isolated delegate cannot reach
  agentmux, but an inheriting one loads the operator's own MCP servers. The marker is not the only
  witness: Codex starts its MCP servers with an allowlist environment that drops it (measured:
  `HOME LANG LC_ALL LOGNAME PATH PWD SHELL TERM TMPDIR USER`, in a fresh process group), so
  `RunStore::running_inside_a_delegate` also walks the process tree (`launch::ancestors`, through
  `sysinfo`) and refuses when any ancestor is a recorded delegate whose start time still matches
  and whose turn nobody has settled. The store names its own resolved root to every delegate as
  `AGENTMUX_STATE_DIR`, so a nested agentmux looks in the same store whatever the platform
  default would have been — except under Codex's MCP-server allowlist, which drops that too, so
  with a non-default state directory the tree witness is blind there (a cost hazard, not an
  identity one; documented, not solved). Decided once per process; a store that cannot be read
  is an error, never "not inside".
- **A request's `env` is an allowlist the operator writes, never a denylist agentmux keeps.** Only
  the names under `request_env` in `agentmux.toml` may be set per call, and by default none: no
  list of names to refuse stays complete against `PATH`, `NODE_OPTIONS`, `LD_PRELOAD` and whatever
  the next runtime reads. An account's own `request_env` counts only when a project file did not
  choose the account. Every environment name agentmux handles
  passes through `config::canonical_env_name` on the way in (upper-cased on Windows, exact
  elsewhere), so every comparison is a plain one.
- **The rendered transcript is append-only for the life of a consultation, and a turn's fold input
  is frozen once it is terminal.** A terminal event ends the fold. A child observed gone is
  recorded once, together with how much capture existed and the tail of its stderr
  (`ExitRecord::Exited { status, events_len, stderr_tail }`), and every later fold reads the
  record rather than the files, which a descendant of the child may still be writing. `cancel`
  marks the turn as cancelling under the lock, stops the child, waits for it to go, then records
  `ExitRecord::Cancelled { events_len }`; a concurrent reader that sees the child go first records
  the same cancellation. A Codex reply recovered from `last-message.md` is only ever added to the
  newest turn and is then kept in `recovered.md`, so it can neither land above a later turn nor
  go missing once rendered; text that cannot be kept is not rendered. Whatever record settles a
  turn, the turn is re-folded from exactly the length that record froze, so two observers publish
  the same bytes. `remove` and the sweep refuse a run while any recorded child without an exit
  record is still alive, because both CLIs outlive their closing event, and a follow-up is
  refused (`NotResumable::StillSaving`) until the newest turn's delegate has left, because both
  save the session it resumes on their way out. `every_fixture_renders_monotonically_over_its_prefixes`
  checks every fixture at every line cut.
- **A pid on disk is trusted only with the start time recorded beside it.** `Launched::started`
  comes from `sysinfo` at spawn; liveness after a restart requires it to match, and a settled turn
  is never observed again. Every change to a run's shape — folding a turn, which may settle it,
  recovering a late reply, publishing the render, claiming a turn, marking and recording a
  cancellation, removing the run — is a method of `run::LockedRun`, which only `RunStore::lock`
  makes, from the advisory lock at `runs/<id>.lock`, which is never unlinked. A fold and the
  publication of its render happen under one hold, so no reader can publish a reading another
  caller has since settled differently. Nothing reachable from a `LockedRun` takes the lock
  again — a second lock on the same file from one process waits on the first for ever.
- **A project file selects an account and switches nothing on.** It arrives with a clone, so an
  account it chose runs isolated, never on API pricing, and without its own `request_env`,
  whatever the machine file configures for it; only an explicit `inherit_settings` from the caller
  overrides the isolation. `delegate::AccountPolicy` is the one place that is decided, for every
  reader — isolation, billing, request environment — and an alias that is the checkout's default
  counts as its choice even when a caller names it, because a follow-up names the default it
  resolved and the checkout's own instructions could name it too.
- **Only a machine config file may define an account.** A project `agentmux.toml`, found by walking
  up from the delegate's working directory, may only select one. It can arrive with a `git clone`,
  and a file that could name a `base_url` and an `api_key_env` would be a credential-exfiltration
  primitive. Machine files live under the home directory only — `XDG_CONFIG_HOME` and `APPDATA`
  count only when they point there — with `AGENTMUX_CONFIG` as the one deliberate escape hatch, so
  a container that points the config base at a workspace cannot promote a checkout to a machine
  file. A turn directory is a turn once it holds a launch or exit record; one with neither is a
  claim, and a claim found under the lock is abandoned: it is set aside under a name no fold
  walks (a child spawned by the caller that died may still be writing to it) and taken over.
- **Model id and reasoning effort are opaque pass-through strings.** agentmux keeps no roster of
  either, and a new model identifier must never require an agentmux release. The delegate CLI is the
  sole authority on which values it accepts; when one is wrong, that CLI's own error — which names
  the valid set — is surfaced verbatim. agentmux validates only that the strings are argv-safe:
  non-empty, no control characters, bounded length.
- **A model rewrite is the operator's file speaking, not a roster.** `[models.<vendor>]` in a
  machine file maps an identifier a caller asks for to the one launched, which is how "fable-5"
  keeps meaning the current point release without an agentmux release or a code change; an
  identifier the file does not name is untouched. Exactly one substitution is applied and a file
  whose rules would chain is refused at load, because either resolution would be a guess made on
  every launch. `Config::rewritten_model` is the only reader, `RunStore::pin_defaults` the only
  caller: the model is frozen with the account and the isolation at `start`, so a file edited
  mid-consultation cannot answer the second half of one transcript with another model. What was
  asked for is kept in `Meta::rewritten_from` and rendered by `run::describe_delegate`, since a
  caller that pinned a model and read back another must be able to tell its own rule from a pin
  agentmux dropped. Only a machine file may define one — which model answers decides what an
  operator pays and what they are told.
- **A default effort is a fallback keyed by the model that runs, and no effort at all is a valid
  answer.** `Delegate::effort` is an `Option`; `[efforts.<vendor>]` in a machine file fills it in
  `pin_defaults`, after the model rewrite and only when the caller named none, and a model the file
  says nothing about launches with no `--effort` and no `model_reasoning_effort` at all, so the
  CLI's own default runs rather than a value agentmux invented. A default keyed by a model the
  same file rewrites away is refused at load (`ConfigError::EffortForRewrittenModel`): it could
  never match and reads as if it did. Both sides are opaque `ModelId` and `Effort` values, because
  effort vocabularies change with models. The schema no longer requires `effort`, and the record and every summary
  name the effort that ran, or `cli-default`.
- **A configuration file is parsed into its types as it is read.** Vendors, aliases, models and
  efforts are map keys of `Vendor`, `AccountAlias`, `ModelId` and `Effort`, so nothing downstream
  re-parses a string, and a value no argument parser would accept is a `ConfigError::Malformed`
  pointing at its byte offset. A vendor this build does not know is passed over by
  `config::by_vendor` and reported as an unknown key, because one file is synced across machines
  running different agentmux releases.
- **An unknown configuration key is reported, never refused.** No struct in `config` sets
  `deny_unknown_fields`: one machine file is read by every agentmux on the machine, and the ones
  that matter are the long-running MCP servers started before the key was added — refusing the file
  takes them out of service over a table they have no use for. `Config::unknown_keys` finds them by
  serialising the parsed config back and diffing it against the file, so the answer cannot drift
  from the types; it runs before `canonicalise_env_names`, which on Windows rewrites the keys it
  would be compared against. A key whose value is an empty table or array is passed over, because
  serialising drops an empty `request_env` and the file would be told its own key is unknown. The
  cost is that a misspelled `api_key_env` now silently authenticates as the CLI's own login, which
  is why every reader says the ignored keys out loud: `tracing::warn!` at each load, an `ignored`
  line in `agentmux accounts`, and an `unknown` array in its `--json`.
- **A delegate inherits whatever confines agentmux, so a session it cannot save is caught at
  every launch.** Run from an agent's sandboxed shell — Codex's `workspace-write` exec, say —
  agentmux hands that sandbox to its child. Measured with claude 2.1.270 under Codex's Seatbelt
  profile: the delegate answers, the kernel denies its transcript write under `~/.claude/projects`,
  print mode says nothing, and every follow-up fails with "No conversation found". `launch_into`,
  the one place a child is spawned, therefore probes the vendor's session store —
  `Vendor::session_store`, resolved from the environment the child gets, request names included —
  by creating and removing a file in it, or the store itself when the CLI has not made it yet,
  because a sandbox that grants the store alone must pass. Only a permission or read-only refusal
  counts, and a configuration directory that does not exist is not probed, because a CLI that is
  not set up there says nothing about confinement. A refusal is recorded in the turn's invocation
  record, before the launch record that makes the directory a turn, so the fold always sees it
  from the turn's first render: it becomes `Turn::unsaved_session_store`, rendered under the
  question ahead of anything the stream adds, and the newest turn's is
  `RunStatus::unsaved_session_store`. `resumable_session` refuses a follow-up with
  `NotResumable::SessionNotSaved` rather than let the CLI fail after being paid for, and
  `describe_unsaved_session_store` is the one phrasing every reader uses. Separately,
  `RunStatus::delegate_stderr` quotes a failed newest turn's stderr when the fold says no exit
  record settled it (`NewestTurn::settled_by` is not `SettledBy::Record`), so it never repeats
  the tail the record froze. It is a snapshot, so the append-only transcript is untouched.
- **No key from the environment reaches a delegate unless a machine file opted its account in.**
  `Billing::Subscription` is the default and needs no configuration: `delegate::scrub_billing_env`
  runs over the child's environment when the identity is resolved and again at the spawn
  (`ProcessLauncher::launch`, and the driver for the server it starts), so no layer — host,
  `[launch]`, account `env`, request `env` — can bring a key; `apply_request_env` refuses a
  billing name even when `request_env` lists it; `apply_account` refuses an account whose
  `Account::api_credential_field` is set under a subscription, and an account a project file
  chose is subscription whatever it says. `is_billing_name` is a rule, not a list — any
  `*_API_KEY` or `*_API_KEY_FILE`, every vendor namespace's URL, `*_ISSUER` and `*_AUTH_TOKEN`
  names (a token endpoint override would send a subscription's refresh token elsewhere), plus a
  few named switches. The rule is about the environment only: what a CLI's own configuration
  or login says (a provider in `~/.codex/config.toml`, a Codex API-key login, a key in Claude's
  `settings.json`) is the user's choice for that CLI and is left alone, so no `-c` pin, login
  check or protocol check stands in for it. A caller may pin the pricing (`StartRequest::billing`,
  `--billing`, the MCP `billing` parameter) — `Api` only within the account's opt-in,
  `Subscription` always — and `start` pins whatever the first turn decides in `Meta::billing`,
  which every follow-up asks for again, so a project-selected account cannot drift onto API pricing
  when the files change. The pricing surfaces as `RunStatus::billing`, `RunSummary::billing`,
  `billing=api` in `report::delegate_line` and `list`, and a note under the question.
- **An inheriting Codex turn is driven over `codex app-server` by agentmux itself, so it can be
  steered.** The driver (`appserver::drive`) is the delegate as far as the store can tell: this
  executable run as the hidden `agentmux codex-app-server <plan>`, launched by the same launcher
  into the same turn directory, reading the question on stdin and writing `codex exec
  --json`-shaped events to stdout, so `stream::codex` folds both modes and nothing downstream
  branches on them. What each notification becomes follows codex's own exec client
  (`EventProcessorWithJsonOutput`), and the translator writes, and the fold reads, one model:
  `stream::codex::CaptureEvent`, whose `exec` events are the `codex_app_server_protocol::exec`
  mirror of codex's own types.
  One driver runs one turn; a follow-up starts another that resumes the thread with
  `thread/resume`. Reading and writing the server's pipes happen on two threads, so a server that
  stops reading cannot stall the driver's deadlines or a cancellation; a turn the server ends
  before answering `turn/start` is remembered, and the end is written once, after the steers in
  flight are settled. `steer` and `cancel` reach the driver through files alone, so a restarted
  agentmux can: the turn's mailbox (a request is claimed or withdrawn by hard-linking its one
  decision file into place, so exactly one wins — not by rename, which on Windows lets every
  racer win; a claimed one is answered accepted, rejected, or unconfirmed when it was sent and never
  answered, because codex may have taken it) and the `cancelling` marker, after which the driver
  sends nothing new and interrupts a started turn. `RunStore::accepts_steers` is the one predicate
  behind both `RunStatus::steerable` and `steer`'s admission; a turn whose capture does not yet
  say `turn.started` — which the driver writes once it knows the turn and serves its mailbox,
  from `turn/started` or the answer to `turn/start`, whichever comes first — is refused as
  `SteerRefused::Starting`, so a steer never waits on setup. When the turn is over, `stop_server`
  reads the server's process tree while the driver still holds its input open, then stops what
  survives the server by pid and start time — codex's sandbox runs commands in sessions of their
  own, which a process-group signal misses, and on Linux the driver is their subreaper, so even
  one whose parent died first is still found (macOS records the tree once a second instead, and
  Windows holds it in a kill-on-close job object, reading no parent pids); the store waits `appserver::STOP_BUDGET` plus a margin
  before it signals. `Delegate::app_server_settings` is the one place that decides a delegate can
  run this way — Codex and `Isolation::Inherit` only, because `codex app-server` has no
  `--ignore-user-config`, and a private `CODEX_HOME` cannot stand in (the design note in
  `appserver` says why) — and `RunStore::with_codex_app_server` opts a store in; without it every
  Codex turn is `exec`. The mode is recorded per turn (`LaunchMode`), and a delivered steer is
  captured as an `agentmux.steer` event that renders as `### user (steer)`.
- **The app-server protocol is generated from codex's own schema, and every message goes through
  its method binding.** `codex-app-server-protocol`'s `schema.rs` (progeny's types) and
  `methods.rs` are written by `crates/codex-app-server-codegen` from codex's JSON Schema
  (`schema/codex_app_server_protocol.schemas.json`), whose four message unions name every method
  and its params. Every method is bound by name, but only the methods `surface!` in the generator
  lists — what agentmux sends, answers or decodes — are bound to types, and only their payloads
  are generated: a method agentmux never reads costs nothing and cannot be decoded. The table is
  also the one place that says which type answers a request, which the schema never does:
  `"turn/start" = TurnStart(TurnStartParams) => TurnStartResponse`, with codex's own variant name.
  Two checks keep it honest: generation checks each entry against the schema (the method is
  listed with exactly those params, no other method of that union carries them, a request's
  response exists), and the oracle (`crates/codex-app-server-codegen/oracle`, `task
  codegen:app-server:oracle`) expands the same table into a match on codex's own
  `ClientRequest`/`ClientResponse`/`ClientNotification`/`ServerRequest`/`ServerResponse`/
  `ServerNotification` at the pinned release, which compiles only when each variant carries
  exactly the entry's params and response. Nothing reads codex's source. Each method is a marker
  type: `Method` names it, `Request` or `Notification` says what kind of message it is, a
  direction trait (`ClientRequest`, `ServerNotification`, …) who sends it, and for a surface
  method `Typed` binds its params (`()` when it carries none, `Option` when they may be left out)
  and `Answered` a request's response, so the driver sends `call::<m::TurnStart>` and gets a
  `TurnStartResponse` back. Each direction also gets an `Any*` enum:
  `AnyServerNotification::classify(method, params)` names the method as a variant, holding a
  `Payload<M>` that decodes only as `M`'s params for a surface method, so a receiver matches
  variants, not strings, and decodes only what it acts on. `decline` matches `AnyServerRequest`
  with no wildcard, so a regeneration that adds a server request fails to build until it is
  decided. progeny reads the bundle as it is, with the v2 namespace listed first, and grows the
  types from the surface's payloads, each marked as sent or received; the workspace's
  `Cargo.lock` pins progeny, and progeny's configuration is code. The oracle is a workspace of its
  own, because codex's crate is several hundred packages with native dependencies that only the oracle
  should build, and resolves only with codex's `[patch.crates-io]` forks and lockfile pins. Do
  not edit the generated files: change the generator, then `task codegen:app-server`.
  `task codegen:app-server:check` (the generator's tests, no codex needed) fails when they drift.
  The codex pin is the oracle's git tag, as its `Cargo.lock` resolves it
  (`codex-app-server-codegen pin` prints it). Moving it is deliberate: set the tag in
  `oracle/Cargo.toml`, copy the forks `codex-rs/Cargo.toml` patches that the graph uses, seed
  `oracle/Cargo.lock` from `codex-rs/Cargo.lock` at that tag, install that codex, and run `task
  codegen:app-server:refresh` and `task codegen:app-server:oracle`. `codex-app-server-codegen
  refresh` refuses a codex other than the pin, and one whose app-server accepts
  `--ignore-user-config`, the cue to let isolated delegates be driven. The `codegen` workflow runs
  all three checks. The generated types read leniently: a status, kind or tagged object the pinned
  schema does not name decodes as an `Unknown` variant, so a newer codex does not fail the message
  around it; the translator writes a message carrying one as drift with its whole payload, as it
  writes one that does not decode, because `exec`'s vocabulary has no word for it. Three things are
  hand-written. The JSON-RPC envelope: codex leaves out the
  `"jsonrpc"` member, so a message is classified by which of `id` and `method` it carries, and a
  payload is decoded separately, so drift is recorded rather than the message lost. `rollout`,
  the usage windows codex records in a session rollout: it reads only a token count's snapshot,
  every other line and event reading as `Other`, and the oracle converts codex's snapshot into it
  with every field of codex's named and reads codex's own rollout lines back as it. And `exec`,
  the events `codex exec --json` writes, for which codex publishes no schema: it mirrors codex's
  `exec_events.rs` type for type, and the oracle converts each of codex's types into its mirror
  with every field named on both sides (a change on either side fails to build) and checks that
  each serialises as codex's does. The mirror writes what codex writes and reads more than codex
  reads, because a capture holds lines a newer codex wrote and lines an older agentmux wrote, and
  the render of a finished consultation must not change: an `Unknown` variant names a type,
  status or kind it was not built for; every field a reader takes that codex requires is an
  `Option`, so absence and `null` read as they always did; and every field of a narration item,
  which nothing reads, has a default. `a_capture_an_older_agentmux_wrote_folds_as_it_did` holds
  the old shapes. A line is read as a JSON value first, because codex writes a web search's `id`
  twice. The oracle's codex crates must resolve to one tag and commit, or the pin is refused. agentmux's own side —
  which settings a thread runs under, how a request is declined — stays in `appserver/protocol.rs`.
- The workspace denies `unwrap`/`expect`/`panic`/`indexing_slicing` outside tests, and denies
  `#[allow]`/`#[expect]` without a `reason = "..."`.

## Commands

| Command                | What it does                                                          |
| ---------------------- | --------------------------------------------------------------------- |
| `task run -- <args>`   | run the `agentmux` binary                                             |
| `task check`           | `cargo check` across the workspace                                    |
| `task test`            | nextest across the workspace                                          |
| `task test:fc`         | nextest across every feature combination                              |
| `task test:doc`        | doctests (nextest cannot run them)                                    |
| `task lint`            | clippy, workspace, all features                                       |
| `task lint:fc`         | clippy across every feature combination × target — what CI gates on   |
| `task lint:fix`        | clippy with `--fix`                                                   |
| `task format`          | `cargo fmt --all`                                                     |
| `task spellcheck`      | typos                                                                 |
| `task audit`           | advisories against the dependency tree                                |
| `task unused`          | unused dependencies (nightly)                                         |
| `task lint:actions`    | actionlint over `.github/workflows`                                   |
| `task codegen:app-server` | regenerate `codex-app-server-protocol` from the vendored schema |
| `task codegen:app-server:check` | the generator's tests: fail when the protocol crate is not what the vendored schema generates — CI runs it |
| `task codegen:app-server:refresh` | re-export the vendored schema from the pinned codex on `PATH`, then regenerate |
| `task codegen:app-server:oracle` | check the answers table and the `exec` mirror against codex's own crates at the pinned tag — CI runs it |
| `task codegen:app-server:upstream` | fail when the vendored schema is not what the pinned codex exports — CI runs it |

## Probing accounts

`agentmux quota` reads what each configured account has left, and costs nothing:

- **Claude** keeps its own figures in `$CLAUDE_CONFIG_DIR/.claude.json` under
  `cachedUsageUtilization`. agentmux reads that file, refreshing it with `claude --print /usage`
  when it is over five minutes old — that command reports `total_cost_usd: 0` and no API duration.
  Only `limits[]` carries the per-model windows; the printed text omits some of them.
- **Codex** publishes nothing to disk outside a session rollout, so agentmux asks
  `codex app-server` over JSON-RPC for `account/rateLimits/read`. The server interleaves unsolicited
  notifications with replies, so the response is matched on request id rather than read as the next
  line.
- Codex reports no rate limits on the stream `codex exec --json` produces. They are recovered
  afterwards from `$CODEX_HOME/sessions/**/rollout-*<thread-id>*.jsonl`, and cached per turn in
  `rate-limit.json`, because that search would otherwise run on every `status` and every page of a
  `tail`. The window recorded is an exhausted one if any, the one reopening last if several are,
  and otherwise the fullest: a plan with a five-hour and a weekly window is refused against
  whichever is used up, and advice to wait for the other would be refused again.

## Testing against the real CLIs

`crates/agentmux/tests/live.rs` drives real `claude` and `codex` processes. Those tests are
`#[ignore]`d, spend money, and need both CLIs logged in; CI passes without them. Run them after
upgrading either CLI:

```bash
cargo nextest run -p agentmux --run-ignored all -E 'test(live)'
```

Each turn's `invocation.json` records what the delegate's CLI printed for `--version`
(`Launcher::version`, asked before the launch with the child's environment) and the agentmux
release that launched it, so a capture that reads differently can be traced to what wrote it.

A steered turn over `codex app-server` has a live test of its own in the crate that builds the
driver, which it runs as the delegate; run it outside any sandbox, so the delegate can save its
session:

```bash
AGENTMUX_LIVE_CODEX_ACCOUNT=personal \
  cargo nextest run -p agentmux-cli --run-ignored all -E 'test(live)' --no-capture
```

Otherwise the driver is tested against `codex-app-server-mock`. `crates/agentmux/tests/appserver.rs`
scripts the server's side of each conversation with a `Peer` over real pipes — what it expects,
what it answers, in order — with payloads built by `fixtures` from the generated types, and
shortens the driver's `Deadlines` to reach what a deadline ends. A `Peer` reads only when its
script waits for a message, so a script that stops asking is a server that stops reading.
`crates/agentmux-cli/tests/steer.rs` runs the real `agentmux` as the driver against the mock's
`codex` binary, which it builds with `escargot` (nothing else builds another package's binary for
a test) and configures through `app-server-mock.json` in the turn's codex home, because the
driver's environment is an allowlist.

Everything else runs against `agentmux::testing::ScriptedLauncher` and the recorded streams under
`crates/agentmux/fixtures/`, which were captured from real runs with the exact flags `delegate.rs`
builds. `no_committed_fixture_contains_an_unnamed_event_type` fails the build if a re-recorded
fixture contains an event type the parser does not name, which is how a CLI's format change becomes
loud instead of lossy.

A real capture arrives carrying the recording machine's identity: the `system`/`init` event holds
your home directory, the uid in a temp path, the absolute path of your checkout, the session UUID
encoded into the memory-directory slug, and the list of skills and slash commands you have
installed. None of it is read by either parser. Replace those fields with mocked values before
committing; `no_committed_fixture_carries_a_recording_machine_path` fails the build if you forget.
