//! Fetch a Linear team over GraphQL and produce an import-wizard ZIP.
//!
//! Strategy (proven by `tests/fixtures/linear/fetch_prototype.py`): beat
//! Linear's 10k-points-per-query complexity ceiling and ~3M-points/hour budget
//! by **flattening** every entity into its own top-level, cursor-paginated
//! stream with lean fields (no deep nesting). Connections carry a back-reference
//! id (`comment.issue.id`, `attachment.issue.id`, `relation.issue.id`) so the
//! pure converter ([`crate::import::convert::linear`]) joins them client-side.
//!
//! On top of the prototype this adds two budget controls, both factored into
//! pure, unit-tested helpers:
//!   - [`next_page_size`] auto-tunes each stream's page size toward a target
//!     per-page complexity, ramping gently, clamped to a sane range.
//!   - [`backoff`] inspects the rate-limit response headers and returns a sleep
//!     duration when the remaining budget is low.
//!
//! No transport abstraction: reqwest is used directly. The crate's reqwest has
//! `default-features = false` (rustls only, no `json` feature), so request
//! bodies are serialized with `serde_json` and responses are read as text.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use crate::export::package::write_space_export_zip;
use crate::import::convert::linear::{
    self, LinearComment, LinearCycle, LinearDocument, LinearIssue, LinearLabelRef, LinearProject,
    LinearRelation, LinearTeam, LinearUser, LinearWorkflowState, LinearWorkspace,
};

/// Default Linear GraphQL endpoint.
pub const DEFAULT_BASE_URL: &str = "https://api.linear.app/graphql";

/// Tuning knobs for a Linear import run.
pub struct LinearImportOptions {
    /// Team key (e.g. `WMT`).
    pub team_key: String,
    /// GraphQL endpoint; defaults to [`DEFAULT_BASE_URL`].
    pub base_url: String,
    /// First page size for every stream before auto-tune kicks in.
    pub seed_page_size: usize,
    /// Target per-page complexity the auto-tuner aims for (Linear caps a single
    /// query at ~10k points; ~8000 leaves headroom).
    pub complexity_target: f64,
    /// Hard upper bound on page size (Linear connections cap at 250).
    pub max_page_size: usize,
}

impl Default for LinearImportOptions {
    fn default() -> Self {
        Self {
            team_key: String::new(),
            base_url: DEFAULT_BASE_URL.to_string(),
            seed_page_size: 10,
            complexity_target: 8000.0,
            max_page_size: 250,
        }
    }
}

/// Result of a successful import run.
pub struct LinearImportOutput {
    /// The import-wizard ZIP (`data.json` + `attachments/…`).
    pub zip: Vec<u8>,
    /// Non-fatal fidelity/transfer warnings (converter + attachment downloads).
    pub warnings: Vec<String>,
    /// Human-readable per-stream counts + attachments-downloaded summary.
    pub summary: String,
}

// =============================================================================
// Pure helpers (unit-tested below)
// =============================================================================

/// Choose the next page size from the last page's observed cost.
///
/// `used` is the page size that produced `x_complexity` (the `x-complexity`
/// response header, i.e. the actual cost of that query). We estimate
/// cost-per-item and pick the page size whose projected cost hits `target`,
/// then clamp to `5 ..= max` and ramp gently (never more than ~2× the page we
/// just ran) so a cheap first page doesn't immediately jump to a giant,
/// possibly-over-budget page. Never returns 0.
pub fn next_page_size(used: usize, x_complexity: f64, target: f64, max: usize) -> usize {
    const MIN_PAGE: usize = 5;
    let max = max.max(MIN_PAGE);
    let used = used.max(1);

    // Without a usable cost signal, hold steady (clamped).
    if !(x_complexity.is_finite()) || x_complexity <= 0.0 {
        return used.clamp(MIN_PAGE, max);
    }

    let cost_per_item = x_complexity / used as f64;
    if cost_per_item <= 0.0 {
        return max;
    }
    let ideal = (target / cost_per_item).floor() as usize;

    // Gentle ramp: at most double the page we just ran.
    let ramp_cap = used.saturating_mul(2);
    ideal.min(ramp_cap).clamp(MIN_PAGE, max)
}

/// How long to sleep before the next request given the rate-limit headers.
///
/// Returns `Some(duration)` when either the complexity budget or the request
/// budget is low — below ~5% of a nominal limit or under a small absolute floor
/// — sleeping until the reset (capped). Returns `None` when both budgets look
/// healthy. `now_ms` is injected so the decision is testable; the caller passes
/// the wall clock.
pub fn backoff(
    complexity_remaining: Option<i64>,
    requests_remaining: Option<i64>,
    reset_epoch_ms: Option<i64>,
    now_ms: i64,
) -> Option<Duration> {
    // Absolute floors below which we always pause, plus a relative floor of ~5%
    // of typical Linear limits (1500 req/hr, 250k complexity/hr observed).
    const COMPLEXITY_FLOOR: i64 = 15_000;
    const REQUESTS_FLOOR: i64 = 75;

    let low_complexity = complexity_remaining.is_some_and(|c| c < COMPLEXITY_FLOOR);
    let low_requests = requests_remaining.is_some_and(|r| r < REQUESTS_FLOOR);
    if !low_complexity && !low_requests {
        return None;
    }

    // Sleep until reset when we know it; otherwise a fixed cool-down. Cap so a
    // bogus far-future reset can't wedge the run.
    const MAX_SLEEP: Duration = Duration::from_secs(60);
    const DEFAULT_SLEEP: Duration = Duration::from_secs(30);
    match reset_epoch_ms {
        Some(reset) if reset > now_ms => {
            let ms = (reset - now_ms).min(MAX_SLEEP.as_millis() as i64).max(0);
            Some(Duration::from_millis(ms as u64))
        }
        _ => Some(DEFAULT_SLEEP),
    }
}

// =============================================================================
// Stream queries (lean; mirror the prototype field-for-field)
// =============================================================================

/// A flat, cursor-paginated stream: its name (= checkpoint key), the GraphQL
/// query, and the connection's root field name in the response.
struct Stream {
    name: &'static str,
    query: String,
    root: &'static str,
}

fn team_filter(team_key: &str) -> String {
    // Linear ids/keys are alphanumeric; the key is escaped defensively.
    format!("filter:{{team:{{key:{{eq:\"{}\"}}}}}}", escape(team_key))
}

fn issue_filter(team_key: &str) -> String {
    format!(
        "filter:{{issue:{{team:{{key:{{eq:\"{}\"}}}}}}}}",
        escape(team_key)
    )
}

/// Escape a string for embedding inside a GraphQL double-quoted literal.
fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn streams(team_key: &str) -> Vec<Stream> {
    let tf = team_filter(team_key);
    let isf = issue_filter(team_key);
    vec![
        Stream {
            name: "issues",
            root: "issues",
            query: format!(
                "query($first:Int!,$after:String){{ issues(first:$first, after:$after, includeArchived:true, {tf}){{ \
                 nodes{{ id identifier number title description priority estimate dueDate startedAt createdAt updatedAt completedAt canceledAt archivedAt \
                 state{{id}} assignee{{id}} creator{{id}} parent{{id}} cycle{{id}} project{{id}} projectMilestone{{id}} \
                 labels(first:50){{ nodes{{ id name color parent{{id name}} }} }} }} \
                 pageInfo{{ hasNextPage endCursor }} }} }}"
            ),
        },
        Stream {
            name: "comments",
            root: "comments",
            query: format!(
                "query($first:Int!,$after:String){{ comments(first:$first, after:$after, {isf}){{ \
                 nodes{{ id body url createdAt editedAt user{{id}} parent{{id}} issue{{id}} }} \
                 pageInfo{{ hasNextPage endCursor }} }} }}"
            ),
        },
        // AttachmentFilter has no issue/team field → pull all, client-filter.
        Stream {
            name: "attachments",
            root: "attachments",
            query: "query($first:Int!,$after:String){ attachments(first:$first, after:$after){ \
                 nodes{ id title subtitle url sourceType issue{id} } \
                 pageInfo{ hasNextPage endCursor } } }"
                .to_string(),
        },
        Stream {
            name: "issueLabels",
            root: "issueLabels",
            query: format!(
                "query($first:Int!,$after:String){{ issueLabels(first:$first, after:$after, {tf}){{ \
                 nodes{{ id name color isGroup parent{{id name}} }} \
                 pageInfo{{ hasNextPage endCursor }} }} }}"
            ),
        },
        Stream {
            name: "workflowStates",
            root: "workflowStates",
            query: format!(
                "query($first:Int!,$after:String){{ workflowStates(first:$first, after:$after, {tf}){{ \
                 nodes{{ id name type position color }} \
                 pageInfo{{ hasNextPage endCursor }} }} }}"
            ),
        },
        Stream {
            name: "cycles",
            root: "cycles",
            query: format!(
                "query($first:Int!,$after:String){{ cycles(first:$first, after:$after, {tf}){{ \
                 nodes{{ id number name startsAt endsAt completedAt }} \
                 pageInfo{{ hasNextPage endCursor }} }} }}"
            ),
        },
        // issueRelations has NO filter arg → pull all, client-filter.
        Stream {
            name: "issueRelations",
            root: "issueRelations",
            query: "query($first:Int!,$after:String){ issueRelations(first:$first, after:$after){ \
                 nodes{ id type issue{id} relatedIssue{id} } \
                 pageInfo{ hasNextPage endCursor } } }"
                .to_string(),
        },
        Stream {
            name: "projects",
            root: "projects",
            query: "query($first:Int!,$after:String){ projects(first:$first, after:$after){ \
                 nodes{ id name description state startDate targetDate \
                 projectMilestones(first:25){ nodes{ id name targetDate } } } \
                 pageInfo{ hasNextPage endCursor } } }"
                .to_string(),
        },
        Stream {
            name: "documents",
            root: "documents",
            query: "query($first:Int!,$after:String){ documents(first:$first, after:$after){ \
                 nodes{ id title content icon createdAt updatedAt creator{id} project{id} } \
                 pageInfo{ hasNextPage endCursor } } }"
                .to_string(),
        },
        Stream {
            name: "users",
            root: "users",
            query: "query($first:Int!,$after:String){ users(first:$first, after:$after){ \
                 nodes{ id name email active admin } \
                 pageInfo{ hasNextPage endCursor } } }"
                .to_string(),
        },
    ]
}

// =============================================================================
// HTTP plumbing
// =============================================================================

/// One GraphQL response page: the raw `data` object, the cost header, and the
/// rate-limit headers we throttle on.
struct GqlPage {
    data: Value,
    x_complexity: Option<f64>,
    complexity_remaining: Option<i64>,
    requests_remaining: Option<i64>,
    reset_epoch_ms: Option<i64>,
}

/// POST a GraphQL query, honoring HTTP 429 `Retry-After` with a bounded retry
/// loop. Returns the parsed `data` plus the rate-limit headers.
async fn gql(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    query: &str,
    first: usize,
    after: Option<&str>,
) -> Result<GqlPage, String> {
    let body = serde_json::json!({
        "query": query,
        "variables": { "first": first, "after": after },
    });
    let body = serde_json::to_vec(&body).map_err(|e| format!("encode request: {e}"))?;

    for attempt in 0..6u32 {
        let resp = client
            .post(base_url)
            // Per the prototype: bare key, NO "Bearer" prefix.
            .header("Authorization", api_key)
            .header("Content-Type", "application/json")
            .body(body.clone())
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;

        let status = resp.status();
        if status.as_u16() == 429 {
            let wait = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.trim().parse::<u64>().ok())
                .unwrap_or(15);
            tracing::warn!("Linear 429 (attempt {attempt}); sleeping {wait}s");
            tokio::time::sleep(Duration::from_secs(wait)).await;
            continue;
        }

        let headers = parse_rate_headers(resp.headers());
        let text = resp.text().await.map_err(|e| format!("read body: {e}"))?;
        if !status.is_success() {
            return Err(format!(
                "HTTP {}: {}",
                status.as_u16(),
                truncate(&text, 500)
            ));
        }

        let mut json: Value =
            serde_json::from_str(&text).map_err(|e| format!("parse response JSON: {e}"))?;
        if let Some(errors) = json.get("errors") {
            if !errors.is_null() {
                return Err(format!(
                    "GraphQL errors: {}",
                    truncate(&errors.to_string(), 800)
                ));
            }
        }
        let data = json
            .get_mut("data")
            .map(Value::take)
            .ok_or_else(|| "response had no `data`".to_string())?;

        return Ok(GqlPage {
            data,
            x_complexity: headers.0,
            complexity_remaining: headers.1,
            requests_remaining: headers.2,
            reset_epoch_ms: headers.3,
        });
    }
    Err("too many 429 retries".to_string())
}

/// Pull `(x-complexity, complexity-remaining, requests-remaining, reset-epoch-ms)`
/// from the response headers. Linear's exact header names aren't contractually
/// stable, so the rate-limit names are matched generically (the prototype does
/// the same).
fn parse_rate_headers(
    headers: &reqwest::header::HeaderMap,
) -> (Option<f64>, Option<i64>, Option<i64>, Option<i64>) {
    let get_str = |name: &str| -> Option<&str> {
        headers
            .iter()
            .find(|(k, _)| k.as_str().eq_ignore_ascii_case(name))
            .and_then(|(_, v)| v.to_str().ok())
    };

    let x_complexity = get_str("x-complexity").and_then(|s| s.trim().parse::<f64>().ok());

    // Remaining: take the minimum of any "*ratelimit*remaining*" header so we
    // throttle on whichever budget (requests vs complexity) is tightest. We
    // separate complexity vs request budgets by the header name where possible.
    let mut complexity_remaining: Option<i64> = None;
    let mut requests_remaining: Option<i64> = None;
    let mut reset_epoch_ms: Option<i64> = None;
    for (k, v) in headers.iter() {
        let name = k.as_str().to_ascii_lowercase();
        let Ok(val) = v.to_str() else { continue };
        let val = val.trim();
        if name.contains("ratelimit") && name.contains("remaining") {
            if let Ok(n) = val.parse::<i64>() {
                if name.contains("complex") {
                    complexity_remaining = Some(complexity_remaining.map_or(n, |c| c.min(n)));
                } else {
                    requests_remaining = Some(requests_remaining.map_or(n, |r| r.min(n)));
                }
            }
        }
        if name.contains("ratelimit") && name.contains("reset") {
            // Linear emits epoch seconds; normalize to ms (heuristic: < 1e12 ⇒ s).
            if let Ok(n) = val.parse::<i64>() {
                let ms = if n < 1_000_000_000_000 { n * 1000 } else { n };
                reset_epoch_ms = Some(reset_epoch_ms.map_or(ms, |r| r.min(ms)));
            }
        }
    }
    (
        x_complexity,
        complexity_remaining,
        requests_remaining,
        reset_epoch_ms,
    )
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}

/// One paginated connection page extracted from a GraphQL `data` object.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Connection {
    #[serde(default)]
    nodes: Vec<Value>,
    // Must map to GraphQL `pageInfo`; without the rename it silently defaults to
    // `has_next_page: false` and every stream stops after its first page.
    #[serde(default)]
    page_info: PageInfo,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    #[serde(default)]
    has_next_page: bool,
    #[serde(default)]
    end_cursor: Option<String>,
}

// =============================================================================
// Orchestration
// =============================================================================

/// Fetch a Linear team and build an import-wizard ZIP.
///
/// `api_key` is the Linear personal API key (sent bare, no "Bearer").
/// `resume` maps `stream name → last end cursor` to continue an interrupted
/// run; `checkpoint(stream, cursor)` is invoked after every page so the caller
/// can persist progress.
pub async fn run_linear_import(
    api_key: &str,
    opts: &LinearImportOptions,
    resume: &HashMap<String, String>,
    mut checkpoint: impl FnMut(&str, &str),
) -> Result<LinearImportOutput, String> {
    let base_url = if opts.base_url.is_empty() {
        DEFAULT_BASE_URL
    } else {
        opts.base_url.as_str()
    };
    let client = reqwest::Client::new();

    // ── Team meta (single object, not paginated) ──────────────────────────────
    let team = fetch_team(&client, base_url, api_key, &opts.team_key).await?;

    // ── Each flat stream, paginated + resumable + auto-tuned ──────────────────
    let mut raw: HashMap<&'static str, Vec<Value>> = HashMap::new();
    for stream in streams(&opts.team_key) {
        let mut nodes: Vec<Value> = Vec::new();
        let mut cursor: Option<String> = resume.get(stream.name).cloned();
        let mut page_size = opts.seed_page_size.clamp(5, opts.max_page_size);

        loop {
            // Throttle ahead of the call based on the last page's headers.
            // (The first call has no headers yet ⇒ no sleep.)
            let page = gql(
                &client,
                base_url,
                api_key,
                &stream.query,
                page_size,
                cursor.as_deref(),
            )
            .await
            .map_err(|e| format!("[{}] {e}", stream.name))?;

            let conn: Connection =
                serde_json::from_value(page.data.get(stream.root).cloned().ok_or_else(|| {
                    format!("[{}] response missing `{}`", stream.name, stream.root)
                })?)
                .map_err(|e| format!("[{}] decode connection: {e}", stream.name))?;

            let got = conn.nodes.len();
            nodes.extend(conn.nodes);
            cursor = conn.page_info.end_cursor.clone();

            if let Some(c) = &cursor {
                checkpoint(stream.name, c);
            }

            tracing::debug!(
                "[{}] +{got} (total {}) next={} page_size={page_size} x_complexity={:?}",
                stream.name,
                nodes.len(),
                conn.page_info.has_next_page,
                page.x_complexity,
            );

            if !conn.page_info.has_next_page {
                break;
            }

            // Auto-tune the next page size from this page's cost.
            if let Some(xc) = page.x_complexity {
                page_size =
                    next_page_size(page_size, xc, opts.complexity_target, opts.max_page_size);
            }

            // Throttle between pages when the budget is low.
            let now_ms = chrono::Utc::now().timestamp_millis();
            if let Some(sleep) = backoff(
                page.complexity_remaining,
                page.requests_remaining,
                page.reset_epoch_ms,
                now_ms,
            ) {
                tracing::warn!("[{}] low budget; sleeping {:?}", stream.name, sleep);
                tokio::time::sleep(sleep).await;
            }
        }

        raw.insert(stream.name, nodes);
    }

    // ── Client-side join: relations/attachments have no team filter ───────────
    let issue_ids: HashSet<String> = raw
        .get("issues")
        .into_iter()
        .flatten()
        .filter_map(|v| v.get("id").and_then(Value::as_str).map(str::to_string))
        .collect();

    let attachments_all = raw.remove("attachments").unwrap_or_default();
    let attachments_in: Vec<Value> = attachments_all
        .into_iter()
        .filter(|a| ref_id(a, "issue").is_some_and(|id| issue_ids.contains(&id)))
        .collect();

    let relations_all = raw.remove("issueRelations").unwrap_or_default();
    let relations_in: Vec<Value> = relations_all
        .into_iter()
        .filter(|r| {
            ref_id(r, "issue").is_some_and(|id| issue_ids.contains(&id))
                || ref_id(r, "relatedIssue").is_some_and(|id| issue_ids.contains(&id))
        })
        .collect();

    // ── Deserialize the converter input structs from the raw streams ──────────
    let issues: Vec<LinearIssue> = decode_nodes(raw.remove("issues"), "issues")?;
    let comments: Vec<LinearComment> = decode_nodes(raw.remove("comments"), "comments")?;
    let mut issue_labels: Vec<LinearLabelRef> =
        decode_nodes(raw.remove("issueLabels"), "issueLabels")?;
    let workflow_states: Vec<LinearWorkflowState> =
        decode_nodes(raw.remove("workflowStates"), "workflowStates")?;
    let cycles: Vec<LinearCycle> = decode_nodes(raw.remove("cycles"), "cycles")?;
    let projects: Vec<LinearProject> = decode_nodes(raw.remove("projects"), "projects")?;
    let documents: Vec<LinearDocument> = decode_nodes(raw.remove("documents"), "documents")?;
    let users: Vec<LinearUser> = decode_nodes(raw.remove("users"), "users")?;
    let issue_relations: Vec<LinearRelation> = decode_values(relations_in, "issueRelations")?;

    // ── Fill in label defs referenced by issues but absent from the stream ────
    fetch_missing_labels(&client, base_url, api_key, &issues, &mut issue_labels).await?;

    let ws = LinearWorkspace {
        team,
        issues,
        comments,
        issue_labels,
        workflow_states,
        cycles,
        projects,
        documents,
        users,
        issue_relations,
    };

    // Counts for the summary (issues etc. are now owned by `ws`).
    let counts = StreamCounts {
        issues: ws.issues.len(),
        comments: ws.comments.len(),
        attachments_scanned: attachments_in.len(),
        issue_labels: ws.issue_labels.len(),
        workflow_states: ws.workflow_states.len(),
        cycles: ws.cycles.len(),
        projects: ws.projects.len(),
        documents: ws.documents.len(),
        users: ws.users.len(),
        issue_relations: ws.issue_relations.len(),
    };

    // ── Convert → export, then download attachment bytes ──────────────────────
    let conversion = linear::convert(&ws);
    let mut warnings = conversion.warnings;
    let mut export = conversion.export;

    // Filename per minted attachment id from the export manifest. Owned so it
    // doesn't borrow `export` (which we mutate below to backfill sizes).
    let filename_by_att: HashMap<String, String> = export
        .attachments
        .iter()
        .map(|a| (a.id.clone(), a.filename.clone()))
        .collect();

    let mut blobs: Vec<(String, String, Vec<u8>)> = Vec::new();
    let mut downloaded = 0usize;
    for (att_id, url) in &conversion.attachment_urls {
        let Some(filename) = filename_by_att.get(att_id.as_str()) else {
            warnings.push(format!("attachment {att_id}: no manifest entry; skipped"));
            continue;
        };
        match download_attachment(&client, api_key, url).await {
            Ok(bytes) => {
                blobs.push((att_id.clone(), filename.clone(), bytes));
                downloaded += 1;
            }
            Err(e) => warnings.push(format!("attachment {att_id} ({url}): download failed: {e}")),
        }
    }

    // Backfill each attachment's real size from the downloaded bytes (the
    // converter leaves a `0` placeholder, which otherwise shows as "0 B" in the
    // import preview). Match by attachment id.
    let size_by_att: HashMap<&str, usize> = blobs
        .iter()
        .map(|(id, _, bytes)| (id.as_str(), bytes.len()))
        .collect();
    for att in &mut export.attachments {
        if let Some(&size) = size_by_att.get(att.id.as_str()) {
            att.size_bytes = size as i64;
        }
    }

    let zip = write_space_export_zip(&export, &blobs)?;
    let summary = counts.render(&export, downloaded);

    Ok(LinearImportOutput {
        zip,
        warnings,
        summary,
    })
}

struct StreamCounts {
    issues: usize,
    comments: usize,
    attachments_scanned: usize,
    issue_labels: usize,
    workflow_states: usize,
    cycles: usize,
    projects: usize,
    documents: usize,
    users: usize,
    issue_relations: usize,
}

impl StreamCounts {
    fn render(&self, export: &crate::export::types::SpaceExport, downloaded: usize) -> String {
        format!(
            "Linear import summary:\n\
             \x20 issues:          {}\n\
             \x20 comments:        {}\n\
             \x20 issueLabels:     {}\n\
             \x20 workflowStates:  {}\n\
             \x20 cycles:          {}\n\
             \x20 projects:        {}\n\
             \x20 documents:       {}\n\
             \x20 users:           {}\n\
             \x20 issueRelations:  {} (touching imported issues)\n\
             \x20 attachments:     {} referenced inline → {} downloaded ({} in manifest)\n\
             \x20 items in export: {}",
            self.issues,
            self.comments,
            self.issue_labels,
            self.workflow_states,
            self.cycles,
            self.projects,
            self.documents,
            self.users,
            self.issue_relations,
            self.attachments_scanned.max(export.attachments.len()),
            downloaded,
            export.attachments.len(),
            export.items.len(),
        )
    }
}

/// Read a `{ id }`-shaped sub-object's id (camelCase field name in the raw JSON).
fn ref_id(v: &Value, field: &str) -> Option<String> {
    v.get(field)
        .and_then(|r| r.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Decode an optional raw node list into the converter's struct.
fn decode_nodes<T: serde::de::DeserializeOwned>(
    nodes: Option<Vec<Value>>,
    name: &str,
) -> Result<Vec<T>, String> {
    decode_values(nodes.unwrap_or_default(), name)
}

fn decode_values<T: serde::de::DeserializeOwned>(
    values: Vec<Value>,
    name: &str,
) -> Result<Vec<T>, String> {
    values
        .into_iter()
        .map(|v| serde_json::from_value(v).map_err(|e| format!("[{name}] decode node: {e}")))
        .collect()
}

/// Fetch the team meta (single object) by key.
async fn fetch_team(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    team_key: &str,
) -> Result<LinearTeam, String> {
    // NB: the `teams` root query takes `key` directly (TeamFilter has no `team`
    // field) — unlike issues/labels/states which nest under `team`.
    let query = format!(
        "{{ teams(filter:{{key:{{eq:\"{}\"}}}}){{ nodes{{ id key name description issueEstimationType }} }} }}",
        escape(team_key)
    );
    let page = gql(client, base_url, api_key, &query, 1, None).await?;
    let node = page
        .data
        .get("teams")
        .and_then(|t| t.get("nodes"))
        .and_then(|n| n.as_array())
        .and_then(|arr| arr.first())
        .cloned()
        .ok_or_else(|| format!("team `{team_key}` not found"))?;
    serde_json::from_value(node).map_err(|e| format!("decode team: {e}"))
}

/// Fetch label definitions for any label id referenced by an issue but missing
/// from the `issueLabels` stream (group parents reachable only via issues, or
/// labels paginated past the stream window). Appends the recovered defs.
async fn fetch_missing_labels(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    issues: &[LinearIssue],
    issue_labels: &mut Vec<LinearLabelRef>,
) -> Result<(), String> {
    let mut known: HashSet<String> = issue_labels.iter().map(|l| l.id.clone()).collect();

    // Every label id referenced by issues: the label itself + its parent group.
    let mut wanted: HashSet<String> = HashSet::new();
    for issue in issues {
        for l in &issue.labels.nodes {
            wanted.insert(l.id.clone());
            if let Some(parent) = &l.parent {
                wanted.insert(parent.id.clone());
            }
        }
    }
    let missing: Vec<String> = wanted.difference(&known).cloned().collect();
    if missing.is_empty() {
        return Ok(());
    }

    let id_list = missing
        .iter()
        .map(|id| format!("\"{}\"", escape(id)))
        .collect::<Vec<_>>()
        .join(",");
    let query = format!(
        "query($first:Int!,$after:String){{ issueLabels(first:$first, after:$after, filter:{{id:{{in:[{id_list}]}}}}){{ \
         nodes{{ id name color isGroup parent{{id name}} }} \
         pageInfo{{ hasNextPage endCursor }} }} }}"
    );

    let mut cursor: Option<String> = None;
    loop {
        let page = gql(
            client,
            base_url,
            api_key,
            &query,
            missing.len().clamp(1, 250),
            cursor.as_deref(),
        )
        .await
        .map_err(|e| format!("[issueLabels backfill] {e}"))?;
        let conn: Connection = serde_json::from_value(
            page.data
                .get("issueLabels")
                .cloned()
                .ok_or_else(|| "[issueLabels backfill] missing `issueLabels`".to_string())?,
        )
        .map_err(|e| format!("[issueLabels backfill] decode: {e}"))?;

        for v in conn.nodes {
            if let Ok(l) = serde_json::from_value::<LinearLabelRef>(v) {
                if known.insert(l.id.clone()) {
                    issue_labels.push(l);
                }
            }
        }
        cursor = conn.page_info.end_cursor;
        if !conn.page_info.has_next_page {
            break;
        }
    }
    Ok(())
}

/// Download an attachment's bytes (same bare-key auth header).
async fn download_attachment(
    client: &reqwest::Client,
    api_key: &str,
    url: &str,
) -> Result<Vec<u8>, String> {
    let resp = client
        .get(url)
        .header("Authorization", api_key)
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
// Tests (pure helpers only; no network)
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_maps_page_info() {
        // Regression: `Connection` must rename `page_info`→`pageInfo`, else it
        // silently defaults to has_next_page=false and every stream stops after
        // its first page (truncating large workspaces to one page).
        let v = serde_json::json!({
            "nodes": [1, 2],
            "pageInfo": { "hasNextPage": true, "endCursor": "abc" }
        });
        let c: Connection = serde_json::from_value(v).unwrap();
        assert_eq!(c.nodes.len(), 2);
        assert!(c.page_info.has_next_page, "pageInfo must deserialize");
        assert_eq!(c.page_info.end_cursor.as_deref(), Some("abc"));
    }

    #[test]
    fn next_page_size_ramps_up_under_budget() {
        // Cheap page (10 items cost 1000 ⇒ 100/item; target 8000 ⇒ ideal 80),
        // but the gentle ramp caps growth at 2× the page we just ran.
        let n = next_page_size(10, 1000.0, 8000.0, 250);
        assert_eq!(n, 20, "gentle ramp caps at 2× the previous page");

        // Run it again from 20: now 2×=40, still under the ideal 80.
        assert_eq!(next_page_size(20, 2000.0, 8000.0, 250), 40);
    }

    #[test]
    fn next_page_size_clamps_at_max() {
        // Very cheap page would suggest a huge size; clamp to max (after ramp).
        // From a large page the 2× ramp would exceed max ⇒ clamps to max.
        assert_eq!(next_page_size(200, 100.0, 8000.0, 250), 250);
    }

    #[test]
    fn next_page_size_shrinks_when_too_costly() {
        // 50 items cost 16000 ⇒ 320/item; target 8000 ⇒ ideal 25 < 50 ⇒ shrink.
        let n = next_page_size(50, 16000.0, 8000.0, 250);
        assert_eq!(n, 25);
    }

    #[test]
    fn next_page_size_never_zero() {
        // Even a catastrophically expensive page floors at the minimum.
        assert!(next_page_size(10, 1_000_000.0, 8000.0, 250) >= 5);
        // Missing/garbage cost signal holds steady (clamped), never 0.
        assert!(next_page_size(10, 0.0, 8000.0, 250) >= 5);
        assert!(next_page_size(10, f64::NAN, 8000.0, 250) >= 5);
        assert!(next_page_size(0, 0.0, 8000.0, 250) >= 5);
    }

    #[test]
    fn backoff_sleeps_when_complexity_low() {
        // Below the complexity floor ⇒ sleep until reset (5s out).
        let now = 1_000_000;
        let d = backoff(Some(1_000), Some(1_000), Some(now + 5_000), now);
        assert_eq!(d, Some(Duration::from_millis(5_000)));
    }

    #[test]
    fn backoff_sleeps_when_requests_low() {
        let now = 1_000_000;
        // Healthy complexity, but requests below floor ⇒ still sleep.
        let d = backoff(Some(200_000), Some(10), None, now);
        assert_eq!(d, Some(Duration::from_secs(30)));
    }

    #[test]
    fn backoff_none_when_healthy() {
        assert_eq!(backoff(Some(200_000), Some(1_400), Some(0), 0), None);
        // Unknown budgets ⇒ no basis to throttle.
        assert_eq!(backoff(None, None, None, 0), None);
    }

    #[test]
    fn backoff_caps_far_future_reset() {
        let now = 0;
        // Reset is an hour out; cap the sleep at 60s.
        let d = backoff(Some(0), None, Some(3_600_000), now);
        assert_eq!(d, Some(Duration::from_secs(60)));
    }

    /// Offline convert→zip over the local golden sample. No network: loads the
    /// `tests/fixtures/linear/*.json` streams, converts, and packages a ZIP that
    /// imports cleanly. No-ops in CI when the fixtures are absent.
    #[test]
    fn golden_sample_convert_to_importable_zip() {
        use crate::export::package::write_space_export_zip;
        use crate::import::{build_space_from_export, ImportMaps};
        use std::io::Read;

        let crate_dir = env!("CARGO_MANIFEST_DIR");
        let fixtures = std::path::Path::new(crate_dir).join("../../../tests/fixtures/linear");
        if !fixtures.join("issues.json").exists() {
            eprintln!("golden sample absent ({fixtures:?}); skipping");
            return;
        }

        let ws = load_golden(&fixtures);
        let conversion = linear::convert(&ws);
        // No bytes are fetched offline; the inline-image attachments package
        // empty-blob entries, which still proves the ZIP layout + manifest.
        let blobs: Vec<(String, String, Vec<u8>)> = conversion
            .export
            .attachments
            .iter()
            .map(|a| (a.id.clone(), a.filename.clone(), Vec::new()))
            .collect();
        let zip = write_space_export_zip(&conversion.export, &blobs).unwrap();
        assert!(zip.starts_with(b"PK"));

        // The ZIP's data.json must round-trip and import without panicking.
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip)).unwrap();
        let mut json = Vec::new();
        archive
            .by_name("data.json")
            .unwrap()
            .read_to_end(&mut json)
            .unwrap();
        let parsed: crate::export::types::SpaceExport = serde_json::from_slice(&json).unwrap();
        let result = build_space_from_export(
            &parsed,
            "bbbbbbbb-0000-0000-0000-000000000002",
            "cccccccc-0000-0000-0000-000000000003",
            &ImportMaps::default(),
        );
        eprintln!(
            "golden zip: {} items, {} attachments, max_short_id={}, warnings={}",
            parsed.items.len(),
            parsed.attachments.len(),
            result.max_short_id,
            conversion.warnings.len(),
        );
        assert!(parsed.items.len() >= 18, "golden sample yields items");
    }

    /// Load the golden sample streams from `tests/fixtures/linear/*.json`.
    fn load_golden(dir: &std::path::Path) -> LinearWorkspace {
        fn nodes<T: serde::de::DeserializeOwned>(dir: &std::path::Path, file: &str) -> Vec<T> {
            let path = dir.join(file);
            let Ok(text) = std::fs::read_to_string(&path) else {
                return Vec::new();
            };
            #[derive(Deserialize)]
            struct Stream<T> {
                #[serde(default = "Vec::new")]
                nodes: Vec<T>,
            }
            serde_json::from_str::<Stream<T>>(&text)
                .map(|s| s.nodes)
                .unwrap_or_default()
        }

        let team: LinearTeam = std::fs::read_to_string(dir.join("team.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();

        LinearWorkspace {
            team,
            issues: nodes(dir, "issues.json"),
            comments: nodes(dir, "comments.json"),
            issue_labels: nodes(dir, "issueLabels.json"),
            workflow_states: nodes(dir, "workflowStates.json"),
            cycles: nodes(dir, "cycles.json"),
            projects: nodes(dir, "projects.json"),
            documents: nodes(dir, "documents.json"),
            users: nodes(dir, "users.json"),
            issue_relations: nodes(dir, "issueRelations.json"),
        }
    }
}
