//! `wodo-import` CLI — thin flag-parse → one `wodo_import::run*` call.
//!
//! Secrets are NOT flags; they are read from the environment (source `.env`
//! first): `LINEAR_API_KEY` for Linear; `JIRA_EMAIL` + `JIRA_API_TOKEN` for
//! Jira.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use wodo_import::{JiraRunOpts, RunOpts};
use wodo_interop::import::jira_fetch::DEFAULT_PAGE_SIZE;
use wodo_interop::import::linear_fetch::DEFAULT_BASE_URL;

#[derive(Parser)]
#[command(
    name = "wodo-import",
    about = "Import data from external tools into a Wodo SpaceExport ZIP",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Pull a Linear team via GraphQL and write a SpaceExport v2 ZIP.
    Linear {
        /// Linear team key (e.g. WMT).
        #[arg(long)]
        team: String,

        /// Output ZIP path (default: {team}-export.zip).
        #[arg(long)]
        out: Option<PathBuf>,

        /// Fetch + convert + report, but don't write the ZIP.
        #[arg(long)]
        dry_run: bool,

        /// First page size before the auto-tuner adjusts it.
        #[arg(long = "seed-page-size", default_value_t = 10)]
        seed_page_size: usize,

        /// Target per-page GraphQL complexity for the auto-tuner.
        #[arg(long = "complexity-target", default_value_t = 8000.0)]
        complexity_target: f64,

        /// Hard upper bound on page size (Linear caps connections at 250).
        #[arg(long = "max-page-size", default_value_t = 250)]
        max_page_size: usize,

        /// GraphQL endpoint.
        #[arg(long = "base-url", default_value = DEFAULT_BASE_URL)]
        base_url: String,
    },

    /// Pull a Jira project via REST and write a SpaceExport v2 ZIP.
    ///
    /// Auth is env-only: JIRA_EMAIL + JIRA_API_TOKEN (source .env first).
    Jira {
        /// Jira project key (e.g. MTR).
        #[arg(long)]
        project: String,

        /// Jira Cloud base URL (e.g. https://your-domain.atlassian.net).
        /// Falls back to the JIRA_BASE_URL env var.
        #[arg(long = "base-url", env = "JIRA_BASE_URL")]
        base_url: Option<String>,

        /// Output ZIP path (default: {project}-export.zip).
        #[arg(long)]
        out: Option<PathBuf>,

        /// Fetch + convert + report, but don't write the ZIP.
        #[arg(long)]
        dry_run: bool,

        /// Issues per /search/jql page (maxResults).
        #[arg(long = "page-size", default_value_t = DEFAULT_PAGE_SIZE)]
        page_size: usize,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Linear {
            team,
            out,
            dry_run,
            seed_page_size,
            complexity_target,
            max_page_size,
            base_url,
        } => {
            wodo_import::run(RunOpts {
                team,
                out,
                dry_run,
                seed_page_size,
                complexity_target,
                max_page_size,
                base_url,
            })
            .await
        }
        Command::Jira {
            project,
            base_url,
            out,
            dry_run,
            page_size,
        } => {
            wodo_import::run_jira(JiraRunOpts {
                project,
                base_url,
                out,
                dry_run,
                page_size,
            })
            .await
        }
    }
}
