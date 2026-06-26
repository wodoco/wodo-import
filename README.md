# wodo-import

[![Release](https://img.shields.io/github/v/release/wodoco/wodo-import)](https://github.com/wodoco/wodo-import/releases)
![Platforms](https://img.shields.io/badge/platforms-Linux%20%7C%20macOS%20%7C%20Windows-informational)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

Convert a **Jira** project or **Linear** team into a `wodo-space-export-v2` archive — the ZIP format the [Wodo](https://wodo.co) import wizard accepts. The format is [published and documented](https://wodo.co/export-format), so you can read it, inspect it, or build your own tooling against it.

You run it yourself, so your API token never leaves your machine. Point it at a project, get a ZIP, and upload that through Wodo's importer — or inspect and transform the archive first; it's just JSON + attachments.

> Prefer one click? Wodo also has an **in-app importer** — paste an API key and it fetches the project for you. This tool is the path for when you'd rather your token never touch Wodo's servers at all. Both finish at the same reviewed, people-mapped import step. See [Importing from Jira & Linear](https://wodo.co/learn/jira-and-linear/).

## Demo

![A wodo-import run: fetch, convert, write the ZIP](https://wodo.co/learn/import-export/wodo-import-terminal.gif)

## Install

Grab a prebuilt binary from [Releases](../../releases) — **Linux (x86-64, aarch64)**, **macOS (Apple Silicon)**, and **Windows (x86-64)**.

Or install with Cargo:

```sh
cargo install --git https://github.com/wodoco/wodo-import
```

Or build from source:

```sh
cargo build --release      # → target/release/wodo-import
```

## Usage

Credentials are read from the environment, never passed as flags. **Read-only access is enough** for both Jira and Linear — the tool only reads.

### Linear

Create a personal API key under **Linear → Settings → Security & access → Personal API keys**.

```sh
export LINEAR_API_KEY=lin_api_xxxxxxxx
wodo-import linear --team WMT          # → WMT-export.zip
```

| Flag | Purpose |
|------|---------|
| `--team` (required) | Linear team key, e.g. `WMT` |
| `--out` | Output path (default `{team}-export.zip`) |
| `--dry-run` | Fetch + convert + report, but don't write the ZIP |
| `--seed-page-size` | First GraphQL page size before auto-tuning (default 10) |
| `--complexity-target` | Target per-page GraphQL complexity (default 8000) |
| `--max-page-size` | Hard cap on page size (default 250) |
| `--base-url` | GraphQL endpoint (default Linear's) |

### Jira

Generate an API token at <https://id.atlassian.com/manage-profile/security/api-tokens>. **Jira Cloud only** (not Server / Data Center).

```sh
export JIRA_EMAIL=you@example.com
export JIRA_API_TOKEN=xxxxxxxx
export JIRA_BASE_URL=https://your-domain.atlassian.net
wodo-import jira --project MTR         # → MTR-export.zip
```

| Flag | Purpose |
|------|---------|
| `--project` (required) | Jira project key, e.g. `MTR` |
| `--base-url` | Jira Cloud base URL (or `JIRA_BASE_URL`) |
| `--out` | Output path (default `{project}-export.zip`) |
| `--dry-run` | Fetch + convert + report, but don't write the ZIP |
| `--page-size` | Issues per `/search/jql` page |

### Then import it into Wodo

The tool writes a ZIP; Wodo does the rest:

1. Run the command above to produce `{team|project}-export.zip`.
2. In Wodo, go to **Settings → Import** and upload the ZIP (chunked and resumable).
3. **Review before anything happens** — a preview of every item, document, comment, and attachment, plus a people-mapping table showing who matches a current member.
4. Approve, and land in your new space.

Full walkthrough: [Importing from Jira & Linear](https://wodo.co/learn/jira-and-linear/).

## What carries over

- **Issues → items**, with their **numbers intact** (`WMT-13` stays 13) and **multiple assignees** (users and teams).
- **Workflow states → a Status label**; "done" / "canceled" states become **completion states** automatically.
- **Priority, type, components, and labels → Wodo labels.** A Linear **label group** (say "Complexity" with Low / Medium / High) becomes a label with those exact values; ungrouped labels collect under a single **Tags** label.
- **Story points / t-shirt sizes → an ordered `Estimate` label.**
- **Comments** (threaded), with original authors — or an author-name snapshot when a user can't be matched.
- **Rich text** — headings, lists, code, tables, callouts, links, and inline images — rebuilt in Wodo's editor (Jira's ADF and Linear's Markdown alike).
- **Attachments**, downloaded and restored.
- **Milestones and cycles / sprints**, with dates preserved.

Creation and update timestamps are kept wherever the source exposes them.

## What doesn't carry over

A few things are dropped on purpose — Wodo's model is simpler than Jira's — and every one is surfaced as a warning in the import preview before anything is created:

- **Jira and Linear only**, for now. Asana, Trello, and others aren't supported yet — new sources land here first (see [Contributing](#contributing)).
- **Jira Cloud only** — Server / Data Center isn't supported.
- **One value per label per item.** An issue with two components keeps the first and **flags the rest**, rather than guessing.
- **Jira custom fields are dropped.** Standard fields map; a *custom* select — your own "Complexity" dropdown — does not. (The same idea modeled in Linear as a label group *does* come across, as above.)
- **People who don't match** a current member keep their names on comments, but their assignments are dropped rather than guessed.

## Privacy

Why this is a separate tool you run yourself:

- Your API token is read from the environment — never passed as a flag, never written to the ZIP, never logged.
- The only network calls are to Jira or Linear (and the attachment URLs they return). Nothing is sent to Wodo, and there is no telemetry.
- The output is a plain ZIP of JSON + attachments on your disk. Inspect it, diff it, transform it — then upload it to Wodo yourself when you're ready.

## Verifying your download

Each release includes a `SHA256SUMS` file. After downloading a binary:

```sh
sha256sum --check SHA256SUMS      # Linux
shasum -a 256 --check SHA256SUMS  # macOS
```

Or skip the prebuilt binaries entirely and build from source (above) — the whole tool is in this repo.

## Layout

- **`crates/wodo-interop`** — the pure conversion library: external-tracker → `SpaceExport`, the export types + ZIP packaging, and Markdown → Yjs encoding.
- **`crates/wodo-import`** — the CLI in this README.

## Contributing

New sources land here first. If you want Asana, Trello, GitHub Issues, or another tracker, the seam is small and self-contained: a converter takes the source's data and returns a `SpaceExport`. Everything downstream — ZIP packaging, Markdown → Yjs, the import preview — already lives in `crates/wodo-interop`.

- Read an existing converter (`jira`, `linear`) as a template.
- Open an issue describing the source's API and what maps to what.
- PRs welcome.

## License

Licensed under either of [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT) at your option.
