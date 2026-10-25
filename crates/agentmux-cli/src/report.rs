//! How a command's result reaches the terminal.
//!
//! Changes when the printed output changes.
//!
//! Every command prints prose by default and JSON under `--json`, and the two carry the same
//! facts.
//! Prose goes to stdout so it can be piped; progress and warnings go to stderr through `tracing`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use agentmux::report::{self, Warning};
use agentmux::run::{
    AutoResume, HookReopening, RunError, RunId, RunStatus, RunStore, RunSummary, SettledTurn,
    TranscriptPage, TurnRef, WaitFor,
};
use agentmux::transcript::{Outcome, TurnState};
use color_eyre::eyre::Result;
use serde::Serialize;

use crate::cli::{TailArgs, WaitArgs};

/// Print a consultation's state.
///
/// # Errors
///
/// Returns an error when the JSON form cannot be serialised.
pub fn status(status: &RunStatus, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(status)?);
        return Ok(());
    }
    println!("{}  {}", status.run_id, report::delegate_line(status));
    println!("  state       {}", report::outcome_line(&status.outcome));
    if let Some(resume) = &status.auto_resume {
        println!("  resume      {}", report::auto_resume_line(resume));
    }
    // Above the facts, as every front end puts them: each changes how what follows is read.
    for warning in status.warnings() {
        for line in warning_lines(&warning) {
            println!("{line}");
        }
    }
    println!("  reading     {}", status.cwd.display());
    println!("  elapsed     {}", report::duration(status.elapsed));
    println!("  turns       {}", status.turns);
    println!("  messages    {}", status.message_count);
    if let Some(input) = status.usage.input_tokens {
        let output = status.usage.output_tokens.unwrap_or(0);
        println!("  tokens      {input} in, {output} out");
    }
    if let Some(cost) = status.cost_usd {
        println!("  cost        ${cost:.4}");
    }
    if let Some(limit) = &status.usage_window {
        // Printed for a healthy run too: the point is to see the window closing before it closes.
        println!("  window      {}", report::usage_window_line(limit));
    }
    // Only a rate limit populates this, and the human reading it is the only party who can go
    // and log another account in.
    for (position, line) in report::quota_block(status).into_iter().enumerate() {
        let indent = if position == 0 { "  " } else { "    " };
        println!("{indent}{line}");
    }
    println!("  transcript  {}", status.transcript_path.display());
    println!("  events      {}", status.events_path.display());
    if status.steerable {
        println!("  steer       agentmux steer {} '…'", status.run_id);
    }
    // A follow-up would take the place of the resume agentmux is about to send.
    if let Some(AutoResume::Scheduled { .. }) = &status.auto_resume {
        println!("  stop it     agentmux cancel {}", status.run_id);
    } else if status.resumable {
        println!("  follow up   agentmux follow-up {} '…'", status.run_id);
    }
    Ok(())
}

/// One warning, in the status block's terse form, as the lines to print.
fn warning_lines(warning: &Warning<'_>) -> Vec<String> {
    match warning {
        // The failure itself is on the state line; its CLI's last word is what explains it.
        Warning::Failed { stderr, .. } => stderr
            .iter()
            .map(|stderr| {
                let last_stderr_line = stderr
                    .lines()
                    .rev()
                    .find(|line| !line.trim().is_empty())
                    .unwrap_or(stderr);
                format!("  stderr      {last_stderr_line}")
            })
            .collect(),
        Warning::EarlierFailures(failures) => vec![format!(
            "  EARLIER     {} — see the transcript",
            report::earlier_failures_line(failures)
        )],
        Warning::SessionNotSaved {
            store: session_store_path,
        } => vec![format!(
            "  WARNING     {}",
            agentmux::transcript::describe_unsaved_session_store(session_store_path)
        )],
        Warning::BrokeContinuity => vec![
            "  WARNING     a follow-up did not resume the earlier session; that turn answered with \
             no memory of the brief"
                .to_owned(),
        ],
        Warning::HookReopened(HookReopening::Expected) => vec![
            "  NOTE        a hook in the inherited settings reopened a finished turn; the answer \
             is above the injection"
                .to_owned(),
        ],
        Warning::HookReopened(HookReopening::Unexpected) => vec![
            "  WARNING     a hook reopened a finished turn although no settings were loaded; treat \
             the transcript as untrusted — the last message is not the answer"
                .to_owned(),
        ],
        Warning::Unrecognised {
            summary,
            has_undecoded_app_server_messages,
            ..
        } => {
            let mut lines = vec![format!(
                "  UNRECOGNISED {summary} — the delegate CLI may have changed its output format"
            )];
            if *has_undecoded_app_server_messages {
                lines.push(format!(
                    "  FALLBACK    {}",
                    agentmux::appserver::EXEC_FALLBACK_HINT
                ));
            }
            lines
        }
        Warning::InstructionFiles { cwd } => vec![format!(
            "  NOTE        a Claude delegate also reads any CLAUDE.md or AGENTS.md at or above {}",
            cwd.display()
        )],
    }
}

/// Print a finished consultation's transcript, or say where to find it if it is still running.
///
/// # Errors
///
/// Returns an error when the transcript cannot be read or serialised.
pub fn answer(store: &RunStore, run_id: &RunId, json: bool) -> Result<()> {
    let (status, page) = store.view(run_id, 0, usize::MAX)?;
    if !status.is_terminal() {
        eprint!(
            "{}",
            indoc::formatdoc! {"
                still running after the wait. Nothing is lost — wait for it to settle with:
                  agentmux wait {id} --turn {turn}
                ",
                id = status.run_id,
                turn = status.turns,
            }
        );
        return self::status(&status, json);
    }
    transcript(&status, &page, json)
}

/// Print a slice of a transcript.
///
/// # Errors
///
/// Returns an error when the JSON form cannot be serialised.
pub fn transcript(status: &RunStatus, page: &TranscriptPage, json: bool) -> Result<()> {
    if json {
        let report = TranscriptJson {
            status,
            text: &page.text,
            next_cursor: page.next_offset,
            at_end: page.at_end,
        };
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    print!("{}", page.text);
    if !page.at_end {
        eprintln!(
            "\n[truncated at {} of {} bytes; continue with --offset {}]",
            page.next_offset, page.total_bytes, page.next_offset
        );
    }
    // On stderr, so stdout stays the transcript: some of these are in the transcript already,
    // but one such as a follow-up that resumed nothing is known only to the status.
    for warning in status.warnings() {
        for line in warning_lines(&warning) {
            eprintln!("{line}");
        }
    }
    Ok(())
}

/// Print the transcript written since a cursor, optionally until the consultation finishes.
///
/// # Errors
///
/// Returns an error when the consultation cannot be read.
pub async fn tail(store: &RunStore, args: &TailArgs, json: bool) -> Result<()> {
    /// Slow enough not to spin, fast enough that a terminal feels live.
    const POLL: Duration = Duration::from_millis(700);

    // Only the first read can start from the end; every later one follows on from its cursor.
    let mut cursor = (!args.from_end).then_some(args.cursor);
    loop {
        let (status, page) = match cursor {
            Some(cursor) => store.view(args.run_id.id(), cursor, args.max_bytes)?,
            None => store.view_end(args.run_id.id(), args.max_bytes)?,
        };
        cursor = Some(page.next_offset);

        if json {
            // The same shape every other `--json` form uses for the outcome, so one reader
            // serves them all.
            let report = TailJson {
                run_id: &status.run_id,
                outcome: &status.outcome,
                text: &page.text,
                next_cursor: page.next_offset,
                at_end: page.at_end,
            };
            println!("{}", serde_json::to_string(&report)?);
        } else {
            print!("{}", page.text);
        }

        if !args.follow {
            if !json && !page.at_end {
                eprintln!("\n[next cursor: {}]", page.next_offset);
            }
            return Ok(());
        }
        if status.is_terminal() && page.at_end {
            return Ok(());
        }
        tokio::time::sleep(POLL).await;
    }
}

/// How `agentmux wait` ended, as its exit status says it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitStatus {
    /// Every turn returned completed.
    Completed,
    /// A turn returned failed.
    Failed,
    /// A turn returned cancelled, and none failed.
    Cancelled,
    /// The timeout passed first.
    TimedOut,
    /// An argument named a consultation or a turn there is not.
    Unusable,
}

impl WaitStatus {
    /// The process exit status.
    ///
    /// 124 for a timeout is what `timeout(1)` exits with, and 1 and 2 stay what every other
    /// command exits with for an error and for arguments it cannot act on.
    #[must_use]
    pub fn code(self) -> i32 {
        match self {
            Self::Completed => 0,
            Self::Failed => 10,
            Self::Cancelled => 11,
            Self::TimedOut => 124,
            Self::Unusable => 2,
        }
    }

    /// How a set of settled turns ended: a failure outweighs a cancellation, which outweighs
    /// completion.
    fn of(settled: &[SettledTurn]) -> Self {
        let has = |state: TurnState| settled.iter().any(|turn| turn.outcome().state() == state);
        if has(TurnState::Failed) {
            Self::Failed
        } else if has(TurnState::Cancelled) {
            Self::Cancelled
        } else {
            Self::Completed
        }
    }
}

/// Wait for the turns `args` names to settle, print them, and say how it ended.
///
/// Every turn is pinned before anything waits, so a follow-up started meanwhile is not returned
/// in its place, and an argument naming nothing is refused before any wait.
///
/// # Errors
///
/// Returns an error when the store cannot be read or a settlement does not hold, which the
/// binary exits 1 for.
pub async fn wait(store: &RunStore, args: &WaitArgs, json: bool) -> Result<WaitStatus> {
    // The clock starts with the command, before anything is read, and a timeout too large to add
    // to it is no timeout.
    let deadline = args
        .timeout
        .and_then(|timeout| tokio::time::Instant::now().checked_add(timeout));
    let mut targets = Vec::with_capacity(args.run_ids.len());
    for run_id in &args.run_ids {
        match store.pin_turn(run_id.id(), args.turn) {
            Ok(target) => targets.push(target),
            Err(err @ (RunError::NotFound(_) | RunError::NoSuchTurn { .. })) => {
                eprintln!("agentmux: {err}");
                return Ok(WaitStatus::Unusable);
            }
            Err(err) => return Err(err.into()),
        }
    }
    let until = if args.any { WaitFor::Any } else { WaitFor::All };
    let waited = store.wait_for(&targets, until, deadline).await?;
    let status = if waited.timed_out {
        WaitStatus::TimedOut
    } else {
        WaitStatus::of(&waited.settled)
    };

    let mut reports = Vec::with_capacity(waited.settled.len());
    for settled in &waited.settled {
        let offset = settled.section_offset();
        let page = settled.page(offset, usize::MAX)?;
        reports.push(SettledJson {
            settled,
            // Decided before the settlement was committed, so it is there for a turn that has one.
            auto_resume: store.auto_resume(settled.run_id(), settled.turn_index())?,
            text_offset: offset,
            text: page.text,
        });
    }
    if json {
        let report = WaitJson {
            result: if waited.timed_out {
                "timed_out"
            } else {
                "settled"
            },
            settled: &reports,
            pending: &waited.pending,
        };
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(status);
    }
    if waited.timed_out {
        let pending = waited
            .pending
            .iter()
            .map(|target| format!("{} turn {}", target.run_id(), target.turn_number()))
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!(
            "timed out; still waiting for {pending}. Nothing is lost — the consultation carries \
             on, and `agentmux wait` again picks it up."
        );
        return Ok(status);
    }
    for (position, settled_report) in reports.iter().enumerate() {
        if position > 0 {
            println!();
        }
        print_settled(settled_report)?;
    }
    Ok(status)
}

/// Print one settled turn of a wait, with what agentmux does about it failing overloaded, and its
/// transcript.
///
/// # Errors
///
/// Returns an error when the lines cannot be formatted.
fn print_settled(settled_report: &SettledJson<'_>) -> Result<()> {
    let settled = settled_report.settled;
    // Its own lines under the state, ending in a newline each, or nothing.
    let mut resume = String::new();
    if let Some(auto_resume) = &settled_report.auto_resume {
        writeln!(
            resume,
            "  resume      {}",
            report::auto_resume_line(auto_resume)
        )?;
        if let AutoResume::Scheduled { turn, .. } | AutoResume::Resumed { turn, .. } = auto_resume {
            writeln!(
                resume,
                "  next        agentmux wait {} --turn {turn}, once it has begun",
                settled.run_id()
            )?;
        }
    }
    print!(
        "{}",
        indoc::formatdoc! {"
            {id}  turn {turn} settled
              state       {state}
            {resume}  transcript  {transcript}
              settled     {marker}

            ---

            {text}",
            id = settled.run_id(),
            turn = settled.turn_number(),
            state = report::outcome_line(settled.outcome()),
            transcript = settled.transcript_path().display(),
            marker = settled.marker_path().display(),
            text = settled_report.text,
        }
    );
    Ok(())
}

/// One settled turn in `wait --json`: the settlement, and its transcript from `text_offset`.
#[derive(Serialize)]
struct SettledJson<'a> {
    #[serde(flatten)]
    settled: &'a SettledTurn,
    /// What agentmux does about the turn having failed because the model was overloaded.
    #[serde(skip_serializing_if = "Option::is_none")]
    auto_resume: Option<AutoResume>,
    text_offset: u64,
    text: String,
}

/// `wait --json`: whether the wait ended in settlement or a timeout, what settled, and what did
/// not.
#[derive(Serialize)]
struct WaitJson<'a> {
    result: &'static str,
    settled: &'a [SettledJson<'a>],
    pending: &'a [TurnRef],
}

/// Print recent consultations.
///
/// # Errors
///
/// Returns an error when the JSON form cannot be serialised.
pub fn list(runs: &[RunSummary], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(runs)?);
        return Ok(());
    }
    if runs.is_empty() {
        println!("no consultations yet");
        return Ok(());
    }
    for run in runs {
        print!(
            "{}",
            indoc::formatdoc! {"
                {run_id}  {state:<9}  {turns} turn(s)  {created_at}
                    {delegate}
                    {question}
                ",
                run_id = run.run_id,
                state = run.outcome.state(),
                turns = run.turns,
                created_at = run.created_at.to_rfc3339(),
                delegate = run.delegate,
                question = run.question,
            }
        );
    }
    Ok(())
}

/// Print the account aliases this machine defines, the model rewrites it applies, the efforts it
/// falls back to and the models it blocks.
///
/// The point of the command is discovery: an alias is a name someone has to know, and the error
/// for guessing wrong is only visible after a consultation has been attempted.
/// A rewrite is the opposite problem — it needs no knowing to work, and is invisible until a
/// consultation comes back naming a model nobody asked for.
///
/// # Errors
///
/// Returns an error only when the JSON form cannot be serialised.
pub fn accounts(
    config: &agentmux::config::Config,
    host_env: &BTreeMap<String, String>,
    json: bool,
) -> Result<()> {
    use agentmux::delegate::Vendor;

    if json {
        // The prose form names the files it read, so the machine form has to carry them too.
        let report = AccountsJson {
            machine_config_path: config.source.as_deref(),
            project_config_path: config.project_source.as_deref(),
            config,
            unknown_keys: &config.unknown,
            permits: Vendor::ALL
                .into_iter()
                .map(|vendor| (vendor, config.describe_permissions(vendor)))
                .collect(),
        };
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    if let Some(path) = &config.source {
        println!("config      {}", path.display());
    } else {
        // Naming the paths actually searched, because the reader's next question is where to put
        // the file.
        println!("config      none found. A machine file defines accounts; first that exists:");
        // Listed first because it outranks every searched path, and a reader who has set it and
        // seen nothing loaded needs to know it was consulted.
        println!("              $AGENTMUX_CONFIG (unset)");
        for path in agentmux::config::Config::machine_paths(host_env) {
            println!("              {}", path.display());
        }
    }
    if let Some(path) = &config.project_source {
        println!("project     {} (selects a default only)", path.display());
        // The account rows below are the machine file's, and a project file's choice of
        // default switches none of what they say on.
        println!("              the default it selects runs isolated and on a subscription, even");
        println!(
            "              when named; --ignore-user-config=false still overrides the isolation,"
        );
        println!("              and --billing api is refused, as is an account that sets an API");
        println!("              credential or endpoint");
    }
    for unknown in &config.unknown {
        // A key that does nothing is invisible everywhere else: the file loads, the delegation
        // runs, and whatever the key was for simply does not happen.
        // Both causes are named because they need opposite fixes — one is a typo, the other is an
        // agentmux older than the file it is reading.
        println!("ignored     {unknown} (a misspelling, or a key newer than this agentmux)");
    }

    if !config.launch.is_empty() {
        println!("launch      every delegate also gets:");
        // Names only: the values are the operator's, and the module docs recommend this table
        // for a Bedrock key.
        // A credential name is marked, because a delegate that names an account does not get
        // it: the account's own login must not be outranked by a key meant for the default.
        // A name can be both, and then both rules withhold it.
        let withholding_note = |name: &str| match (
            agentmux::delegate::is_billing_env_name(name),
            agentmux::delegate::is_credential_name(name),
        ) {
            (true, true) => {
                "; withheld when an account is named, and from every delegate on a subscription"
            }
            (true, false) => "; withheld unless the account sets billing = \"api\"",
            (false, true) => "; withheld when an account is named",
            (false, false) => "",
        };
        for name in config.launch.env.keys() {
            println!(
                "              {name} (set in config{})",
                withholding_note(name)
            );
        }
        for name in &config.launch.env_passthrough {
            println!(
                "              {name} (forwarded from this environment{})",
                withholding_note(name)
            );
        }
        if !config.launch.request_env.is_empty() {
            println!(
                "              a request may set: {}",
                config.launch.request_env.join(", ")
            );
        }
    }

    let home = agentmux::config::home_dir(host_env);

    for vendor in Vendor::ALL {
        println!();
        let label = format!("{vendor}");
        match config.default_account(vendor) {
            Some(agentmux::config::DefaultAccount {
                alias,
                chosen_by: agentmux::config::ChosenBy::Project(_),
            }) => {
                println!("{label:<11} default: {alias} (selected by the project file)");
            }
            Some(default) => println!("{label:<11} default: {}", default.alias),
            None => println!("{label:<11} default: the {vendor} CLI's own login"),
        }
        // Printed where the accounts are, because it answers the same kind of question: what this
        // machine does with what a caller asks for.
        print_vendor_rules(config, vendor);
        let accounts = config.accounts(vendor);
        if accounts.is_empty() {
            println!("  (none configured — omit --account to use the CLI's own default)");
            continue;
        }
        for (alias, account) in accounts {
            let how = describe_account(account, home.as_deref());
            println!("  {alias:<14} {}", how.join(", "));
            if let Some(description) = &account.description {
                println!("  {:<14} {description}", "");
            }
        }
    }
    Ok(())
}

/// Print what this machine does with what a caller asks of one vendor: its model rules, and what
/// a delegate loads and may do with the limits a request is held to.
fn print_vendor_rules(config: &agentmux::config::Config, vendor: agentmux::delegate::Vendor) {
    print_model_rules(config.model_rules(vendor));
    print_rows(
        "permits",
        std::iter::once(config.describe_permissions(vendor)),
    );
}

/// Print what this machine does with the model names a caller hands it for one vendor: the
/// rewrites, the default efforts and the blocked models.
fn print_model_rules(rules: agentmux::config::ModelRules<'_>) {
    print_rows(
        "models",
        rules.aliases().map(|(from, to)| format!("{from} -> {to}")),
    );
    // Keyed by the model that runs, which the `->` above may have chosen.
    print_rows(
        "efforts",
        rules
            .efforts()
            .map(|(model, effort)| format!("{model} defaults to {effort}")),
    );
    // Also keyed by the model that runs, and refused at `start`.
    print_rows(
        "blocked",
        rules
            .blocked()
            .map(|(model, block)| format!("{model}: {block}")),
    );
}

/// Print `rows` in the account column layout, naming `heading` on the first row only.
fn print_rows(heading: &str, rows: impl Iterator<Item = String>) {
    for (index, row) in rows.enumerate() {
        let heading = if index == 0 { heading } else { "" };
        println!("  {heading:<14} {row}");
    }
}

/// How one account authenticates, as a list of `field=value` facts.
fn describe_account(account: &agentmux::config::Account, home: Option<&Path>) -> Vec<String> {
    let mut how = Vec::new();
    if let Some(dir) = &account.config_dir {
        // Availability, because one synced config file describes several machines and not
        // all of them are logged into everything.
        let resolved = agentmux::config::expand_tilde(dir, home);
        let state = if resolved.is_dir() { "ok" } else { "MISSING" };
        how.push(format!("config_dir={} ({state})", dir.display()));
    }
    if let Some(base) = &account.base_url {
        how.push(format!("base_url={base}"));
    }
    if account.api_key.is_some() {
        how.push("api_key=<set in config>".to_owned());
    }
    if let Some(name) = &account.api_key_env {
        how.push(format!("api_key=${name}"));
    }
    if account.auth_token.is_some() {
        how.push("auth_token=<set in config>".to_owned());
    }
    if let Some(name) = &account.auth_token_env {
        how.push(format!("auth_token=${name}"));
    }
    // An account that names no directory and no credential is the CLI's own login under a name,
    // which is a configuration in its own right rather than an unfinished one.
    // Left unsaid it renders as a blank column, which reads as an entry someone failed to fill in.
    if how.is_empty() {
        how.push("the CLI's own login".to_owned());
    }
    match account.inherit_settings {
        Some(true) => how.push("inherits settings and hooks".to_owned()),
        Some(false) => how.push("runs isolated".to_owned()),
        None => {}
    }
    // Said only for the exception, like inheritance: the default is a subscription, and a
    // credential named without the opt-in is refused at launch, which `refused` makes visible here.
    match (account.billing, account.api_only_field()) {
        (Some(agentmux::delegate::Billing::Api), _) => how.push("billing=api".to_owned()),
        (_, Some(field)) => how.push(format!(
            "REFUSED at launch: sets {field} without billing = \"api\""
        )),
        (_, None) => {}
    }
    how
}

/// Print what each account has left.
///
/// The vendor payload is printed exactly as it arrived under `--json`.
/// The prose form shows what a person scans for, and defers to the shared renderer so the CLI and
/// the MCP tool cannot describe the same account differently.
///
/// # Errors
///
/// Returns an error only when the JSON form cannot be serialised.
pub fn quota(reported: &agentmux::run::QuotaReport, json: bool) -> Result<()> {
    if let Some(error) = &reported.project_error {
        // On stderr, so the JSON on stdout keeps its shape.
        eprintln!(
            "warning: the project agentmux.toml could not be read, so these are the machine \
             file's accounts alone: {error}"
        );
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&reported.accounts)?);
        return Ok(());
    }
    for entry in &reported.accounts {
        for line in report::quota_report(entry) {
            println!("{line}");
        }
        println!();
    }
    Ok(())
}

/// `result --json` and `transcript --json`: the consultation's state with the page beside it.
#[derive(Serialize)]
struct TranscriptJson<'a> {
    #[serde(flatten)]
    status: &'a RunStatus,
    text: &'a str,
    next_cursor: u64,
    at_end: bool,
}

/// One line of `tail --json`: the outcome every other `--json` form carries, and the new text.
#[derive(Serialize)]
struct TailJson<'a> {
    run_id: &'a RunId,
    outcome: &'a Outcome,
    text: &'a str,
    next_cursor: u64,
    at_end: bool,
}

/// `accounts --json`: the files read, what they configure, and the keys nothing reads.
#[derive(Serialize)]
struct AccountsJson<'a> {
    #[serde(rename = "source")]
    machine_config_path: Option<&'a Path>,
    #[serde(rename = "project_source")]
    project_config_path: Option<&'a Path>,
    #[serde(flatten)]
    config: &'a agentmux::config::Config,
    #[serde(rename = "unknown")]
    unknown_keys: &'a [agentmux::config::UnknownKey],
    /// Each vendor's effective permission defaults and limits, built-in ones included, which the
    /// `codex` and `claude` tables above hold only as far as the file sets them.
    permits: std::collections::BTreeMap<agentmux::delegate::Vendor, String>,
}
