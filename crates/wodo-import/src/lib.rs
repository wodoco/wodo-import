//! `wodo-import` — pull a Linear team and write a SpaceExport v2 ZIP for the
//! existing import wizard.
//!
//! All the real work lives in `wodo-interop`
//! ([`wodo_interop::import::linear_fetch`]); this crate is the fs/CLI glue:
//! read the API key from the env, invoke the fetcher, and write the ZIP. The
//! binary ([`crate`]'s `main`) is a thin flag-parse → one [`run`] call.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use wodo_interop::import::jira_fetch::{run_jira_import, JiraImportOptions};
use wodo_interop::import::linear_fetch::{
    run_linear_import, LinearImportOptions, DEFAULT_BASE_URL,
};

/// Options for one import run (built by the bin from CLI flags).
pub struct RunOpts {
    /// Linear team key (e.g. `WMT`).
    pub team: String,
    /// Output ZIP path; defaults to `{team}-export.zip`.
    pub out: Option<PathBuf>,
    /// Fetch + report but don't write the ZIP.
    pub dry_run: bool,
    /// First page size before auto-tune.
    pub seed_page_size: usize,
    /// Target per-page complexity for the auto-tuner.
    pub complexity_target: f64,
    /// Hard cap on page size.
    pub max_page_size: usize,
    /// GraphQL endpoint.
    pub base_url: String,
}

/// Run a Linear import end to end.
pub async fn run(opts: RunOpts) -> Result<()> {
    let api_key = std::env::var("LINEAR_API_KEY")
        .context("LINEAR_API_KEY is not set (source .env or export it)")?;

    let fetch_opts = LinearImportOptions {
        team_key: opts.team.clone(),
        base_url: if opts.base_url.is_empty() {
            DEFAULT_BASE_URL.to_string()
        } else {
            opts.base_url.clone()
        },
        seed_page_size: opts.seed_page_size,
        complexity_target: opts.complexity_target,
        max_page_size: opts.max_page_size,
    };

    let client = http_client()?;
    let out_path = opts
        .out
        .clone()
        .unwrap_or_else(|| PathBuf::from(format!("{}-export.zip", opts.team)));

    // The CLI doesn't expose resume: always run start-to-finish with an empty
    // cursor map and a no-op checkpoint. (The fetcher still supports both.)
    let resume = HashMap::new();
    let checkpoint = |_: &str, _: &str| {};

    // Dry run: stream into an in-memory buffer and discard it (no file written).
    if opts.dry_run {
        let output = run_linear_import(
            &client,
            std::io::Cursor::new(Vec::new()),
            &api_key,
            &fetch_opts,
            &resume,
            checkpoint,
        )
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
        println!("{}", output.summary);
        print_warnings(&output.warnings);
        println!(
            "\nDry run: fetched and converted {} bytes of ZIP (not written).",
            output.bytes_written
        );
        return Ok(());
    }

    // Stream the archive straight to disk — never holds the whole ZIP in memory.
    let sink = std::io::BufWriter::new(
        std::fs::File::create(&out_path)
            .with_context(|| format!("creating {}", out_path.display()))?,
    );
    let output = run_linear_import(&client, sink, &api_key, &fetch_opts, &resume, checkpoint)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;

    println!("{}", output.summary);
    print_warnings(&output.warnings);
    println!(
        "\nWrote {} ({} bytes)",
        out_path.display(),
        output.bytes_written
    );

    Ok(())
}

/// Options for one Jira import run (built by the bin from CLI flags).
pub struct JiraRunOpts {
    /// Jira project key (e.g. `MTR`).
    pub project: String,
    /// Jira Cloud base URL (e.g. `https://your-domain.atlassian.net`); falls
    /// back to `JIRA_BASE_URL` when `None`.
    pub base_url: Option<String>,
    /// Output ZIP path; defaults to `{project}-export.zip`.
    pub out: Option<PathBuf>,
    /// Fetch + report but don't write the ZIP.
    pub dry_run: bool,
    /// Issues per `/search/jql` page (`maxResults`).
    pub page_size: usize,
}

/// Run a Jira import end to end.
///
/// `JIRA_EMAIL` + `JIRA_API_TOKEN` are read from the environment (never flags);
/// the base URL comes from `--base-url` or `JIRA_BASE_URL`.
pub async fn run_jira(opts: JiraRunOpts) -> Result<()> {
    let email =
        std::env::var("JIRA_EMAIL").context("JIRA_EMAIL is not set (source .env or export it)")?;
    let api_token = std::env::var("JIRA_API_TOKEN")
        .context("JIRA_API_TOKEN is not set (source .env or export it)")?;
    let base_url = opts
        .base_url
        .clone()
        .or_else(|| std::env::var("JIRA_BASE_URL").ok())
        .filter(|u| !u.is_empty())
        .context("no Jira base URL: pass --base-url or set JIRA_BASE_URL")?;

    let fetch_opts = JiraImportOptions {
        project_key: opts.project.clone(),
        base_url,
        page_size: opts.page_size,
    };

    // The CLI doesn't expose resume: always run start-to-finish with an empty
    // cursor map and a no-op checkpoint. (The fetcher still supports both.)
    let resume = HashMap::new();
    let checkpoint = |_: &str, _: &str| {};

    let client = http_client()?;
    let out_path = opts
        .out
        .clone()
        .unwrap_or_else(|| PathBuf::from(format!("{}-export.zip", opts.project)));

    if opts.dry_run {
        let output = run_jira_import(
            &client,
            std::io::Cursor::new(Vec::new()),
            &email,
            &api_token,
            &fetch_opts,
            &resume,
            checkpoint,
        )
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
        println!("{}", output.summary);
        print_warnings(&output.warnings);
        println!(
            "\nDry run: fetched and converted {} bytes of ZIP (not written).",
            output.bytes_written
        );
        return Ok(());
    }

    let sink = std::io::BufWriter::new(
        std::fs::File::create(&out_path)
            .with_context(|| format!("creating {}", out_path.display()))?,
    );
    let output = run_jira_import(
        &client,
        sink,
        &email,
        &api_token,
        &fetch_opts,
        &resume,
        checkpoint,
    )
    .await
    .map_err(|e| anyhow::anyhow!(e))?;

    println!("{}", output.summary);
    print_warnings(&output.warnings);
    println!(
        "\nWrote {} ({} bytes)",
        out_path.display(),
        output.bytes_written
    );

    Ok(())
}

/// Shared HTTP client. A connect timeout avoids hanging forever on a dead host;
/// downloads themselves are left untimed (attachments can be large + slow).
fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()
        .context("building HTTP client")
}

/// Print non-fatal warnings to stderr, if any.
fn print_warnings(warnings: &[String]) {
    if !warnings.is_empty() {
        eprintln!("\n{} warning(s):", warnings.len());
        for w in warnings {
            eprintln!("  - {w}");
        }
    }
}
