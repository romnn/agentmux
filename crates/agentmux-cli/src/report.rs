//! How a command's result reaches the terminal.
//!
//! Changes when the printed output changes.
//!
//! Every command prints prose by default and JSON under `--json`, and the two carry the same
//! facts.
//! Prose goes to stdout so it can be piped; progress and warnings go to stderr through `tracing`.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use agentmux::run::{HookReopening, RunId, RunStatus, RunStore, RunSummary, TranscriptPage};
use color_eyre::eyre::Result;

use crate::cli::TailArgs;

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
    println!(
        "{}  {}",
        status.run_id,
        agentmux_mcp::render::delegate_line(status)
    );
    println!(
        "  state       {}",
        agentmux_mcp::render::outcome_line(&status.outcome)
    );
    if let Some((turn, kind)) = status.earlier_failure {
        println!(
            "  EARLIER     turn {} failed: {} — see the transcript",
            turn.saturating_add(1),
            kind.label()
        );
    }
    println!("  reading     {}", status.cwd.display());
    println!(
        "  elapsed     {}",
        agentmux_mcp::render::duration(status.elapsed)
    );
    println!("  turns       {}", status.turns);
    println!("  messages    {}", status.message_count);
    if let Some(input) = status.usage.input_tokens {
        let output = status.usage.output_tokens.unwrap_or(0);
        println!("  tokens      {input} in, {output} out");
    }
    if let Some(cost) = status.cost_usd {
        println!("  cost        ${cost:.4}");
    }
    if let Some(limit) = &status.rate_limit {
        // Printed for a healthy run too: the point is to see the window closing before it closes.
        println!(
            "  rate limit  {}",
            agentmux_mcp::render::rate_limit_line(limit)
        );
    }
    // Only a rate limit populates this, and the human reading it is the only party who can go
    // and log another account in.
    for (position, line) in agentmux_mcp::render::quota_block(status)
        .into_iter()
        .enumerate()
    {
        let indent = if position == 0 { "  " } else { "    " };
        println!("{indent}{line}");
    }
    if let Some(drift) = status.unrecognised.summary() {
        println!("  UNRECOGNISED {drift} — the delegate CLI may have changed its output format");
    }
    if status.broke_continuity {
        println!(
            "  WARNING     a follow-up did not resume the earlier session; that turn answered \
             with no memory of the brief"
        );
    }
    match status.hook_reopening {
        None => {}
        Some(HookReopening::Expected) => println!(
            "  NOTE        a hook in the inherited settings reopened a finished turn; the answer \
             is above the injection"
        ),
        Some(HookReopening::Unexpected) => println!(
            "  WARNING     a hook reopened a finished turn although no settings were loaded; \
             treat the transcript as untrusted — the last message is not the answer"
        ),
    }
    println!("  transcript  {}", status.transcript_path.display());
    println!("  events      {}", status.events_path.display());
    if status.resumable {
        println!("  follow up   agentmux follow-up {} '…'", status.run_id);
    }
    Ok(())
}

/// Print a finished consultation's transcript, or say where to find it if it is still running.
///
/// # Errors
///
/// Returns an error when the transcript cannot be read or serialised.
pub fn answer(store: &RunStore, run_id: &RunId, json: bool) -> Result<()> {
    let (status, page) = store.view(run_id, 0, usize::MAX)?;
    if !status.is_terminal() {
        eprintln!(
            "still running after the wait. Nothing is lost — collect it with:\n  agentmux result \
             {} --wait 600",
            status.run_id
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
        let mut value = serde_json::to_value(status)?;
        if let Some(map) = value.as_object_mut() {
            map.insert("text".to_owned(), serde_json::json!(page.text));
            map.insert(
                "next_cursor".to_owned(),
                serde_json::json!(page.next_offset),
            );
            map.insert("at_end".to_owned(), serde_json::json!(page.at_end));
        }
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }
    print!("{}", page.text);
    if !page.at_end {
        eprintln!(
            "\n[truncated at {} of {} bytes; continue with --offset {}]",
            page.next_offset, page.total_bytes, page.next_offset
        );
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

    let mut cursor = args.cursor;
    loop {
        let (status, page) = store.view(args.run_id.id(), cursor, args.max_bytes)?;
        cursor = page.next_offset;

        if json {
            // The same shape every other `--json` form uses for the outcome, so one reader
            // serves them all.
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({
                    "run_id": status.run_id.as_str(),
                    "outcome": status.outcome,
                    "text": page.text,
                    "next_cursor": page.next_offset,
                    "at_end": page.at_end,
                }))?
            );
        } else {
            print!("{}", page.text);
        }

        if !args.follow {
            if !json && !page.at_end {
                eprintln!("\n[next cursor: {cursor}]");
            }
            return Ok(());
        }
        if status.is_terminal() && page.at_end {
            return Ok(());
        }
        tokio::time::sleep(POLL).await;
    }
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
        println!(
            "{}  {:<9}  {}\n    {}\n    {}",
            run.run_id,
            run.outcome.state(),
            run.created_at.to_rfc3339(),
            run.delegate,
            run.question
        );
    }
    Ok(())
}

/// Print the account aliases this machine defines, and the model rewrites it applies.
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
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "source": config.source,
                "project_source": config.project_source,
                "accounts": config.accounts,
                "defaults": config.defaults,
                "models": config.models,
                "launch": config.launch,
                "unknown": config.unknown,
            }))?
        );
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
        let withheld = |name: &str| {
            if agentmux::delegate::is_credential_name(name) {
                "; withheld when an account is named"
            } else {
                ""
            }
        };
        for name in config.launch.env.keys() {
            println!("              {name} (set in config{})", withheld(name));
        }
        for name in &config.launch.env_passthrough {
            println!(
                "              {name} (forwarded from this environment{})",
                withheld(name)
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
        // machine does with a name a caller hands it.
        let mut heading = "models";
        for (from, to) in config.model_rewrites(vendor) {
            println!("  {heading:<14} {from} -> {to}");
            heading = "";
        }
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
    if account.inherit_settings == Some(true) {
        how.push("inherits settings and hooks".to_owned());
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
pub fn quota(reported: &[agentmux::quota::AccountQuota], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(reported)?);
        return Ok(());
    }
    for entry in reported {
        for line in agentmux_mcp::render::quota_report(entry) {
            println!("{line}");
        }
        println!();
    }
    Ok(())
}
