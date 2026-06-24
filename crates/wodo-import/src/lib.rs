//! `wodo-import` — pull a Linear team and write a SpaceExport v2 ZIP for the
//! existing import wizard.
//!
//! All the real work lives in `wodo-interop`
//! ([`wodo_interop::import::linear_fetch`]); this crate is the fs/CLI glue:
//! read the API key from the env, load/persist resume cursors, invoke the
//! fetcher, and write the ZIP. The binary ([`crate`]'s `main`) is a thin
//! flag-parse → one [`run`] call.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use wodo_interop::import::jira_fetch::{run_jira_import, JiraImportOptions};
use wodo_interop::import::linear_fetch::{
    run_linear_import, LinearImportOptions, DEFAULT_BASE_URL,
};

/// Filename under `--state-dir` holding the per-stream resume cursors.
const CURSORS_FILE: &str = "cursors.json";

/// Filename under `--state-dir` holding the Jira resume page token.
const JIRA_CURSORS_FILE: &str = "jira-cursors.json";

/// Options for one import run (built by the bin from CLI flags).
pub struct RunOpts {
    /// Linear team key (e.g. `WMT`).
    pub team: String,
    /// Output ZIP path; defaults to `{team}-export.zip`.
    pub out: Option<PathBuf>,
    /// Directory for resume cursors (`cursors.json`); when absent, no resume.
    pub state_dir: Option<PathBuf>,
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

    let resume = load_cursors(opts.state_dir.as_deref())?;
    if !resume.is_empty() {
        eprintln!(
            "Resuming from {} saved cursor(s) in {}",
            resume.len(),
            state_path(opts.state_dir.as_deref().unwrap()).display()
        );
    }

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

    // Checkpoint closure: persist each (stream → cursor) so a crash resumes.
    // Keeps the running map in this scope; the fetcher calls it after each page.
    let state_dir = opts.state_dir.clone();
    let mut cursors = resume.clone();
    let checkpoint = move |stream: &str, cursor: &str| {
        cursors.insert(stream.to_string(), cursor.to_string());
        if let Err(e) = save_cursors(state_dir.as_deref(), &cursors) {
            eprintln!("warning: could not checkpoint cursors: {e:#}");
        }
    };

    let output = run_linear_import(&api_key, &fetch_opts, &resume, checkpoint)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;

    println!("{}", output.summary);
    if !output.warnings.is_empty() {
        eprintln!("\n{} warning(s):", output.warnings.len());
        for w in &output.warnings {
            eprintln!("  - {w}");
        }
    }

    if opts.dry_run {
        println!(
            "\nDry run: fetched and converted {} bytes of ZIP (not written).",
            output.zip.len()
        );
        return Ok(());
    }

    let out_path = opts
        .out
        .clone()
        .unwrap_or_else(|| PathBuf::from(format!("{}-export.zip", opts.team)));
    std::fs::write(&out_path, &output.zip)
        .with_context(|| format!("writing {}", out_path.display()))?;
    println!(
        "\nWrote {} ({} bytes)",
        out_path.display(),
        output.zip.len()
    );

    // A completed run's cursors are stale; leave them for the user to inspect
    // but note the run finished.
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
    /// Directory for the resume token (`jira-cursors.json`); when absent, no
    /// resume.
    pub state_dir: Option<PathBuf>,
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

    let resume = load_jira_cursors(opts.state_dir.as_deref())?;
    if !resume.is_empty() {
        eprintln!(
            "Resuming from saved page token in {}",
            jira_state_path(opts.state_dir.as_deref().unwrap()).display()
        );
    }

    let fetch_opts = JiraImportOptions {
        project_key: opts.project.clone(),
        base_url,
        page_size: opts.page_size,
    };

    // Checkpoint closure: persist the latest (stream → token) so a crash can
    // resume. Jira page tokens may be short-lived, so this is best-effort.
    let state_dir = opts.state_dir.clone();
    let mut cursors = resume.clone();
    let checkpoint = move |stream: &str, token: &str| {
        cursors.insert(stream.to_string(), token.to_string());
        if let Err(e) = save_jira_cursors(state_dir.as_deref(), &cursors) {
            eprintln!("warning: could not checkpoint Jira token: {e:#}");
        }
    };

    let output = run_jira_import(&email, &api_token, &fetch_opts, &resume, checkpoint)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;

    println!("{}", output.summary);
    if !output.warnings.is_empty() {
        eprintln!("\n{} warning(s):", output.warnings.len());
        for w in &output.warnings {
            eprintln!("  - {w}");
        }
    }

    if opts.dry_run {
        println!(
            "\nDry run: fetched and converted {} bytes of ZIP (not written).",
            output.zip.len()
        );
        return Ok(());
    }

    let out_path = opts
        .out
        .clone()
        .unwrap_or_else(|| PathBuf::from(format!("{}-export.zip", opts.project)));
    std::fs::write(&out_path, &output.zip)
        .with_context(|| format!("writing {}", out_path.display()))?;
    println!(
        "\nWrote {} ({} bytes)",
        out_path.display(),
        output.zip.len()
    );

    Ok(())
}

fn state_path(dir: &std::path::Path) -> PathBuf {
    dir.join(CURSORS_FILE)
}

fn jira_state_path(dir: &std::path::Path) -> PathBuf {
    dir.join(JIRA_CURSORS_FILE)
}

/// Load the Jira resume token from `state_dir/jira-cursors.json` if present.
fn load_jira_cursors(state_dir: Option<&std::path::Path>) -> Result<HashMap<String, String>> {
    let Some(dir) = state_dir else {
        return Ok(HashMap::new());
    };
    let path = jira_state_path(dir);
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let map: HashMap<String, String> =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(map)
}

/// Write the Jira resume token to `state_dir/jira-cursors.json` (creating the dir).
fn save_jira_cursors(
    state_dir: Option<&std::path::Path>,
    cursors: &HashMap<String, String>,
) -> Result<()> {
    let Some(dir) = state_dir else {
        return Ok(());
    };
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = jira_state_path(dir);
    let json = serde_json::to_vec_pretty(cursors)?;
    std::fs::write(&path, json).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Load resume cursors from `state_dir/cursors.json` if present.
fn load_cursors(state_dir: Option<&std::path::Path>) -> Result<HashMap<String, String>> {
    let Some(dir) = state_dir else {
        return Ok(HashMap::new());
    };
    let path = state_path(dir);
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let map: HashMap<String, String> =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(map)
}

/// Write resume cursors to `state_dir/cursors.json` (creating the dir).
fn save_cursors(
    state_dir: Option<&std::path::Path>,
    cursors: &HashMap<String, String>,
) -> Result<()> {
    let Some(dir) = state_dir else {
        return Ok(());
    };
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = state_path(dir);
    let json = serde_json::to_vec_pretty(cursors)?;
    std::fs::write(&path, json).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}
