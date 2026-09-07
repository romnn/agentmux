//! How a command's result reaches the terminal.
//!
//! Changes when the printed output changes.
//!
//! Every command prints prose by default and JSON under `--json`, and the two carry the same
//! facts.
//! Prose goes to stdout so it can be piped; progress and warnings go to stderr through `tracing`.

use std::time::Duration;

use agentmux::run::{RunStatus, RunStore, RunSummary, TranscriptPage};
use color_eyre::eyre::Result;

use crate::cli::TailArgs;

/// Print a consultation's state.
///
/// # Errors
///
/// Returns an error when the JSON form cannot be serialised.
pub fn status(status: &RunStatus, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&json_status(status))?);
        return Ok(());
    }
    println!("{}  {}", status.run_id, status.delegate.summary());
    println!(
        "  state       {}",
        agentmux_mcp::render::outcome_line(&status.outcome)
    );
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
    if let Some(drift) = status.unrecognised.summary() {
        println!("  UNRECOGNISED {drift} — the delegate CLI may have changed its output format");
    }
    if status.reopened_by_hook {
        println!(
            "  NOTE        a hook reopened a finished turn; the last message is not the answer"
        );
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
pub fn answer(store: &RunStore, status: &RunStatus, json: bool) -> Result<()> {
    if !status.is_terminal() {
        eprintln!(
            "still running after the wait. Nothing is lost — collect it with:\n  agentmux result \
             {} --wait 600",
            status.run_id
        );
        return self::status(status, json);
    }
    let page = store.read_transcript(&status.run_id, 0, usize::MAX)?;
    transcript(status, &page, json)
}

/// Print a slice of a transcript.
///
/// # Errors
///
/// Returns an error when the JSON form cannot be serialised.
pub fn transcript(status: &RunStatus, page: &TranscriptPage, json: bool) -> Result<()> {
    if json {
        let mut value = json_status(status);
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
        let status = store.status(&args.run_id.0)?;
        let page = store.read_transcript(&args.run_id.0, cursor, args.max_bytes)?;
        cursor = page.next_offset;

        if json {
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({
                    "run_id": status.run_id.as_str(),
                    "state": status.outcome.state(),
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
        let rows: Vec<_> = runs
            .iter()
            .map(|run| {
                serde_json::json!({
                    "run_id": run.run_id.as_str(),
                    "delegate": run.delegate,
                    "state": run.state,
                    "created_at": run.created_at.to_rfc3339(),
                    "question": run.question,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
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
            run.state,
            run.created_at.to_rfc3339(),
            run.delegate,
            run.question
        );
    }
    Ok(())
}

fn json_status(status: &RunStatus) -> serde_json::Value {
    serde_json::json!({
        "run_id": status.run_id.as_str(),
        "delegate": status.delegate,
        "state": status.outcome.state(),
        "outcome": status.outcome,
        "created_at": status.created_at.to_rfc3339(),
        "elapsed_seconds": status.elapsed.as_secs(),
        "turns": status.turns,
        "message_count": status.message_count,
        "usage": status.usage,
        "cost_usd": status.cost_usd,
        "unrecognised_events": status.unrecognised,
        "reopened_by_hook": status.reopened_by_hook,
        "rate_limit": status.rate_limit,
        "cwd": status.cwd,
        "transcript_path": status.transcript_path,
        "events_path": status.events_path,
        "stderr_path": status.stderr_path,
        "transcript_bytes": status.transcript_bytes,
        "resumable": status.resumable,
    })
}

/// Print the account aliases this machine defines.
///
/// The point of the command is discovery: an alias is a name someone has to know, and the error
/// for guessing wrong is only visible after a consultation has been attempted.
///
/// # Errors
///
/// Returns an error only when the JSON form cannot be serialised.
pub fn accounts(config: &agentmux::config::Config, json: bool) -> Result<()> {
    use agentmux::delegate::Vendor;

    if json {
        println!("{}", serde_json::to_string_pretty(config)?);
        return Ok(());
    }

    match &config.source {
        Some(path) => println!("config      {}", path.display()),
        None => println!(
            "config      none found (searched from the working directory up to $HOME, then the \
             platform config directory)"
        ),
    }

    for vendor in [Vendor::Claude, Vendor::Codex] {
        println!("\n{vendor}");
        let accounts = config.accounts(vendor);
        if accounts.is_empty() {
            println!("  (none configured — omit --account to use the CLI's own default)");
            continue;
        }
        for (alias, account) in accounts {
            // Say what each alias would actually do, without printing a credential.
            let mut how = Vec::new();
            if let Some(dir) = &account.config_dir {
                how.push(format!("config_dir={}", dir.display()));
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
            println!("  {alias:<16} {}", how.join(", "));
        }
    }
    Ok(())
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
