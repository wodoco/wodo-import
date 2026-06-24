# wodo-import

Convert a **Jira** project or **Linear** team into a `wodo-space-export-v2`
archive — the ZIP format the [Wodo](https://wodo.co) import wizard accepts.

You run it yourself, so your API token never leaves your machine. Point it at a
project, get a ZIP, and upload that through Wodo's importer — or inspect /
transform the archive first; it's just JSON + attachments.

## Install

Grab a prebuilt binary from [Releases](../../releases) (Windows x86-64 and
macOS Apple Silicon), or build from source:

```sh
cargo build --release      # → target/release/wodo-import
```

## Usage

Credentials are read from the environment, never passed as flags.

### Linear

```sh
export LINEAR_API_KEY=lin_api_xxxxxxxx
wodo-import linear --team WMT          # → WMT-export.zip
```

| Flag | Purpose |
|------|---------|
| `--team` (required) | Linear team key, e.g. `WMT` |
| `--out` | Output path (default `{team}-export.zip`) |
| `--state-dir` | Directory for resume cursors — enables crash-resume |
| `--dry-run` | Fetch + convert + report, but don't write the ZIP |
| `--seed-page-size` | First GraphQL page size before auto-tuning (default 10) |
| `--complexity-target` | Target per-page GraphQL complexity (default 8000) |
| `--max-page-size` | Hard cap on page size (default 250) |
| `--base-url` | GraphQL endpoint (default Linear's) |

### Jira

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
| `--state-dir` | Directory for the resume token — enables crash-resume |
| `--dry-run` | Fetch + convert + report, but don't write the ZIP |
| `--page-size` | Issues per `/search/jql` page |

Generate a Jira API token at <https://id.atlassian.com/manage-profile/security/api-tokens>.

## What carries over

- Issues → items, with **multiple assignees** (users and teams)
- Workflow states & labels → Wodo labels and values; **story points / t-shirt
  sizes** map to an ordered `Estimate` label
- Comments (threaded), with author-name snapshots when a user can't be matched
- Attachments, and rich text — headings, lists, code, tables, callouts, links,
  and inline images
- Milestones and cycles/sprints, with dates preserved

Completion state, archived/done status, and creation/update timestamps are
preserved where the source exposes them.

## Layout

- **`crates/wodo-interop`** — the pure conversion library: external-tracker →
  `SpaceExport`, the export types + ZIP packaging, and Markdown → Yjs encoding.
- **`crates/wodo-import`** — the CLI in this README.

## License

Licensed under either of [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT) at
your option.
