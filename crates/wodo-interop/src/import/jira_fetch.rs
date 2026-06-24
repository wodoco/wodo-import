//! Fetch a Jira project over REST and produce an import-wizard ZIP.
//!
//! The sibling of [`crate::import::linear_fetch`], but much simpler: Jira's
//! `/search/jql` endpoint returns every issue fully **denormalized** — status,
//! priority, type, components, labels, fixVersions, sprints, parent, links,
//! attachments, and the whole comment thread all live on `fields` — so there is
//! ONE paginated stream, not ten. The pure converter
//! ([`crate::import::convert::jira`]) does all the cross-referencing client-side.
//!
//! Auth is HTTP Basic (`email:api_token`), the Atlassian Cloud API-token scheme.
//! Pagination is token-based (`nextPageToken` / `isLast`), not cursor+hasNext.
//! Rate limiting is burst-based, not a points budget, so the only throttle is an
//! HTTP 429 + `Retry-After` retry — no complexity auto-tune.
//!
//! As in the Linear fetcher, reqwest is used directly. The crate's reqwest has
//! `default-features = false` (no `json` feature), so request bodies are
//! serialized with `serde_json` and responses are read as text. Basic auth uses
//! the built-in `RequestBuilder::basic_auth`, which needs no extra feature.

use std::collections::HashMap;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use crate::export::package::write_space_export_zip;
use crate::import::convert::jira::{self, JiraIssue, JiraProject};

/// Default page size for the issue search before any override.
pub const DEFAULT_PAGE_SIZE: usize = 50;

/// Tuning knobs for a Jira import run.
pub struct JiraImportOptions {
    /// Project key (e.g. `MTR`).
    pub project_key: String,
    /// Jira Cloud base URL, e.g. `https://your-domain.atlassian.net` (no
    /// trailing `/rest/...`).
    pub base_url: String,
    /// Issues fetched per `/search/jql` page (`maxResults`).
    pub page_size: usize,
}

/// Result of a successful import run.
pub struct JiraImportOutput {
    /// The import-wizard ZIP (`data.json` + `attachments/…`).
    pub zip: Vec<u8>,
    /// Non-fatal fidelity/transfer warnings (converter + attachment downloads).
    pub warnings: Vec<String>,
    /// Human-readable summary (issue count, attachments downloaded, warnings).
    pub summary: String,
}

// =============================================================================
// Pure helpers (unit-tested below)
// =============================================================================

/// Build the `/search/jql` request body for one page.
///
/// `project_key` is embedded in the JQL (single-quoted, Jira's string-literal
/// quote, so the key is escaped defensively). `page_token` is the opaque
/// `nextPageToken` from the previous page, omitted on the first page. `*all`
/// pulls every field so the denormalized converter has everything it needs.
pub fn build_search_body(project_key: &str, page_size: usize, page_token: Option<&str>) -> Value {
    let jql = format!(
        "project = '{}' ORDER BY created ASC",
        jql_escape(project_key)
    );
    let mut body = serde_json::json!({
        "jql": jql,
        "fields": ["*all"],
        "maxResults": page_size,
    });
    if let Some(token) = page_token {
        body["nextPageToken"] = Value::String(token.to_string());
    }
    body
}

/// Escape a string for embedding inside a JQL single-quoted literal.
fn jql_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}

/// One decoded `/search/jql` page: the raw issue values, whether it's the last
/// page, and the token for the next one. `is_last` OR a missing `next_token`
/// terminates the loop (Jira sets `isLast` on the final page and may also stop
/// emitting a token).
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchPage {
    #[serde(default)]
    issues: Vec<Value>,
    #[serde(default)]
    is_last: bool,
    #[serde(default)]
    next_page_token: Option<String>,
}

impl SearchPage {
    /// Whether the pagination loop should stop after this page.
    fn done(&self) -> bool {
        self.is_last || self.next_page_token.as_deref().unwrap_or("").is_empty()
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}

// =============================================================================
// HTTP plumbing
// =============================================================================

/// Trim a trailing slash so `{base}/rest/...` joins cleanly.
fn base(opts: &JiraImportOptions) -> &str {
    opts.base_url.trim_end_matches('/')
}

/// POST `/search/jql`, honoring HTTP 429 `Retry-After` with a bounded retry
/// loop. Returns the decoded page.
async fn search_page(
    client: &reqwest::Client,
    base_url: &str,
    email: &str,
    api_token: &str,
    project_key: &str,
    page_size: usize,
    page_token: Option<&str>,
) -> Result<SearchPage, String> {
    let url = format!("{base_url}/rest/api/3/search/jql");
    let body = build_search_body(project_key, page_size, page_token);
    let body = serde_json::to_vec(&body).map_err(|e| format!("encode request: {e}"))?;

    for attempt in 0..6u32 {
        let resp = client
            .post(&url)
            .basic_auth(email, Some(api_token))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .body(body.clone())
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;

        let status = resp.status();
        if status.as_u16() == 429 {
            let wait = retry_after_secs(resp.headers());
            tracing::warn!("Jira 429 (attempt {attempt}); sleeping {wait}s");
            tokio::time::sleep(Duration::from_secs(wait)).await;
            continue;
        }

        let text = resp.text().await.map_err(|e| format!("read body: {e}"))?;
        if !status.is_success() {
            return Err(format!(
                "HTTP {}: {}",
                status.as_u16(),
                truncate(&text, 500)
            ));
        }
        return serde_json::from_str::<SearchPage>(&text)
            .map_err(|e| format!("parse search response JSON: {e}"));
    }
    Err("too many 429 retries".to_string())
}

/// `Retry-After` in seconds (defaults to 15 when absent/unparseable).
fn retry_after_secs(headers: &reqwest::header::HeaderMap) -> u64 {
    headers
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(15)
}

/// Fetch the project meta (id/key/name) by key. The converter only needs these
/// three fields; everything else in the payload is ignored.
async fn fetch_project(
    client: &reqwest::Client,
    base_url: &str,
    email: &str,
    api_token: &str,
    project_key: &str,
) -> Result<JiraProject, String> {
    let url = format!("{base_url}/rest/api/3/project/{project_key}");
    let resp = client
        .get(&url)
        .basic_auth(email, Some(api_token))
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| format!("project request failed: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| format!("read body: {e}"))?;
    if !status.is_success() {
        return Err(format!(
            "project HTTP {}: {}",
            status.as_u16(),
            truncate(&text, 300)
        ));
    }
    serde_json::from_str(&text).map_err(|e| format!("decode project: {e}"))
}

/// Download an attachment's bytes with Basic auth.
async fn download_attachment(
    client: &reqwest::Client,
    email: &str,
    api_token: &str,
    url: &str,
) -> Result<Vec<u8>, String> {
    let resp = client
        .get(url)
        .basic_auth(email, Some(api_token))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status().as_u16()));
    }
    resp.bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| e.to_string())
}

// =============================================================================
// Orchestration
// =============================================================================

/// Fetch a Jira project and build an import-wizard ZIP.
///
/// Auth is HTTP Basic (`email:api_token`). `resume` may carry a starting page
/// token under the key `"issues"`; `checkpoint("issues", token)` is invoked
/// after every page so the caller can persist progress. (Jira page tokens can be
/// short-lived, so resume is best-effort — a stale token simply restarts the
/// search from the beginning on the next run.)
pub async fn run_jira_import(
    email: &str,
    api_token: &str,
    opts: &JiraImportOptions,
    resume: &HashMap<String, String>,
    mut checkpoint: impl FnMut(&str, &str),
) -> Result<JiraImportOutput, String> {
    let base_url = base(opts).to_string();
    let page_size = opts.page_size.max(1);
    let client = reqwest::Client::new();

    // ── Issues: one denormalized, token-paginated stream ──────────────────────
    let mut issue_values: Vec<Value> = Vec::new();
    let mut token: Option<String> = resume.get("issues").cloned().filter(|t| !t.is_empty());

    loop {
        let page = search_page(
            &client,
            &base_url,
            email,
            api_token,
            &opts.project_key,
            page_size,
            token.as_deref(),
        )
        .await
        .map_err(|e| format!("[issues] {e}"))?;

        let done = page.done();
        let got = page.issues.len();
        issue_values.extend(page.issues);

        if let Some(t) = page.next_page_token.as_deref().filter(|t| !t.is_empty()) {
            checkpoint("issues", t);
        }
        token = page.next_page_token;

        tracing::debug!(
            "[issues] +{got} (total {}) is_last={} next={}",
            issue_values.len(),
            page.is_last,
            token.is_some(),
        );

        if done {
            break;
        }
    }

    // ── Deserialize each raw issue into the converter input ───────────────────
    let issues: Vec<JiraIssue> = issue_values
        .into_iter()
        .map(|v| serde_json::from_value(v).map_err(|e| format!("[issues] decode issue: {e}")))
        .collect::<Result<_, _>>()?;

    // ── Project meta: prefer the REST project endpoint, fall back to the first
    //    issue's embedded `fields.project` ─────────────────────────────────────
    let project = match fetch_project(&client, &base_url, email, api_token, &opts.project_key).await
    {
        Ok(p) => p,
        Err(e) => issues
            .first()
            .map(|i| i.fields.project.clone())
            .filter(|p| !p.id.is_empty())
            .ok_or_else(|| format!("project lookup failed and no issues to fall back on: {e}"))?,
    };

    // ── Convert → export ──────────────────────────────────────────────────────
    let conv = jira::jira_to_space_export(&project, &issues);
    let mut warnings = conv.warnings;
    let export = conv.export;

    // Filename per minted attachment id from the export manifest.
    let filename_by_att: HashMap<&str, &str> = export
        .attachments
        .iter()
        .map(|a| (a.id.as_str(), a.filename.as_str()))
        .collect();

    // ── Download attachment bytes (warn-and-skip on failure) ──────────────────
    let mut blobs: Vec<(String, String, Vec<u8>)> = Vec::new();
    let mut downloaded = 0usize;
    for (att_id, url) in &conv.attachment_urls {
        let Some(&filename) = filename_by_att.get(att_id.as_str()) else {
            warnings.push(format!("attachment {att_id}: no manifest entry; skipped"));
            continue;
        };
        match download_attachment(&client, email, api_token, url).await {
            Ok(bytes) => {
                blobs.push((att_id.clone(), filename.to_string(), bytes));
                downloaded += 1;
            }
            Err(e) => warnings.push(format!("attachment {att_id} ({url}): download failed: {e}")),
        }
    }

    // Jira already supplied size/mime, so no size backfill is needed.
    let zip = write_space_export_zip(&export, &blobs)?;
    let summary = format!(
        "Jira import summary:\n\
         \x20 project:         {} ({})\n\
         \x20 issues:          {}\n\
         \x20 items in export: {}\n\
         \x20 attachments:     {} in manifest → {} downloaded\n\
         \x20 warnings:        {}",
        project.name,
        project.key,
        issues.len(),
        export.items.len(),
        export.attachments.len(),
        downloaded,
        warnings.len(),
    );

    Ok(JiraImportOutput {
        zip,
        warnings,
        summary,
    })
}

// =============================================================================
// Tests (pure helpers + offline page-loop; no network)
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_body_first_page_has_no_token() {
        let body = build_search_body("MTR", 50, None);
        assert_eq!(
            body["jql"].as_str(),
            Some("project = 'MTR' ORDER BY created ASC")
        );
        assert_eq!(body["fields"], serde_json::json!(["*all"]));
        assert_eq!(body["maxResults"].as_u64(), Some(50));
        assert!(
            body.get("nextPageToken").is_none(),
            "first page omits nextPageToken"
        );
    }

    #[test]
    fn search_body_later_page_carries_token() {
        let body = build_search_body("SCRUM", 25, Some("tok-abc"));
        assert_eq!(body["nextPageToken"].as_str(), Some("tok-abc"));
        assert_eq!(body["maxResults"].as_u64(), Some(25));
        assert_eq!(
            body["jql"].as_str(),
            Some("project = 'SCRUM' ORDER BY created ASC")
        );
    }

    #[test]
    fn search_body_escapes_project_key() {
        // Defensive: a key with a quote can't break out of the JQL literal.
        let body = build_search_body("A'B\\C", 10, None);
        assert_eq!(
            body["jql"].as_str(),
            Some("project = 'A\\'B\\\\C' ORDER BY created ASC")
        );
    }

    #[test]
    fn page_done_on_is_last() {
        let page: SearchPage = serde_json::from_value(serde_json::json!({
            "issues": [{"id": "1"}],
            "isLast": true,
            "nextPageToken": "still-here"
        }))
        .unwrap();
        assert_eq!(page.issues.len(), 1);
        assert!(
            page.done(),
            "isLast=true terminates even with a token present"
        );
    }

    #[test]
    fn page_done_on_absent_token() {
        // No isLast, no token ⇒ done (the real `/search/jql` final page).
        let page: SearchPage = serde_json::from_value(serde_json::json!({
            "issues": [{"id": "1"}]
        }))
        .unwrap();
        assert!(!page.is_last);
        assert!(page.next_page_token.is_none());
        assert!(page.done());
    }

    #[test]
    fn page_continues_with_token_and_not_last() {
        let page: SearchPage = serde_json::from_value(serde_json::json!({
            "issues": [{"id": "1"}, {"id": "2"}],
            "isLast": false,
            "nextPageToken": "next-tok"
        }))
        .unwrap();
        assert!(!page.done(), "a token + isLast=false means keep going");
        assert_eq!(page.next_page_token.as_deref(), Some("next-tok"));
    }

    /// The page loop's pure core: drive two canned pages then a terminator and
    /// assert it accumulates every issue, threads the token forward, and
    /// checkpoints each page. No HTTP — exercises the same termination logic
    /// (`SearchPage::done` + token threading) the real loop uses.
    #[test]
    fn page_loop_accumulates_until_done() {
        // Two non-final pages then a final page (isLast).
        let pages = vec![
            serde_json::json!({"issues":[{"id":"1"},{"id":"2"}],"isLast":false,"nextPageToken":"t1"}),
            serde_json::json!({"issues":[{"id":"3"}],"isLast":false,"nextPageToken":"t2"}),
            serde_json::json!({"issues":[{"id":"4"},{"id":"5"}],"isLast":true,"nextPageToken":null}),
        ];

        let mut all: Vec<Value> = Vec::new();
        let mut token: Option<String> = None;
        let mut tokens_seen_by_request: Vec<Option<String>> = Vec::new();
        let mut checkpoints: Vec<String> = Vec::new();
        let mut idx = 0usize;

        loop {
            // Stand-in for `search_page`: record the token this "request" would
            // carry (None first, then each prior page's token), then take the
            // next canned page.
            tokens_seen_by_request.push(token.clone());
            let page: SearchPage = serde_json::from_value(pages[idx].clone()).unwrap();
            idx += 1;
            let done = page.done();
            all.extend(page.issues);
            if let Some(t) = page.next_page_token.as_deref().filter(|t| !t.is_empty()) {
                checkpoints.push(t.to_string());
            }
            token = page.next_page_token;
            if done {
                break;
            }
        }

        assert_eq!(
            tokens_seen_by_request,
            vec![None, Some("t1".to_string()), Some("t2".to_string())],
            "each request carried the prior page's token"
        );

        assert_eq!(all.len(), 5, "all issues across pages accumulated");
        assert_eq!(
            checkpoints,
            vec!["t1", "t2"],
            "checkpointed each non-final page token"
        );
        assert!(token.is_none(), "final page cleared the token");
        assert_eq!(idx, 3, "stopped exactly at the isLast page");
    }
}
