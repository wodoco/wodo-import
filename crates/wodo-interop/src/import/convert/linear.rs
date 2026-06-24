//! Linear → `SpaceExport` v2 converter.
//!
//! Pure, DB-free. Maps a flattened snapshot of a Linear team's GraphQL data
//! (issues, comments, labels, workflow states, cycles, projects, documents,
//! users, relations) onto Wodo's [`SpaceExport`]. The output is the exact shape
//! the import pipeline ([`crate::import::build_space_from_export`]) expects;
//! feed it straight in to materialize a new space.
//!
//! UUIDs are derived deterministically via [`Uuid::new_v5`] from each Linear id
//! (against a fixed namespace), so re-runs are stable and cross-references —
//! parent issues, blockers, comment authors, labels — resolve to the same
//! Wodo id every time.
//!
//! Inline `uploads.linear.app` images become [`ExportAttachment`] metadata and
//! the markdown URL is rewritten to the Wodo attachment route BEFORE the
//! markdown is encoded to Yjs. The actual image bytes are NOT fetched here; the
//! converter returns an `attachment id → original Linear URL` map for a later
//! download phase.

use std::collections::{HashMap, HashSet};

use base64::Engine as _;
use serde::Deserialize;
use uuid::Uuid;
use yrs::{Doc, ReadTxn, StateVector, Transact, WriteTxn};

use crate::export::types::{
    ExportAttachment, ExportComment, ExportCycle, ExportDocument, ExportItem, ExportMilestoneDef,
    ExportMilestones, ExportSpaceMetadata, ExportUser, SpaceExport,
};
use crate::markdown::write_markdown_to_fragment;
use crate::template::types::{
    TemplateLabelDef, TemplateLabelValue, TemplateLabels, TemplateViewDef, TemplateViews,
};

/// Fixed namespace for all v5 ids minted by this converter. A stable, arbitrary
/// constant so the derived ids are reproducible across builds.
const NS_LINEAR: Uuid = Uuid::from_u128(0x4c696e6561725f776f646f5f636e7674u128);

// =============================================================================
// Input model (serde structs for the flattened Linear streams)
// =============================================================================

/// A reference to another Linear entity by id (e.g. `state: { id }`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct LinearRef {
    pub id: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinearTeam {
    pub id: String,
    pub key: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub issue_estimation_type: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct LinearLabelRef {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub color: Option<String>,
    /// On an issue's label, the parent group (id + name); on the labels stream,
    /// only the id is reliably present.
    #[serde(default)]
    pub parent: Option<LinearLabelParent>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct LinearLabelParent {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct LinearLabelNodes {
    #[serde(default)]
    pub nodes: Vec<LinearLabelRef>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinearIssue {
    pub id: String,
    #[serde(default)]
    pub identifier: Option<String>,
    pub number: i64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    /// Linear priority: 0 = none, 1 = urgent … 4 = low.
    #[serde(default)]
    pub priority: i64,
    #[serde(default)]
    pub estimate: Option<f64>,
    #[serde(default)]
    pub due_date: Option<String>,
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub archived_at: Option<String>,
    #[serde(default)]
    pub state: Option<LinearRef>,
    #[serde(default)]
    pub assignee: Option<LinearRef>,
    #[serde(default)]
    pub creator: Option<LinearRef>,
    #[serde(default)]
    pub parent: Option<LinearRef>,
    #[serde(default)]
    pub cycle: Option<LinearRef>,
    #[serde(default)]
    pub project: Option<LinearRef>,
    #[serde(default)]
    pub project_milestone: Option<LinearRef>,
    #[serde(default)]
    pub labels: LinearLabelNodes,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinearComment {
    pub id: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub edited_at: Option<String>,
    #[serde(default)]
    pub user: Option<LinearRef>,
    #[serde(default)]
    pub parent: Option<LinearRef>,
    #[serde(default)]
    pub issue: Option<LinearRef>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinearWorkflowState {
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// One of: backlog, unstarted, started, completed, canceled, duplicate.
    #[serde(default, rename = "type")]
    pub state_type: String,
    #[serde(default)]
    pub position: f64,
    #[serde(default)]
    pub color: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinearCycle {
    pub id: String,
    #[serde(default)]
    pub number: Option<i64>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub starts_at: Option<String>,
    #[serde(default)]
    pub ends_at: Option<String>,
    #[serde(default)]
    pub completed_at: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinearProjectMilestone {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub target_date: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct LinearProjectMilestoneNodes {
    #[serde(default)]
    pub nodes: Vec<LinearProjectMilestone>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinearProject {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub project_milestones: LinearProjectMilestoneNodes,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinearDocument {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub creator: Option<LinearRef>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinearUser {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub admin: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinearRelation {
    pub id: String,
    /// One of: blocks, duplicate, related (Linear emits a few more, dropped).
    #[serde(default, rename = "type")]
    pub relation_type: String,
    #[serde(default)]
    pub issue: Option<LinearRef>,
    #[serde(default)]
    pub related_issue: Option<LinearRef>,
}

/// A flattened snapshot of one Linear team's data — the converter's input.
#[derive(Debug, Clone, Default)]
pub struct LinearWorkspace {
    pub team: LinearTeam,
    pub issues: Vec<LinearIssue>,
    pub comments: Vec<LinearComment>,
    pub issue_labels: Vec<LinearLabelRef>,
    pub workflow_states: Vec<LinearWorkflowState>,
    pub cycles: Vec<LinearCycle>,
    pub projects: Vec<LinearProject>,
    pub documents: Vec<LinearDocument>,
    pub users: Vec<LinearUser>,
    pub issue_relations: Vec<LinearRelation>,
}

/// Output of the pure converter: the export plus, separately, the inline-image
/// attachments still needing a byte download (keyed by minted attachment id).
#[derive(Debug, Clone)]
pub struct LinearConversion {
    pub export: SpaceExport,
    /// Human-readable fidelity warnings (dropped relations, label collisions…).
    pub warnings: Vec<String>,
    /// `attachment id → original https://uploads.linear.app/... URL`. The
    /// converter does not fetch bytes; a later phase downloads each URL and
    /// stores it under the (already-rewritten) attachment route.
    pub attachment_urls: HashMap<String, String>,
}

// =============================================================================
// Deterministic ids
// =============================================================================

/// Stable v5 UUID string from any Linear id, scoped by a kind prefix so
/// different entity kinds that happen to share a source id never collide.
fn det_id(kind: &str, linear_id: &str) -> String {
    Uuid::new_v5(&NS_LINEAR, format!("{kind}:{linear_id}").as_bytes()).to_string()
}

// =============================================================================
// Markdown → description_yjs (no shared helper exists; mirror make_desc_blob
// but use the production markdown parser for full fidelity)
// =============================================================================

/// Encode markdown as the base64 Yjs rich-text blob the import builder expects
/// for `*_yjs` fields: a Yjs update whose root `XmlFragment` named `"content"`
/// holds the parsed Tiptap tree. Returns `None` for empty/whitespace input
/// (the serializer skips empty fragments).
fn markdown_to_content_yjs(markdown: &str) -> Option<String> {
    if markdown.trim().is_empty() {
        return None;
    }
    let doc = Doc::new();
    {
        let mut txn = doc.transact_mut();
        let frag = txn.get_or_insert_xml_fragment("content");
        write_markdown_to_fragment(&mut txn, &frag, markdown, None);
    }
    let bytes = {
        let txn = doc.transact();
        txn.encode_state_as_update_v1(&StateVector::default())
    };
    Some(base64::engine::general_purpose::STANDARD.encode(&bytes))
}

// =============================================================================
// Inline image extraction + URL rewrite
// =============================================================================

/// Scan markdown for `https://uploads.linear.app/...` URLs (in either
/// `![alt](url)` or `[text](url)` form, or bare). For each, mint an attachment,
/// record its original URL, and rewrite the URL in-place to the Wodo route.
/// Returns the rewritten markdown.
fn rewrite_inline_images(
    markdown: &str,
    space_id: &str,
    attachments: &mut Vec<ExportAttachment>,
    attachment_urls: &mut HashMap<String, String>,
    uploaded_by: &str,
    uploaded_at: &str,
) -> String {
    let mut out = String::with_capacity(markdown.len());
    let mut rest = markdown;
    const NEEDLE: &str = "https://uploads.linear.app/";

    while let Some(pos) = rest.find(NEEDLE) {
        out.push_str(&rest[..pos]);
        let after = &rest[pos..];
        // The URL runs until the first character that can't be part of a URL in
        // markdown link/image syntax: whitespace, ), ], or quotes.
        let url_end = after
            .find(|c: char| c.is_whitespace() || matches!(c, ')' | ']' | '"' | '\'' | '<' | '>'))
            .unwrap_or(after.len());
        let url = &after[..url_end];

        // A nearby `![alt](` or `[text](` immediately before the URL gives a
        // filename hint — essential when the URL ends in a bare UUID.
        let label = preceding_label(&out);
        let filename = filename_for_url(url, label.as_deref());

        let att_id = det_id("attachment", url);
        let content_type = guess_content_type(&filename);
        // De-dup: the same URL may appear in several places.
        if !attachment_urls.contains_key(&att_id) {
            attachments.push(ExportAttachment {
                id: att_id.clone(),
                filename,
                content_type,
                size_bytes: 0,
                uploaded_by: uploaded_by.to_string(),
                uploaded_at: uploaded_at.to_string(),
                document_id: None,
                orphaned: false,
            });
            attachment_urls.insert(att_id.clone(), url.to_string());
        }

        out.push_str(&format!("/api/spaces/{space_id}/attachments/{att_id}"));
        rest = &after[url_end..];
    }
    out.push_str(rest);
    out
}

/// If the text just emitted ends with a markdown link/image opener — either an
/// image `![alt](` or a plain link `[text](` — recover the label (alt or link
/// text) for a filename hint. Linear writes file links as `[name.ext](url)`
/// just as often as images, and the label carries the only real filename when
/// the URL ends in a bare UUID.
fn preceding_label(emitted: &str) -> Option<String> {
    // Look at the tail: ...[LABEL]( or ...![LABEL](
    let trimmed = emitted.strip_suffix('(')?;
    let bracket = trimmed.rfind('[')?;
    // Reject `[[...](` (item-ref-ish) — a `[` right before our `[` is not a
    // simple link/image label.
    if bracket > 0 && trimmed.as_bytes()[bracket - 1] == b'[' {
        return None;
    }
    let inner = &trimmed[bracket + 1..];
    if let Some(stripped) = inner.strip_suffix(']') {
        let label = stripped.trim();
        if label.is_empty() {
            None
        } else {
            Some(label.to_string())
        }
    } else {
        None
    }
}

/// Whether a candidate filename has a usable file extension (a short, alphanumeric
/// suffix after a `.`). Bare UUIDs and extension-less names return false.
fn has_usable_extension(name: &str) -> bool {
    match name.rsplit_once('.') {
        Some((stem, ext)) => {
            !stem.is_empty()
                && !ext.is_empty()
                && ext.len() <= 5
                && ext.chars().all(|c| c.is_ascii_alphanumeric())
        }
        None => false,
    }
}

/// Filename for an attachment: prefer the URL's last path segment when it has a
/// usable file extension; otherwise fall back to the link/alt label (which
/// Linear file links carry, e.g. `[kom.m4v](…/<uuid>)`); else the bare segment
/// or a generic name. The chosen filename also drives the MIME guess.
fn filename_for_url(url: &str, label: Option<&str>) -> String {
    let last_seg = url.rsplit('/').next().unwrap_or("");
    if has_usable_extension(last_seg) {
        return last_seg.to_string();
    }
    // URL has no usable extension — the label is the better filename source.
    if let Some(label) = label {
        let label = label.trim();
        if !label.is_empty() {
            return label.to_string();
        }
    }
    if last_seg.is_empty() {
        "attachment".to_string()
    } else {
        last_seg.to_string()
    }
}

/// Guess a MIME type from a filename extension; default to octet-stream.
fn guess_content_type(filename: &str) -> String {
    let ext = filename
        .rsplit('.')
        .next()
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "bmp" => "image/bmp",
        "tiff" | "tif" => "image/tiff",
        "mp4" => "video/mp4",
        "mov" | "m4v" => "video/quicktime",
        "webm" => "video/webm",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
    .to_string()
}

// =============================================================================
// Entry point
// =============================================================================

/// Convert a flattened Linear workspace into a [`SpaceExport`] plus warnings.
///
/// Thin wrapper preserving the spec's signature; see [`convert`] for the full
/// result (including the inline-image URL map).
pub fn linear_to_space_export(src: &LinearWorkspace) -> (SpaceExport, Vec<String>) {
    let c = convert(src);
    (c.export, c.warnings)
}

/// Full conversion: the export, warnings, and the inline-image URL map.
pub fn convert(src: &LinearWorkspace) -> LinearConversion {
    let mut warnings: Vec<String> = Vec::new();
    let mut attachments: Vec<ExportAttachment> = Vec::new();
    let mut attachment_urls: HashMap<String, String> = HashMap::new();

    // ── Space metadata ───────────────────────────────────────────────────────
    let space_id = det_id("space", &src.team.id);
    let space_name = if src.team.name.is_empty() {
        src.team.key.clone()
    } else {
        src.team.name.clone()
    };
    let now = chrono::Utc::now().to_rfc3339();
    // First active user (or any user) seeds attachment provenance — exports need
    // an `uploaded_by`, and the converter has no real uploader per image.
    let provenance_user = src
        .users
        .iter()
        .find(|u| u.active)
        .or_else(|| src.users.first())
        .map(|u| det_id("user", &u.id))
        .unwrap_or_else(|| Uuid::nil().to_string());

    let space = ExportSpaceMetadata {
        id: space_id.clone(),
        name: space_name.clone(),
        slug: slugify(&space_name),
        region: "eu".to_string(),
        short_id_prefix: Some(src.team.key.clone()),
        short_id_visible: true,
        created_at: now.clone(),
    };

    // ── Users ────────────────────────────────────────────────────────────────
    let users: Vec<ExportUser> = src
        .users
        .iter()
        .map(|u| ExportUser {
            id: det_id("user", &u.id),
            display_name: if u.name.is_empty() {
                "Unknown user".to_string()
            } else {
                u.name.clone()
            },
            email: u.email.clone(),
        })
        .collect();
    let user_name_by_linear: HashMap<&str, &str> = src
        .users
        .iter()
        .map(|u| (u.id.as_str(), u.name.as_str()))
        .collect();

    // ── Labels ────────────────────────────────────────────────────────────────
    // Builds the TemplateLabels: Status, Priority, Estimate, per-group labels,
    // Tags (standalone), Project. Returns the lookups items use to set values.
    let label_build = build_labels(src, &mut warnings);

    // ── Milestones (Linear projectMilestones across all projects) ─────────────
    let mut ms_order: Vec<String> = Vec::new();
    let mut ms_defs: HashMap<String, ExportMilestoneDef> = HashMap::new();
    for project in &src.projects {
        for m in &project.project_milestones.nodes {
            let id = det_id("milestone", &m.id);
            if ms_defs.contains_key(&id) {
                continue;
            }
            ms_order.push(id.clone());
            ms_defs.insert(
                id.clone(),
                ExportMilestoneDef {
                    id,
                    name: m.name.clone(),
                    description: None,
                    deadline: m.target_date.clone(),
                    deprecated: false,
                },
            );
        }
    }

    // ── Cycles ────────────────────────────────────────────────────────────────
    let cycles: Vec<ExportCycle> = src
        .cycles
        .iter()
        .map(|cy| ExportCycle {
            id: det_id("cycle", &cy.id),
            name: cycle_name(cy),
            start_date: cy.starts_at.clone().unwrap_or_default(),
            end_date: cy.ends_at.clone().unwrap_or_default(),
            archived: false,
        })
        .collect();

    // ── Items ────────────────────────────────────────────────────────────────
    // The set of imported issue ids (for relation endpoint validation).
    let issue_id_set: HashSet<&str> = src.issues.iter().map(|i| i.id.as_str()).collect();

    let mut items: Vec<ExportItem> = Vec::with_capacity(src.issues.len());
    for issue in &src.issues {
        let item_id = det_id("item", &issue.id);
        let mut labels: HashMap<String, String> = HashMap::new();

        // Status
        if let Some(state) = &issue.state {
            if let Some(value_id) = label_build.status_value_by_state.get(state.id.as_str()) {
                labels.insert(label_build.status_label_id.clone(), value_id.clone());
            }
        }
        // Priority (0 = none → no value)
        if let Some(value_id) = label_build.priority_value_by_level.get(&issue.priority) {
            labels.insert(label_build.priority_label_id.clone(), value_id.clone());
        }
        // Estimate (present & non-zero)
        if let Some(est) = issue.estimate {
            if est != 0.0 {
                if let Some(value_id) = label_build.estimate_value_by_num.get(&(est as i64)) {
                    labels.insert(label_build.estimate_label_id.clone(), value_id.clone());
                }
            }
        }
        // Project
        if let Some(project) = &issue.project {
            if let Some(value_id) = label_build
                .project_value_by_project
                .get(project.id.as_str())
            {
                labels.insert(label_build.project_label_id.clone(), value_id.clone());
            }
        }
        // Linear labels: group children (exclusive) + standalone (Tags, first-wins)
        apply_issue_labels(issue, &label_build, &mut labels, &mut warnings);

        // Description: rewrite inline images, then md → yjs
        let (description_yjs, description_text) = match &issue.description {
            Some(desc) if !desc.trim().is_empty() => {
                let rewritten = rewrite_inline_images(
                    desc,
                    &space_id,
                    &mut attachments,
                    &mut attachment_urls,
                    &provenance_user,
                    &now,
                );
                (markdown_to_content_yjs(&rewritten), Some(rewritten))
            }
            _ => (None, None),
        };

        let assignee_user_ids = issue
            .assignee
            .as_ref()
            .map(|a| vec![det_id("user", &a.id)])
            .unwrap_or_default();

        items.push(ExportItem {
            id: item_id,
            short_id: Some(issue.number),
            title: issue.title.clone(),
            description_text,
            description_yjs,
            labels,
            assignee_user_ids,
            assignee_team_ids: Vec::new(),
            due_date: issue.due_date.clone(),
            start_date: issue.started_at.clone(),
            milestone_id: issue
                .project_milestone
                .as_ref()
                .map(|m| det_id("milestone", &m.id)),
            cycle_id: issue.cycle.as_ref().map(|c| det_id("cycle", &c.id)),
            parent_id: issue.parent.as_ref().map(|p| det_id("item", &p.id)),
            blocked_by: Vec::new(),
            duplicate_of: None,
            archived: issue.archived_at.is_some(),
            deep_archived: false,
            created_at: issue.created_at.clone(),
            created_by: issue.creator.as_ref().map(|c| det_id("user", &c.id)),
            updated_at: issue.updated_at.clone(),
            archived_at: issue.archived_at.clone(),
            completion_prompt: None,
            completion_note_text: None,
            completion_note_yjs: None,
            comments: Vec::new(),
        });
    }

    // ── Relations (apply onto items by Linear-issue id) ───────────────────────
    let mut item_idx_by_linear: HashMap<&str, usize> = HashMap::new();
    for (idx, issue) in src.issues.iter().enumerate() {
        item_idx_by_linear.insert(issue.id.as_str(), idx);
    }
    let mut dropped_relations = 0usize;
    for rel in &src.issue_relations {
        let (Some(a), Some(b)) = (&rel.issue, &rel.related_issue) else {
            dropped_relations += 1;
            continue;
        };
        if !issue_id_set.contains(a.id.as_str()) || !issue_id_set.contains(b.id.as_str()) {
            dropped_relations += 1;
            continue;
        }
        match rel.relation_type.as_str() {
            "blocks" => {
                // A blocks B ⇒ B.blocked_by += A
                if let Some(&bi) = item_idx_by_linear.get(b.id.as_str()) {
                    items[bi].blocked_by.push(det_id("item", &a.id));
                }
            }
            "duplicate" => {
                // A duplicate B ⇒ A.duplicate_of = B
                if let Some(&ai) = item_idx_by_linear.get(a.id.as_str()) {
                    items[ai].duplicate_of = Some(det_id("item", &b.id));
                }
            }
            // related → drop (no Wodo analog); any other type → drop
            _ => {}
        }
    }
    if dropped_relations > 0 {
        warnings.push(format!(
            "{dropped_relations} issue relation(s) referenced an issue outside the import and were dropped"
        ));
    }

    // ── Comments (nested under items, threaded) ───────────────────────────────
    for comment in &src.comments {
        let Some(issue_ref) = &comment.issue else {
            continue;
        };
        let Some(&item_idx) = item_idx_by_linear.get(issue_ref.id.as_str()) else {
            continue;
        };
        let author_linear_id = comment.user.as_ref().map(|u| u.id.as_str());
        let author_id = author_linear_id
            .map(|id| det_id("user", id))
            .unwrap_or_else(|| Uuid::nil().to_string());
        let author_name = author_linear_id
            .and_then(|id| user_name_by_linear.get(id).copied())
            .filter(|n| !n.is_empty())
            .map(|n| n.to_string());

        let rewritten = rewrite_inline_images(
            &comment.body,
            &space_id,
            &mut attachments,
            &mut attachment_urls,
            &provenance_user,
            &now,
        );
        items[item_idx].comments.push(ExportComment {
            id: det_id("comment", &comment.id),
            author_id,
            author_name,
            content_text: Some(rewritten.clone()),
            content_yjs: markdown_to_content_yjs(&rewritten),
            created_at: comment.created_at.clone(),
            edited_at: comment.edited_at.clone(),
            deleted: false,
            deleted_at: None,
            parent_id: comment.parent.as_ref().map(|p| det_id("comment", &p.id)),
        });
    }

    // ── Documents (Linear project documents) ──────────────────────────────────
    let documents: Vec<ExportDocument> = src
        .documents
        .iter()
        .map(|d| {
            let content = d.content.as_deref().unwrap_or("");
            let rewritten = rewrite_inline_images(
                content,
                &space_id,
                &mut attachments,
                &mut attachment_urls,
                &provenance_user,
                &now,
            );
            ExportDocument {
                id: det_id("document", &d.id),
                title: d.title.clone(),
                labels: HashMap::new(),
                content_text: if rewritten.trim().is_empty() {
                    None
                } else {
                    Some(rewritten.clone())
                },
                content_yjs: markdown_to_content_yjs(&rewritten),
                archived: false,
                owner_user_ids: Vec::new(),
                owner_team_ids: Vec::new(),
                parent_item_id: None,
                parent_milestone_id: None,
                review_cadence_days: None,
                last_reviewed_at: None,
                is_template: false,
                forked_from: None,
                created_at: d.created_at.clone(),
                created_by: d.creator.as_ref().map(|c| det_id("user", &c.id)),
                updated_at: d.updated_at.clone(),
                archived_at: None,
            }
        })
        .collect();

    // ── Views: always By Status; By Cycle when cycles exist ───────────────────
    let views = build_views(&label_build.status_label_id, !cycles.is_empty());

    let export = SpaceExport {
        format: "wodo-space-export-v2".to_string(),
        exported_at: now,
        space,
        labels: label_build.labels,
        milestones: ExportMilestones {
            order: ms_order,
            definitions: ms_defs,
        },
        cycle_config: None,
        cycles,
        archive_config: None,
        views,
        items,
        documents,
        attachments,
        users,
        teams: Vec::new(),
        overview_text: None,
        overview_yjs: None,
    };

    LinearConversion {
        export,
        warnings,
        attachment_urls,
    }
}

// =============================================================================
// Label building
// =============================================================================

/// Lookups produced while building the label schema, used to set item values.
struct LabelBuild {
    labels: TemplateLabels,
    status_label_id: String,
    status_value_by_state: HashMap<String, String>, // linear state id → value id
    priority_label_id: String,
    priority_value_by_level: HashMap<i64, String>, // linear priority 1..4 → value id
    estimate_label_id: String,
    estimate_value_by_num: HashMap<i64, String>, // estimate number → value id
    project_label_id: String,
    project_value_by_project: HashMap<String, String>, // linear project id → value id
    /// Linear group-label id → (wodo label id, child linear label id → value id)
    group_label_by_linear: HashMap<String, (String, HashMap<String, String>)>,
    tags_label_id: String,
    /// Linear standalone label id → Tags value id
    tags_value_by_linear: HashMap<String, String>,
}

fn build_labels(src: &LinearWorkspace, _warnings: &mut [String]) -> LabelBuild {
    let mut order: Vec<String> = Vec::new();
    let mut definitions: HashMap<String, TemplateLabelDef> = HashMap::new();

    // ── Status ────────────────────────────────────────────────────────────────
    let status_label_id = det_id("label", "status");
    let mut status_value_by_state: HashMap<String, String> = HashMap::new();
    {
        let mut states: Vec<&LinearWorkflowState> = src.workflow_states.iter().collect();
        states.sort_by(|a, b| {
            a.position
                .partial_cmp(&b.position)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut values_order = Vec::new();
        let mut values = HashMap::new();
        for st in states {
            let value_id = det_id("status_value", &st.id);
            let is_completion = matches!(
                st.state_type.as_str(),
                "completed" | "canceled" | "duplicate"
            );
            values_order.push(value_id.clone());
            values.insert(
                value_id.clone(),
                TemplateLabelValue {
                    id: value_id.clone(),
                    name: st.name.clone(),
                    color: st.color.clone().unwrap_or_else(|| "#6B7280".to_string()),
                    is_completion_state: is_completion,
                    ..Default::default()
                },
            );
            status_value_by_state.insert(st.id.clone(), value_id);
        }
        if !values_order.is_empty() {
            order.push(status_label_id.clone());
            definitions.insert(
                status_label_id.clone(),
                TemplateLabelDef {
                    id: status_label_id.clone(),
                    name: "Status".to_string(),
                    icon: "circle".to_string(),
                    values_order,
                    values,
                    ..Default::default()
                },
            );
        }
    }

    // ── Priority ────────────────────────────────────────────────────────────────
    let priority_label_id = det_id("label", "priority");
    let mut priority_value_by_level: HashMap<i64, String> = HashMap::new();
    {
        // Linear: 1 = Urgent, 2 = High, 3 = Medium, 4 = Low. Severity order.
        let levels = [(1, "Urgent"), (2, "High"), (3, "Medium"), (4, "Low")];
        let mut values_order = Vec::new();
        let mut values = HashMap::new();
        for (level, name) in levels {
            let value_id = det_id("priority_value", &level.to_string());
            values_order.push(value_id.clone());
            values.insert(
                value_id.clone(),
                TemplateLabelValue {
                    id: value_id.clone(),
                    name: name.to_string(),
                    color: priority_color(level),
                    ..Default::default()
                },
            );
            priority_value_by_level.insert(level, value_id);
        }
        order.push(priority_label_id.clone());
        definitions.insert(
            priority_label_id.clone(),
            TemplateLabelDef {
                id: priority_label_id.clone(),
                name: "Priority".to_string(),
                icon: "flag".to_string(),
                values_order,
                values,
                ..Default::default()
            },
        );
    }

    // ── Estimate ──────────────────────────────────────────────────────────────
    let estimate_label_id = det_id("label", "estimate");
    let mut estimate_value_by_num: HashMap<i64, String> = HashMap::new();
    {
        let t_shirt = src
            .team
            .issue_estimation_type
            .as_deref()
            .map(|t| t.eq_ignore_ascii_case("tShirt"))
            .unwrap_or(false);
        // Collect distinct estimate numbers actually used (non-zero).
        let mut nums: Vec<i64> = src
            .issues
            .iter()
            .filter_map(|i| i.estimate)
            .filter(|e| *e != 0.0)
            .map(|e| e as i64)
            .collect();
        nums.sort_unstable();
        nums.dedup();
        if !nums.is_empty() {
            let mut values_order = Vec::new();
            let mut values = HashMap::new();
            for n in nums {
                let value_id = det_id("estimate_value", &n.to_string());
                let name = if t_shirt {
                    tshirt_name(n)
                } else {
                    n.to_string()
                };
                values_order.push(value_id.clone());
                values.insert(
                    value_id.clone(),
                    TemplateLabelValue {
                        id: value_id.clone(),
                        name,
                        color: "#6B7280".to_string(),
                        ..Default::default()
                    },
                );
                estimate_value_by_num.insert(n, value_id);
            }
            order.push(estimate_label_id.clone());
            definitions.insert(
                estimate_label_id.clone(),
                TemplateLabelDef {
                    id: estimate_label_id.clone(),
                    name: "Estimate".to_string(),
                    icon: "gauge".to_string(),
                    values_order,
                    values,
                    ..Default::default()
                },
            );
        }
    }

    // ── Project ──────────────────────────────────────────────────────────────
    let project_label_id = det_id("label", "project");
    let mut project_value_by_project: HashMap<String, String> = HashMap::new();
    if !src.projects.is_empty() {
        let mut values_order = Vec::new();
        let mut values = HashMap::new();
        for project in &src.projects {
            let value_id = det_id("project_value", &project.id);
            values_order.push(value_id.clone());
            values.insert(
                value_id.clone(),
                TemplateLabelValue {
                    id: value_id.clone(),
                    name: project.name.clone(),
                    color: "#6B7280".to_string(),
                    ..Default::default()
                },
            );
            project_value_by_project.insert(project.id.clone(), value_id);
        }
        order.push(project_label_id.clone());
        definitions.insert(
            project_label_id.clone(),
            TemplateLabelDef {
                id: project_label_id.clone(),
                name: "Project".to_string(),
                icon: "folder".to_string(),
                values_order,
                values,
                ..Default::default()
            },
        );
    }

    // ── Linear labels: groups → own labels; standalone → Tags ──────────────────
    // Discover groups + their children from BOTH the issueLabels stream and the
    // labels embedded on issues (the embedded ones carry parent name reliably).
    let (group_meta, child_to_group, standalone) = discover_label_topology(src);

    // One Wodo label per group, child labels are its values (exclusive).
    let mut group_label_by_linear: HashMap<String, (String, HashMap<String, String>)> =
        HashMap::new();
    for (group_id, group_name) in &group_meta {
        let label_id = det_id("group_label", group_id);
        let mut values_order = Vec::new();
        let mut values = HashMap::new();
        let mut child_value_by_linear: HashMap<String, String> = HashMap::new();
        // Children of this group, deterministically ordered by linear id.
        let mut children: Vec<(&String, &ChildGroup)> = child_to_group
            .iter()
            .filter(|(_, g)| &g.group_id == group_id)
            .collect();
        children.sort_by(|a, b| a.0.cmp(b.0));
        for (child_id, meta) in children {
            let value_id = det_id("group_value", child_id);
            values_order.push(value_id.clone());
            values.insert(
                value_id.clone(),
                TemplateLabelValue {
                    id: value_id.clone(),
                    name: meta.name.clone(),
                    color: meta.color.clone().unwrap_or_else(|| "#6B7280".to_string()),
                    ..Default::default()
                },
            );
            child_value_by_linear.insert(child_id.clone(), value_id);
        }
        if !values_order.is_empty() {
            order.push(label_id.clone());
            definitions.insert(
                label_id.clone(),
                TemplateLabelDef {
                    id: label_id.clone(),
                    name: group_name.clone(),
                    icon: "tag".to_string(),
                    values_order,
                    values,
                    ..Default::default()
                },
            );
            group_label_by_linear.insert(group_id.clone(), (label_id, child_value_by_linear));
        }
    }

    // Standalone labels → single Tags label.
    let tags_label_id = det_id("label", "tags");
    let mut tags_value_by_linear: HashMap<String, String> = HashMap::new();
    if !standalone.is_empty() {
        let mut sorted: Vec<(&String, &LabelMeta)> = standalone.iter().collect();
        sorted.sort_by(|a, b| a.0.cmp(b.0));
        let mut values_order = Vec::new();
        let mut values = HashMap::new();
        for (lid, meta) in sorted {
            let value_id = det_id("tag_value", lid);
            values_order.push(value_id.clone());
            values.insert(
                value_id.clone(),
                TemplateLabelValue {
                    id: value_id.clone(),
                    name: meta.name.clone(),
                    color: meta.color.clone().unwrap_or_else(|| "#6B7280".to_string()),
                    ..Default::default()
                },
            );
            tags_value_by_linear.insert(lid.clone(), value_id);
        }
        order.push(tags_label_id.clone());
        definitions.insert(
            tags_label_id.clone(),
            TemplateLabelDef {
                id: tags_label_id.clone(),
                name: "Tags".to_string(),
                icon: "tag".to_string(),
                values_order,
                values,
                ..Default::default()
            },
        );
    }

    let primary_label_id = if definitions.contains_key(&status_label_id) {
        Some(status_label_id.clone())
    } else {
        order.first().cloned()
    };

    LabelBuild {
        labels: TemplateLabels {
            order,
            primary_label_id,
            definitions,
        },
        status_label_id,
        status_value_by_state,
        priority_label_id,
        priority_value_by_level,
        estimate_label_id,
        estimate_value_by_num,
        project_label_id,
        project_value_by_project,
        group_label_by_linear,
        tags_label_id,
        tags_value_by_linear,
    }
}

#[derive(Debug, Clone)]
struct LabelMeta {
    name: String,
    color: Option<String>,
}

#[derive(Debug, Clone)]
struct ChildGroup {
    group_id: String,
    name: String,
    color: Option<String>,
}

/// Inspect every label reference (stream + issue-embedded) and classify:
/// - groups: id → display name
/// - children: child id → its group (+ child name/color)
/// - standalone: id → name/color (no parent, not a group)
#[allow(clippy::type_complexity)]
fn discover_label_topology(
    src: &LinearWorkspace,
) -> (
    HashMap<String, String>,     // group id → group name
    HashMap<String, ChildGroup>, // child id → group ref
    HashMap<String, LabelMeta>,  // standalone id → meta
) {
    // Collect metadata for every label id we can see, and which ids are groups.
    let mut meta: HashMap<String, LabelMeta> = HashMap::new();
    let mut child_to_group: HashMap<String, ChildGroup> = HashMap::new();
    let mut group_names: HashMap<String, String> = HashMap::new();
    let mut is_group: HashSet<String> = HashSet::new();

    let mut record = |lid: &str, name: &str, color: &Option<String>| {
        let entry = meta.entry(lid.to_string()).or_insert_with(|| LabelMeta {
            name: name.to_string(),
            color: color.clone(),
        });
        if entry.name.is_empty() && !name.is_empty() {
            entry.name = name.to_string();
        }
        if entry.color.is_none() {
            entry.color = color.clone();
        }
    };

    let note_child = |l: &LinearLabelRef,
                      child_to_group: &mut HashMap<String, ChildGroup>,
                      is_group: &mut HashSet<String>,
                      group_names: &mut HashMap<String, String>| {
        let Some(parent) = &l.parent else {
            return;
        };
        is_group.insert(parent.id.clone());
        if let Some(pn) = &parent.name {
            if !pn.is_empty() {
                group_names
                    .entry(parent.id.clone())
                    .or_insert_with(|| pn.clone());
            }
        }
        let entry = child_to_group
            .entry(l.id.clone())
            .or_insert_with(|| ChildGroup {
                group_id: parent.id.clone(),
                name: l.name.clone(),
                color: l.color.clone(),
            });
        // Fill in name/color if a later, richer reference carries them.
        if entry.name.is_empty() && !l.name.is_empty() {
            entry.name = l.name.clone();
        }
        if entry.color.is_none() {
            entry.color = l.color.clone();
        }
    };

    // From the dedicated issueLabels stream (carries isGroup via parent shape):
    for l in &src.issue_labels {
        record(&l.id, &l.name, &l.color);
        note_child(l, &mut child_to_group, &mut is_group, &mut group_names);
    }

    // From labels embedded on issues (parent.name is present there):
    for issue in &src.issues {
        for l in &issue.labels.nodes {
            record(&l.id, &l.name, &l.color);
            note_child(l, &mut child_to_group, &mut is_group, &mut group_names);
        }
    }

    // Group display names: prefer a recorded parent name, fall back to meta or id.
    let mut groups: HashMap<String, String> = HashMap::new();
    for gid in &is_group {
        let name = group_names
            .get(gid)
            .cloned()
            .or_else(|| meta.get(gid).map(|m| m.name.clone()))
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| gid.clone());
        groups.insert(gid.clone(), name);
    }

    // Standalone = a label that is neither a group nor a child of one.
    let mut standalone: HashMap<String, LabelMeta> = HashMap::new();
    for (lid, m) in &meta {
        if is_group.contains(lid) || child_to_group.contains_key(lid) {
            continue;
        }
        standalone.insert(lid.clone(), m.clone());
    }

    (groups, child_to_group, standalone)
}

/// Apply an issue's Linear labels to the Wodo `labels` map: group children
/// (exclusive) and standalone labels (single Tags, first-wins with a warning).
fn apply_issue_labels(
    issue: &LinearIssue,
    lb: &LabelBuild,
    labels: &mut HashMap<String, String>,
    warnings: &mut Vec<String>,
) {
    let mut standalone_set = 0usize;
    let mut standalone_seen = false;
    for l in &issue.labels.nodes {
        // Group child?
        let mut handled = false;
        for (group_label_id, child_map) in lb.group_label_by_linear.values() {
            if let Some(value_id) = child_map.get(&l.id) {
                labels.insert(group_label_id.clone(), value_id.clone());
                handled = true;
                break;
            }
        }
        if handled {
            continue;
        }
        // Standalone → Tags, first-wins
        if let Some(value_id) = lb.tags_value_by_linear.get(&l.id) {
            standalone_set += 1;
            if !standalone_seen {
                labels.insert(lb.tags_label_id.clone(), value_id.clone());
                standalone_seen = true;
            }
        }
    }
    if standalone_set > 1 {
        let id = issue.identifier.clone().unwrap_or_else(|| issue.id.clone());
        warnings.push(format!(
            "issue {id} had {standalone_set} standalone labels; Tags keeps only the first ({} dropped)",
            standalone_set - 1
        ));
    }
}

// =============================================================================
// Views
// =============================================================================

fn build_views(status_label_id: &str, has_cycles: bool) -> TemplateViews {
    let mut order = Vec::new();
    let mut definitions = HashMap::new();

    let by_status_id = det_id("view", "by-status");
    order.push(by_status_id.clone());
    definitions.insert(
        by_status_id.clone(),
        TemplateViewDef {
            id: by_status_id.clone(),
            name: "By Status".to_string(),
            content_type: "items".to_string(),
            view_type: "board".to_string(),
            column_grouping: Some(status_label_id.to_string()),
            row_grouping: None,
            sort_order: "0".to_string(),
            show_archived: false,
            filters: None,
            zoom_level: None,
        },
    );

    if has_cycles {
        let by_cycle_id = det_id("view", "by-cycle");
        order.push(by_cycle_id.clone());
        definitions.insert(
            by_cycle_id.clone(),
            TemplateViewDef {
                id: by_cycle_id.clone(),
                name: "By Cycle".to_string(),
                content_type: "items".to_string(),
                view_type: "board".to_string(),
                column_grouping: Some("cycle".to_string()),
                row_grouping: None,
                sort_order: "1".to_string(),
                show_archived: false,
                filters: None,
                zoom_level: None,
            },
        );
    }

    TemplateViews { order, definitions }
}

// =============================================================================
// Small helpers
// =============================================================================

fn slugify(name: &str) -> String {
    let mut slug = String::with_capacity(name.len());
    let mut last_dash = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash && !slug.is_empty() {
            slug.push('-');
            last_dash = true;
        }
    }
    let trimmed = slug.trim_end_matches('-').to_string();
    if trimmed.is_empty() {
        "space".to_string()
    } else {
        trimmed
    }
}

fn cycle_name(cy: &LinearCycle) -> String {
    if let Some(name) = &cy.name {
        if !name.trim().is_empty() {
            return name.clone();
        }
    }
    match cy.number {
        Some(n) => format!("Cycle {n}"),
        None => "Cycle".to_string(),
    }
}

fn tshirt_name(n: i64) -> String {
    match n {
        1 => "XS".to_string(),
        2 => "S".to_string(),
        3 => "M".to_string(),
        5 => "L".to_string(),
        8 => "XL".to_string(),
        13 => "XXL".to_string(),
        21 => "XXXL".to_string(),
        other => other.to_string(),
    }
}

fn priority_color(level: i64) -> String {
    match level {
        1 => "#EF4444", // Urgent
        2 => "#F97316", // High
        3 => "#EAB308", // Medium
        _ => "#6B7280", // Low
    }
    .to_string()
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::{build_space_from_export, ImportMaps};

    const NEW_SPACE: &str = "bbbbbbbb-0000-0000-0000-000000000002";
    const IMPORTER: &str = "cccccccc-0000-0000-0000-000000000003";

    fn st(id: &str, name: &str, ty: &str, pos: f64) -> LinearWorkflowState {
        LinearWorkflowState {
            id: id.to_string(),
            name: name.to_string(),
            state_type: ty.to_string(),
            position: pos,
            color: Some("#abcdef".to_string()),
        }
    }

    fn issue(id: &str, number: i64, title: &str) -> LinearIssue {
        LinearIssue {
            id: id.to_string(),
            identifier: Some(format!("WMT-{number}")),
            number,
            title: title.to_string(),
            ..Default::default()
        }
    }

    // ── Fix #3: filename + MIME from link/alt label when URL lacks an ext ──────

    #[test]
    fn test_filename_for_url_prefers_url_segment_with_extension() {
        // URL ends in a real filename → use it, ignore the label.
        assert_eq!(
            filename_for_url("https://uploads.linear.app/t/x/arch.png", Some("ignored")),
            "arch.png"
        );
    }

    #[test]
    fn test_filename_for_url_falls_back_to_label_when_url_has_no_extension() {
        // Bare UUID segment (no extension) + a `name.ext` link label → use the
        // label, and the MIME is guessed from it.
        let url = "https://uploads.linear.app/t/x/3f0c1a2b-4d5e-6789-abcd-ef0123456789";
        let filename = filename_for_url(url, Some("kom.m4v"));
        assert_eq!(filename, "kom.m4v");
        assert_eq!(guess_content_type(&filename), "video/quicktime");
    }

    #[test]
    fn test_filename_for_url_no_label_no_extension() {
        // No usable extension and no label → keep the bare segment.
        let url = "https://uploads.linear.app/t/x/3f0c1a2b-4d5e-6789-abcd-ef0123456789";
        assert_eq!(
            filename_for_url(url, None),
            "3f0c1a2b-4d5e-6789-abcd-ef0123456789"
        );
    }

    #[test]
    fn test_has_usable_extension() {
        assert!(has_usable_extension("arch.png"));
        assert!(has_usable_extension("kom.m4v"));
        assert!(!has_usable_extension(
            "3f0c1a2b-4d5e-6789-abcd-ef0123456789"
        ));
        assert!(!has_usable_extension("noext"));
        assert!(!has_usable_extension(".hidden")); // empty stem
        assert!(!has_usable_extension("a.toolongext")); // ext too long → not usable
    }

    #[test]
    fn test_preceding_label_recovers_image_and_link() {
        assert_eq!(
            preceding_label("foo ![kom.m4v]("),
            Some("kom.m4v".to_string())
        );
        assert_eq!(
            preceding_label("foo [kom.m4v]("),
            Some("kom.m4v".to_string())
        );
        assert_eq!(preceding_label("no opener here"), None);
        // An item-ref-looking `[[..](` is not a simple label.
        assert_eq!(preceding_label("see [[42]("), None);
    }

    /// Fix #3 end-to-end through the converter: a Linear file link whose URL ends
    /// in a bare UUID gets its filename + MIME from the `[name.ext]` link text.
    #[test]
    fn test_convert_linear_file_link_no_extension_uses_link_text() {
        let mut iss = issue("iss-vid", 99, "Has a video link");
        iss.description = Some(
            "Watch [kom.m4v](https://uploads.linear.app/team/x/3f0c1a2b-4d5e-6789-abcd-ef0123456789) here.".into(),
        );
        let ws = LinearWorkspace {
            team: LinearTeam {
                id: "team-1".into(),
                key: "WMT".into(),
                name: "Web".into(),
                ..Default::default()
            },
            issues: vec![iss],
            users: vec![LinearUser {
                id: "u1".into(),
                name: "Timo".into(),
                active: true,
                ..Default::default()
            }],
            ..Default::default()
        };
        let conv = convert(&ws);
        assert_eq!(conv.export.attachments.len(), 1);
        let att = &conv.export.attachments[0];
        assert_eq!(att.filename, "kom.m4v", "filename from link text");
        assert_eq!(
            att.content_type, "video/quicktime",
            "MIME from link-text ext"
        );
    }

    /// Self-contained inline test: exercises every mapping branch, then proves
    /// the produced SpaceExport imports without panicking.
    #[test]
    fn test_inline_conversion_and_import() {
        let team = LinearTeam {
            id: "team-1".into(),
            key: "WMT".into(),
            name: "My Web Team".into(),
            description: None,
            issue_estimation_type: Some("tShirt".into()),
        };

        // Workflow states: completed + canceled + duplicate (all completion),
        // plus started.
        let states = vec![
            st("s-backlog", "Backlog", "backlog", 0.0),
            st("s-started", "In Progress", "started", 1.0),
            st("s-done", "Done", "completed", 2.0),
            st("s-cancel", "Canceled", "canceled", 3.0),
            st("s-dup", "Duplicate", "duplicate", 4.0),
        ];

        // A group label "Type" with child "Chore", plus 2 standalone labels.
        let group_child = LinearLabelRef {
            id: "lbl-chore".into(),
            name: "Chore".into(),
            color: Some("#111111".into()),
            parent: Some(LinearLabelParent {
                id: "lbl-type".into(),
                name: Some("Type".into()),
            }),
        };
        let tag_a = LinearLabelRef {
            id: "lbl-bug".into(),
            name: "Bug".into(),
            color: Some("#EB5757".into()),
            parent: None,
        };
        let tag_b = LinearLabelRef {
            id: "lbl-feature".into(),
            name: "Feature".into(),
            color: Some("#BB87FC".into()),
            parent: None,
        };
        let issue_labels = vec![
            group_child.clone(),
            tag_a.clone(),
            tag_b.clone(),
            LinearLabelRef {
                id: "lbl-type".into(),
                name: "Type".into(),
                color: None,
                parent: None,
            },
        ];

        // Parent issue.
        let mut parent = issue("iss-parent", 10, "Parent");
        parent.state = Some(LinearRef {
            id: "s-started".into(),
        });
        parent.priority = 0; // none → no priority value

        // Child issue with: completed state, tShirt estimate 8 → "XL",
        // priority 2 (High), a group label + two standalone labels, an inline
        // image, a project + milestone + cycle, and parent link.
        let mut child = issue("iss-child", 11, "Child with everything");
        child.state = Some(LinearRef {
            id: "s-done".into(),
        });
        child.priority = 2;
        child.estimate = Some(8.0);
        child.parent = Some(LinearRef {
            id: "iss-parent".into(),
        });
        child.project = Some(LinearRef {
            id: "proj-1".into(),
        });
        child.project_milestone = Some(LinearRef { id: "ms-1".into() });
        child.cycle = Some(LinearRef { id: "cyc-1".into() });
        child.assignee = Some(LinearRef {
            id: "user-1".into(),
        });
        child.creator = Some(LinearRef {
            id: "user-1".into(),
        });
        child.created_at = Some("2026-02-01T10:00:00.000Z".into());
        child.updated_at = Some("2026-03-01T11:00:00.000Z".into());
        child.labels = LinearLabelNodes {
            nodes: vec![group_child.clone(), tag_a.clone(), tag_b.clone()],
        };
        child.description = Some(
            "See the diagram below.\n\n![arch.png](https://uploads.linear.app/team/x/arch.png)\n\nDone.".into(),
        );

        // A canceled issue and a duplicate-state issue to assert completion.
        let mut canceled = issue("iss-cancel", 12, "Canceled one");
        canceled.state = Some(LinearRef {
            id: "s-cancel".into(),
        });
        let mut dup_state = issue("iss-dupstate", 13, "Dup state");
        dup_state.state = Some(LinearRef { id: "s-dup".into() });

        // Relations: blocks, duplicate, related, and one dangling.
        let relations = vec![
            LinearRelation {
                id: "rel-1".into(),
                relation_type: "blocks".into(),
                issue: Some(LinearRef {
                    id: "iss-parent".into(),
                }),
                related_issue: Some(LinearRef {
                    id: "iss-child".into(),
                }),
            },
            LinearRelation {
                id: "rel-2".into(),
                relation_type: "duplicate".into(),
                issue: Some(LinearRef {
                    id: "iss-child".into(),
                }),
                related_issue: Some(LinearRef {
                    id: "iss-parent".into(),
                }),
            },
            LinearRelation {
                id: "rel-3".into(),
                relation_type: "related".into(),
                issue: Some(LinearRef {
                    id: "iss-parent".into(),
                }),
                related_issue: Some(LinearRef {
                    id: "iss-child".into(),
                }),
            },
            LinearRelation {
                id: "rel-4".into(),
                relation_type: "blocks".into(),
                issue: Some(LinearRef {
                    id: "iss-parent".into(),
                }),
                related_issue: Some(LinearRef {
                    id: "iss-missing".into(),
                }),
            },
        ];

        // Threaded comments on the child.
        let comments = vec![
            LinearComment {
                id: "cmt-root".into(),
                body: "Root comment".into(),
                created_at: Some("2026-03-02T08:00:00.000Z".into()),
                edited_at: None,
                user: Some(LinearRef {
                    id: "user-1".into(),
                }),
                parent: None,
                issue: Some(LinearRef {
                    id: "iss-child".into(),
                }),
            },
            LinearComment {
                id: "cmt-reply".into(),
                body: "A reply".into(),
                created_at: Some("2026-03-02T09:00:00.000Z".into()),
                edited_at: None,
                user: Some(LinearRef {
                    id: "user-1".into(),
                }),
                parent: Some(LinearRef {
                    id: "cmt-root".into(),
                }),
                issue: Some(LinearRef {
                    id: "iss-child".into(),
                }),
            },
        ];

        let projects = vec![LinearProject {
            id: "proj-1".into(),
            name: "Workflow Docs".into(),
            description: None,
            project_milestones: LinearProjectMilestoneNodes {
                nodes: vec![LinearProjectMilestone {
                    id: "ms-1".into(),
                    name: "Definitions".into(),
                    target_date: Some("2026-07-01".into()),
                }],
            },
        }];

        let cycles = vec![LinearCycle {
            id: "cyc-1".into(),
            number: Some(1),
            name: Some("Cycle 1".into()),
            starts_at: Some("2026-06-22T11:49:27.000Z".into()),
            ends_at: Some("2026-07-06T11:49:27.000Z".into()),
            completed_at: None,
        }];

        let documents = vec![LinearDocument {
            id: "doc-1".into(),
            title: "Project doc".into(),
            content: Some("# Heading\n\nSome body.".into()),
            created_at: Some("2026-01-01T00:00:00.000Z".into()),
            updated_at: Some("2026-02-01T00:00:00.000Z".into()),
            creator: Some(LinearRef {
                id: "user-1".into(),
            }),
        }];

        let users = vec![LinearUser {
            id: "user-1".into(),
            name: "Timo".into(),
            email: Some("timo@example.com".into()),
            active: true,
            admin: true,
        }];

        let ws = LinearWorkspace {
            team,
            issues: vec![parent, child, canceled, dup_state],
            comments,
            issue_labels,
            workflow_states: states,
            cycles,
            projects,
            documents,
            users,
            issue_relations: relations,
        };

        let conv = convert(&ws);
        let export = &conv.export;

        // ── Completion states: all three flagged ──────────────────────────────
        let status = &export.labels.definitions[&det_id("label", "status")];
        let done_v = &status.values[&det_id("status_value", "s-done")];
        let cancel_v = &status.values[&det_id("status_value", "s-cancel")];
        let dup_v = &status.values[&det_id("status_value", "s-dup")];
        let started_v = &status.values[&det_id("status_value", "s-started")];
        assert!(done_v.is_completion_state, "completed → completion");
        assert!(cancel_v.is_completion_state, "canceled → completion");
        assert!(dup_v.is_completion_state, "duplicate → completion");
        assert!(!started_v.is_completion_state, "started → not completion");

        // ── tShirt estimate 8 → XL ─────────────────────────────────────────────
        let est_label = &export.labels.definitions[&det_id("label", "estimate")];
        let est_v = &est_label.values[&det_id("estimate_value", "8")];
        assert_eq!(est_v.name, "XL");

        // ── Items by id ────────────────────────────────────────────────────────
        let child_item = export
            .items
            .iter()
            .find(|i| i.id == det_id("item", "iss-child"))
            .unwrap();
        let parent_item = export
            .items
            .iter()
            .find(|i| i.id == det_id("item", "iss-parent"))
            .unwrap();

        // Priority: 0 on parent → no priority value; 2 on child → High value set
        assert!(
            !parent_item
                .labels
                .contains_key(&det_id("label", "priority")),
            "priority 0 sets no value"
        );
        assert_eq!(
            child_item.labels.get(&det_id("label", "priority")),
            Some(&det_id("priority_value", "2"))
        );

        // Group label value set + Tags first-wins + warning
        assert_eq!(
            child_item.labels.get(&det_id("group_label", "lbl-type")),
            Some(&det_id("group_value", "lbl-chore")),
            "group child value set"
        );
        let tags_val = child_item.labels.get(&det_id("label", "tags"));
        assert!(
            tags_val.is_some(),
            "Tags value set (first of two standalone)"
        );
        // first-wins: exactly one of bug/feature
        let bug_v = det_id("tag_value", "lbl-bug");
        let feat_v = det_id("tag_value", "lbl-feature");
        assert!(
            tags_val == Some(&bug_v) || tags_val == Some(&feat_v),
            "Tags holds one standalone value"
        );
        assert!(
            conv.warnings
                .iter()
                .any(|w| w.contains("standalone labels")),
            "first-wins warning emitted: {:?}",
            conv.warnings
        );

        // Sub-issue parent_id
        assert_eq!(
            child_item.parent_id,
            Some(det_id("item", "iss-parent")),
            "parent_id set"
        );

        // Relations: blocks → child.blocked_by has parent; duplicate →
        // child.duplicate_of = parent; related dropped; dangling dropped + warn
        assert_eq!(child_item.blocked_by, vec![det_id("item", "iss-parent")]);
        assert_eq!(child_item.duplicate_of, Some(det_id("item", "iss-parent")));
        assert!(
            conv.warnings.iter().any(|w| w.contains("relation")),
            "dangling-relation warning emitted: {:?}",
            conv.warnings
        );

        // Cycle + milestone + project + assignee + provenance on child
        assert_eq!(child_item.cycle_id, Some(det_id("cycle", "cyc-1")));
        assert_eq!(child_item.milestone_id, Some(det_id("milestone", "ms-1")));
        assert_eq!(
            child_item.labels.get(&det_id("label", "project")),
            Some(&det_id("project_value", "proj-1"))
        );
        assert_eq!(child_item.assignee_user_ids, vec![det_id("user", "user-1")]);
        assert_eq!(child_item.created_by, Some(det_id("user", "user-1")));
        assert_eq!(
            child_item.created_at.as_deref(),
            Some("2026-02-01T10:00:00.000Z")
        );

        // ── Inline image: attachment minted, URL rewritten, url map entry ──────
        assert_eq!(export.attachments.len(), 1, "one inline image attachment");
        let att = &export.attachments[0];
        assert_eq!(att.filename, "arch.png");
        assert_eq!(att.content_type, "image/png");
        assert_eq!(att.size_bytes, 0);
        assert!(!att.orphaned);
        // URL map has the original Linear URL
        assert_eq!(
            conv.attachment_urls.get(&att.id).map(String::as_str),
            Some("https://uploads.linear.app/team/x/arch.png")
        );
        // Rewritten in description_text
        let dtext = child_item.description_text.as_deref().unwrap();
        assert!(
            dtext.contains(&format!(
                "/api/spaces/{}/attachments/{}",
                export.space.id, att.id
            )),
            "URL rewritten in text: {dtext}"
        );
        assert!(
            !dtext.contains("uploads.linear.app"),
            "original URL gone from text"
        );
        // Rewritten in the encoded yjs blob too
        let dyjs = child_item.description_yjs.as_deref().unwrap();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(dyjs)
            .unwrap();
        let blob = String::from_utf8_lossy(&decoded);
        assert!(
            blob.contains(&format!("attachments/{}", att.id)),
            "rewritten URL present in yjs blob"
        );
        assert!(
            !blob.contains("uploads.linear.app"),
            "original URL gone from yjs blob"
        );

        // ── Threaded comment ───────────────────────────────────────────────────
        assert_eq!(child_item.comments.len(), 2);
        let reply = child_item
            .comments
            .iter()
            .find(|c| c.id == det_id("comment", "cmt-reply"))
            .unwrap();
        assert_eq!(reply.parent_id, Some(det_id("comment", "cmt-root")));
        assert_eq!(reply.author_id, det_id("user", "user-1"));
        assert_eq!(reply.author_name.as_deref(), Some("Timo"));

        // ── By Cycle view seeded (cycles exist) ────────────────────────────────
        let by_cycle = export
            .views
            .definitions
            .values()
            .find(|v| v.name == "By Cycle");
        assert!(by_cycle.is_some(), "By Cycle view seeded when cycles exist");
        assert!(
            export
                .views
                .definitions
                .values()
                .any(|v| v.name == "By Status"),
            "By Status view always seeded"
        );

        // ── Space metadata ─────────────────────────────────────────────────────
        assert_eq!(export.space.short_id_prefix.as_deref(), Some("WMT"));
        assert_eq!(export.space.name, "My Web Team");
        assert_eq!(export.space.region, "eu");
        assert!(export.space.short_id_visible);

        // ── Documents + milestones + cycles + users ────────────────────────────
        assert_eq!(export.documents.len(), 1);
        assert_eq!(export.documents[0].title, "Project doc");
        assert!(export.documents[0].content_yjs.is_some());
        assert_eq!(
            export.milestones.definitions[&det_id("milestone", "ms-1")]
                .deadline
                .as_deref(),
            Some("2026-07-01")
        );
        assert_eq!(export.cycles.len(), 1);
        assert_eq!(export.cycles[0].name, "Cycle 1");
        assert_eq!(export.users.len(), 1);

        // ── PROVE IT IMPORTS: build_space_from_export must not panic ────────────
        let result = build_space_from_export(export, NEW_SPACE, IMPORTER, &ImportMaps::default());
        assert!(
            result.space_doc.transact().get_map("config").is_some(),
            "imported space doc has config"
        );
        // 4 items: at least one carried a short_id, so max > 0
        assert!(
            result.max_short_id >= 13,
            "short_ids carried through import"
        );
    }

    /// Dev smoke test against the local golden sample. No-ops in CI (returns
    /// early when the fixture directory is absent).
    #[test]
    fn test_golden_sample_round_trips() {
        // Locate tests/fixtures/linear relative to this crate.
        let crate_dir = env!("CARGO_MANIFEST_DIR");
        let fixtures = std::path::Path::new(crate_dir).join("../../../tests/fixtures/linear");
        let issues_path = fixtures.join("issues.json");
        if !issues_path.exists() {
            eprintln!("golden sample absent ({issues_path:?}); skipping");
            return;
        }

        let ws = load_golden(&fixtures);
        let conv = convert(&ws);
        eprintln!(
            "golden: {} items, {} attachments, {} warnings",
            conv.export.items.len(),
            conv.export.attachments.len(),
            conv.warnings.len()
        );
        for w in &conv.warnings {
            eprintln!("  warn: {w}");
        }
        assert!(
            conv.export.items.len() >= 18,
            "expected >=18 items from golden sample, got {}",
            conv.export.items.len()
        );

        // Round-trips through the import builder.
        let result =
            build_space_from_export(&conv.export, NEW_SPACE, IMPORTER, &ImportMaps::default());
        assert!(result.space_doc.transact().get_map("config").is_some());
        eprintln!(
            "golden import: max_short_id={}, doc_manifest={}, warnings={}",
            result.max_short_id,
            result.document_manifest.len(),
            result.warnings.len()
        );
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
