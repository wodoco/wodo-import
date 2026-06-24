//! Jira → `SpaceExport` v2 converter.
//!
//! Pure, DB-free. Maps Jira REST `/search` issue payloads (company-managed and
//! team-managed projects) onto Wodo's [`SpaceExport`], the exact shape the
//! import pipeline ([`crate::import::build_space_from_export`]) consumes.
//!
//! Mirrors the sibling [`crate::import::convert::linear`] converter: a
//! kind-scoped deterministic UUIDv5 helper makes every cross-reference (parent,
//! blocker, comment author, label) resolve to the same Wodo id on re-runs; ADF
//! rich text is rendered to Markdown (via [`crate::import::convert::adf`]) and
//! then encoded to a Yjs blob; inline `media` images become [`ExportAttachment`]
//! metadata + an `attachment id → content URL` map for a later download phase.
//!
//! Jira specifics vs. Linear: statuses carry a `statusCategory` (new /
//! indeterminate / done) that drives completion (BY CATEGORY, so a "Done"
//! category status named "Decided" still counts); `components`/`labels` are
//! first-wins single-valued Wodo labels; `customfield_10016` is story points
//! (Estimate); `fixVersions` are milestones; `customfield_10020` is the inline
//! sprint array (cycles); `parent` covers both epic→story and subtask→parent;
//! `issuelinks` are processed only via their `outwardIssue` entry to dedup.

use std::collections::{HashMap, HashSet};

use base64::Engine as _;
use serde::Deserialize;
use uuid::Uuid;
use yrs::{Doc, ReadTxn, StateVector, Transact, WriteTxn};

use crate::export::types::{
    ExportAttachment, ExportComment, ExportCycle, ExportItem, ExportMilestoneDef, ExportMilestones,
    ExportSpaceMetadata, ExportUser, SpaceExport,
};
use crate::import::convert::adf::adf_to_markdown;
use crate::markdown::write_markdown_to_fragment;
use crate::template::types::{
    TemplateLabelDef, TemplateLabelValue, TemplateLabels, TemplateViewDef, TemplateViews,
};

/// Fixed namespace for all v5 ids minted by this converter. A stable, arbitrary
/// constant so derived ids are reproducible across builds.
const NS_JIRA: Uuid = Uuid::from_u128(0x4a_69_72_61_5f_77_6f_64_6f_5f_63_6e_76_74_5f_31u128);

// =============================================================================
// Input model (serde structs matching the Jira REST /search payload)
// =============================================================================

/// A Jira project reference (carried on every issue's `fields.project`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct JiraProject {
    pub id: String,
    pub key: String,
    #[serde(default)]
    pub name: String,
}

/// A Jira user (assignee / reporter / creator / comment author / mention).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JiraUser {
    #[serde(default)]
    pub account_id: String,
    #[serde(default)]
    pub display_name: String,
    /// Hidden by default by Jira privacy settings; often absent.
    #[serde(default)]
    pub email_address: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct JiraStatusCategory {
    /// One of: "new", "indeterminate", "done".
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub id: i64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JiraStatus {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub status_category: JiraStatusCategory,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct JiraPriority {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JiraIssueType {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub subtask: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct JiraComponent {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JiraVersion {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub release_date: Option<String>,
}

/// An inline sprint object (team-managed `customfield_10020[]`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JiraSprint {
    pub id: i64,
    #[serde(default)]
    pub name: String,
    /// "future" | "active" | "closed".
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub start_date: Option<String>,
    #[serde(default)]
    pub end_date: Option<String>,
}

/// An issue link type (`Blocks`, `Duplicate`, `Relates`, `Cloners`, …).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct JiraLinkType {
    #[serde(default)]
    pub name: String,
}

/// A reference to another issue inside an `issuelinks[]` entry.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct JiraLinkedIssue {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub key: String,
}

/// One `issuelinks[]` entry. Exactly one of `outward_issue`/`inward_issue` is
/// set per entry; the converter only acts on the `outward_issue` direction so
/// each link (which appears on both endpoints) is processed once.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JiraIssueLink {
    #[serde(default)]
    pub r#type: JiraLinkType,
    #[serde(default)]
    pub outward_issue: Option<JiraLinkedIssue>,
    #[serde(default)]
    pub inward_issue: Option<JiraLinkedIssue>,
}

/// `fields.parent` — present for both subtask→parent and story→epic.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct JiraParent {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub key: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct JiraAttachment {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub filename: String,
    #[serde(default, rename = "mimeType")]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub size: i64,
    /// REST content URL (the fetcher downloads this).
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub created: Option<String>,
    #[serde(default)]
    pub author: Option<JiraUser>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct JiraComment {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub author: Option<JiraUser>,
    /// ADF body.
    #[serde(default)]
    pub body: Option<serde_json::Value>,
    #[serde(default)]
    pub created: Option<String>,
    #[serde(default)]
    pub updated: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct JiraComments {
    #[serde(default)]
    pub comments: Vec<JiraComment>,
}

/// The `fields` object of an issue. Custom fields are referenced by their
/// stable ids; everything not mapped is dropped.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JiraFields {
    #[serde(default)]
    pub summary: String,
    /// ADF document or null.
    #[serde(default)]
    pub description: Option<serde_json::Value>,
    #[serde(default)]
    pub issuetype: JiraIssueType,
    #[serde(default)]
    pub project: JiraProject,
    #[serde(default)]
    pub status: JiraStatus,
    /// Nullable: an issue may have no priority.
    #[serde(default)]
    pub priority: Option<JiraPriority>,
    #[serde(default)]
    pub components: Vec<JiraComponent>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default, rename = "fixVersions")]
    pub fix_versions: Vec<JiraVersion>,
    #[serde(default)]
    pub assignee: Option<JiraUser>,
    #[serde(default)]
    pub reporter: Option<JiraUser>,
    #[serde(default)]
    pub creator: Option<JiraUser>,
    #[serde(default)]
    pub parent: Option<JiraParent>,
    #[serde(default, rename = "issuelinks")]
    pub issue_links: Vec<JiraIssueLink>,
    #[serde(default)]
    pub attachment: Vec<JiraAttachment>,
    #[serde(default)]
    pub comment: JiraComments,
    #[serde(default)]
    pub created: Option<String>,
    #[serde(default)]
    pub updated: Option<String>,
    #[serde(default)]
    pub duedate: Option<String>,
    /// Story points (company- and team-managed both use 10016 here).
    #[serde(default, rename = "customfield_10016")]
    pub story_points: Option<f64>,
    /// Inline sprint array (team-managed boards).
    #[serde(default, rename = "customfield_10020")]
    pub sprints: Option<Vec<JiraSprint>>,
    /// Epic Link (older company-managed projects) — fallback for parent.
    #[serde(default, rename = "customfield_10014")]
    pub epic_link: Option<String>,
}

/// A single Jira issue from `/search`'s `issues[]`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct JiraIssue {
    pub id: String,
    pub key: String,
    #[serde(default)]
    pub fields: JiraFields,
}

/// Output of the pure converter: the export, fidelity warnings, and the
/// attachment-content URL map (keyed by minted attachment id) for a later
/// download phase. Mirrors `LinearConversion`.
#[derive(Debug, Clone)]
pub struct JiraConversion {
    pub export: SpaceExport,
    pub warnings: Vec<String>,
    /// `attachment id → Jira REST content URL`. The converter fetches no bytes.
    pub attachment_urls: HashMap<String, String>,
}

// =============================================================================
// Deterministic ids
// =============================================================================

/// Stable v5 UUID string from any Jira id, scoped by a kind prefix so different
/// entity kinds that share a source id never collide.
fn det_id(kind: &str, jira_id: &str) -> String {
    Uuid::new_v5(&NS_JIRA, format!("{kind}:{jira_id}").as_bytes()).to_string()
}

// =============================================================================
// Markdown → description_yjs (mirrors linear::markdown_to_content_yjs)
// =============================================================================

/// Encode markdown as the base64 Yjs rich-text blob the import builder expects
/// for `*_yjs` fields. Returns `None` for empty/whitespace input.
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
// Inline media (ADF) → attachment + URL rewrite
// =============================================================================

/// Resolve `![alt](jira-media:<id>)` sentinels in `markdown` against the issue's
/// attachments. The ADF media id is a media-services UUID that does NOT equal
/// the REST attachment id, so we correlate by filename (`media.attrs.alt` ==
/// `attachment.filename`), falling back to id. On a hit the attachment is minted
/// (deduped), its content URL recorded, and the sentinel rewritten to the Wodo
/// attachment route. Unresolved sentinels degrade to `![alt]()` and warn.
#[allow(clippy::too_many_arguments)] // internal converter helper; a params struct adds no clarity
fn resolve_media(
    markdown: &str,
    issue_key: &str,
    issue_attachments: &[JiraAttachment],
    space_id: &str,
    attachments: &mut Vec<ExportAttachment>,
    attachment_urls: &mut HashMap<String, String>,
    seen_attachments: &mut HashSet<String>,
    provenance_user: &str,
    now: &str,
    warnings: &mut Vec<String>,
) -> String {
    const NEEDLE: &str = "](jira-media:";
    if !markdown.contains(NEEDLE) {
        return markdown.to_string();
    }
    let mut out = String::with_capacity(markdown.len());
    let mut rest = markdown;
    while let Some(pos) = rest.find(NEEDLE) {
        // Emit up to and including the closing `](` of the markdown image.
        let keep = pos + 2; // through "]("
        out.push_str(&rest[..keep]);
        let after = &rest[keep..]; // starts at "jira-media:<id>)"
        let id_start = "jira-media:".len();
        let Some(close) = after.find(')') else {
            // Malformed sentinel — emit verbatim and stop.
            out.push_str(after);
            rest = "";
            break;
        };
        let media_id = &after[id_start..close];
        // Jira's ADF media id (a media-services UUID) is NOT the REST attachment
        // id, but `media.attrs.alt` (the markdown alt captured here) equals the
        // attachment filename. Correlate by filename; fall back to id.
        let alt = rest[..pos]
            .rfind("![")
            .map(|s| &rest[s + 2..pos])
            .unwrap_or("");
        let matched = issue_attachments
            .iter()
            .find(|a| !a.filename.is_empty() && a.filename == alt)
            .or_else(|| issue_attachments.iter().find(|a| a.id == media_id));

        match matched {
            Some(att) => {
                let att_id = mint_attachment(
                    att,
                    space_id,
                    attachments,
                    attachment_urls,
                    seen_attachments,
                    provenance_user,
                    now,
                );
                out.push_str(&format!("/api/spaces/{space_id}/attachments/{att_id}"));
            }
            None => {
                // Unresolved: leave a degraded empty src + warn.
                warnings.push(format!(
                    "issue {issue_key}: inline media (alt={alt:?}, id={media_id}) had no matching attachment; image left without a source"
                ));
            }
        }
        rest = &after[close..]; // resume at the closing ')'
    }
    out.push_str(rest);
    out
}

/// Mint (or reuse) an `ExportAttachment` for a Jira attachment and record its
/// content URL. Returns the Wodo attachment id.
fn mint_attachment(
    att: &JiraAttachment,
    _space_id: &str,
    attachments: &mut Vec<ExportAttachment>,
    attachment_urls: &mut HashMap<String, String>,
    seen_attachments: &mut HashSet<String>,
    provenance_user: &str,
    now: &str,
) -> String {
    let att_id = det_id("attachment", &att.id);
    if seen_attachments.insert(att_id.clone()) {
        let filename = if att.filename.is_empty() {
            "attachment".to_string()
        } else {
            att.filename.clone()
        };
        let content_type = att
            .mime_type
            .clone()
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| "application/octet-stream".to_string());
        attachments.push(ExportAttachment {
            id: att_id.clone(),
            filename,
            content_type,
            size_bytes: att.size,
            uploaded_by: att
                .author
                .as_ref()
                .filter(|a| !a.account_id.is_empty())
                .map(|a| det_id("user", &a.account_id))
                .unwrap_or_else(|| provenance_user.to_string()),
            uploaded_at: att.created.clone().unwrap_or_else(|| now.to_string()),
            document_id: None,
            orphaned: false,
        });
        if let Some(url) = att.content.clone().filter(|u| !u.is_empty()) {
            attachment_urls.insert(att_id.clone(), url);
        }
    }
    att_id
}

// =============================================================================
// Entry point
// =============================================================================

/// Convert a Jira project + its issues into a [`SpaceExport`] plus warnings and
/// the attachment-URL map. Mirrors `linear::convert`.
pub fn jira_to_space_export(project: &JiraProject, issues: &[JiraIssue]) -> JiraConversion {
    let mut warnings: Vec<String> = Vec::new();
    let mut attachments: Vec<ExportAttachment> = Vec::new();
    let mut attachment_urls: HashMap<String, String> = HashMap::new();
    let mut seen_attachments: HashSet<String> = HashSet::new();

    // ── Space metadata ────────────────────────────────────────────────────────
    let space_id = det_id("space", &project.id);
    let space_name = if project.name.is_empty() {
        project.key.clone()
    } else {
        project.name.clone()
    };
    let now = chrono::Utc::now().to_rfc3339();

    // ── Users (collect every referenced accountId) ────────────────────────────
    let users = collect_users(issues);
    let provenance_user = users
        .first()
        .map(|u| u.id.clone())
        .unwrap_or_else(|| Uuid::nil().to_string());

    let space = ExportSpaceMetadata {
        id: space_id.clone(),
        name: space_name.clone(),
        slug: slugify(&space_name),
        region: "eu".to_string(),
        short_id_prefix: Some(project.key.clone()),
        short_id_visible: true,
        created_at: now.clone(),
    };

    // ── Labels: Status, Priority, Type, Component, Tags, Estimate ─────────────
    let label_build = build_labels(issues);

    // ── Milestones (fixVersions, deduped, deadline = releaseDate) ─────────────
    let mut ms_order: Vec<String> = Vec::new();
    let mut ms_defs: HashMap<String, ExportMilestoneDef> = HashMap::new();
    for issue in issues {
        for v in &issue.fields.fix_versions {
            let id = det_id("milestone", &v.id);
            if ms_defs.contains_key(&id) {
                continue;
            }
            ms_order.push(id.clone());
            ms_defs.insert(
                id.clone(),
                ExportMilestoneDef {
                    id,
                    name: v.name.clone(),
                    description: v.description.clone(),
                    deadline: v.release_date.clone(),
                    deprecated: false,
                },
            );
        }
    }

    // ── Cycles (inline sprints, deduped) ──────────────────────────────────────
    let mut cycle_order: Vec<String> = Vec::new();
    let mut cycle_seen: HashSet<String> = HashSet::new();
    let mut cycles: Vec<ExportCycle> = Vec::new();
    for issue in issues {
        for sp in issue.fields.sprints.iter().flatten() {
            let id = det_id("cycle", &sp.id.to_string());
            if !cycle_seen.insert(id.clone()) {
                continue;
            }
            cycle_order.push(id.clone());
            cycles.push(ExportCycle {
                id,
                name: if sp.name.is_empty() {
                    format!("Sprint {}", sp.id)
                } else {
                    sp.name.clone()
                },
                start_date: sp.start_date.clone().unwrap_or_default(),
                end_date: sp.end_date.clone().unwrap_or_default(),
                archived: false,
            });
        }
    }

    // ── Items ─────────────────────────────────────────────────────────────────
    let issue_id_set: HashSet<&str> = issues.iter().map(|i| i.id.as_str()).collect();
    let key_to_id: HashMap<&str, &str> = issues
        .iter()
        .map(|i| (i.key.as_str(), i.id.as_str()))
        .collect();

    let mut items: Vec<ExportItem> = Vec::with_capacity(issues.len());
    for issue in issues {
        let f = &issue.fields;
        let item_id = det_id("item", &issue.id);
        let mut labels: HashMap<String, String> = HashMap::new();

        // Status
        if let Some(value_id) = label_build.status_value_by_status.get(f.status.id.as_str()) {
            labels.insert(label_build.status_label_id.clone(), value_id.clone());
        }
        // Priority (nullable → no value)
        if let Some(prio) = &f.priority {
            if let Some(value_id) = label_build.priority_value_by_id.get(prio.id.as_str()) {
                labels.insert(label_build.priority_label_id.clone(), value_id.clone());
            }
        }
        // Type
        if let Some(value_id) = label_build
            .type_value_by_name
            .get(f.issuetype.name.as_str())
        {
            labels.insert(label_build.type_label_id.clone(), value_id.clone());
        }
        // Component (first-wins + warn if >1)
        if let Some(first) = f.components.first() {
            if let Some(value_id) = label_build.component_value_by_id.get(first.id.as_str()) {
                labels.insert(label_build.component_label_id.clone(), value_id.clone());
            }
            if f.components.len() > 1 {
                warnings.push(format!(
                    "issue {} had {} components; Component keeps only the first ({} dropped)",
                    issue.key,
                    f.components.len(),
                    f.components.len() - 1
                ));
            }
        }
        // Tags (Jira labels[], first-wins + warn if >1)
        if let Some(first) = f.labels.first() {
            if let Some(value_id) = label_build.tag_value_by_name.get(first.as_str()) {
                labels.insert(label_build.tags_label_id.clone(), value_id.clone());
            }
            if f.labels.len() > 1 {
                warnings.push(format!(
                    "issue {} had {} labels; Tags keeps only the first ({} dropped)",
                    issue.key,
                    f.labels.len(),
                    f.labels.len() - 1
                ));
            }
        }
        // Estimate (story points)
        if let Some(points) = f.story_points {
            if let Some(value_id) = label_build.estimate_value_by_key.get(&estimate_key(points)) {
                labels.insert(label_build.estimate_label_id.clone(), value_id.clone());
            }
        }

        // Description: ADF → md → (media resolve) → yjs
        let (description_yjs, description_text) = match &f.description {
            Some(adf) if !adf.is_null() => {
                let md = adf_to_markdown(adf);
                if md.trim().is_empty() {
                    (None, None)
                } else {
                    let resolved = resolve_media(
                        &md,
                        &issue.key,
                        &f.attachment,
                        &space_id,
                        &mut attachments,
                        &mut attachment_urls,
                        &mut seen_attachments,
                        &provenance_user,
                        &now,
                        &mut warnings,
                    );
                    (markdown_to_content_yjs(&resolved), Some(resolved))
                }
            }
            _ => (None, None),
        };

        // Non-inline attachments (anything not already minted by media resolve)
        // also get an ExportAttachment so the file is carried over.
        for att in &f.attachment {
            mint_attachment(
                att,
                &space_id,
                &mut attachments,
                &mut attachment_urls,
                &mut seen_attachments,
                &provenance_user,
                &now,
            );
        }

        // Active/last sprint → cycle (prefer active, then last in array)
        let cycle_id = f.sprints.as_ref().and_then(|sprints| {
            sprints
                .iter()
                .rev()
                .find(|s| s.state.eq_ignore_ascii_case("active"))
                .or_else(|| sprints.last())
                .map(|s| det_id("cycle", &s.id.to_string()))
        });

        // Parent: fields.parent.key (covers epic→story AND subtask→parent);
        // fallback to the Epic Link custom field.
        let parent_id = parent_jira_id(f, &key_to_id).map(|jid| det_id("item", jid));

        let assignee_user_ids = f
            .assignee
            .as_ref()
            .filter(|a| !a.account_id.is_empty())
            .map(|a| vec![det_id("user", &a.account_id)])
            .unwrap_or_default();

        // created_by: reporter, then creator.
        let created_by = f
            .reporter
            .as_ref()
            .or(f.creator.as_ref())
            .filter(|u| !u.account_id.is_empty())
            .map(|u| det_id("user", &u.account_id));

        items.push(ExportItem {
            id: item_id,
            short_id: short_id_from_key(&issue.key),
            title: f.summary.clone(),
            description_text,
            description_yjs,
            labels,
            assignee_user_ids,
            assignee_team_ids: Vec::new(),
            due_date: f.duedate.clone(),
            start_date: None,
            milestone_id: f.fix_versions.first().map(|v| det_id("milestone", &v.id)),
            cycle_id,
            parent_id,
            blocked_by: Vec::new(),
            duplicate_of: None,
            archived: false,
            deep_archived: false,
            created_at: f.created.clone(),
            created_by,
            updated_at: f.updated.clone(),
            archived_at: None,
            completion_prompt: None,
            completion_note_text: None,
            completion_note_yjs: None,
            comments: Vec::new(),
        });
    }

    // ── Issue links (process only outward entries; dedups both endpoints) ─────
    let mut item_idx_by_jira: HashMap<&str, usize> = HashMap::new();
    for (idx, issue) in issues.iter().enumerate() {
        item_idx_by_jira.insert(issue.id.as_str(), idx);
    }
    let mut dropped_links = 0usize;
    for issue in issues {
        for link in &issue.fields.issue_links {
            let Some(out) = &link.outward_issue else {
                continue; // inward entry → handled on the other endpoint
            };
            let this_id = issue.id.as_str();
            // Resolve the outward endpoint to an imported issue id (by id or key).
            let other_id = if issue_id_set.contains(out.id.as_str()) {
                out.id.as_str()
            } else if let Some(&mapped) = key_to_id.get(out.key.as_str()) {
                mapped
            } else {
                dropped_links += 1;
                continue;
            };
            match link.r#type.name.as_str() {
                // "A blocks B" (A is this, outward is B) ⇒ B.blocked_by += A
                "Blocks" => {
                    if let Some(&bi) = item_idx_by_jira.get(other_id) {
                        items[bi].blocked_by.push(det_id("item", this_id));
                    }
                }
                // "A duplicates B" ⇒ A.duplicate_of = B
                "Duplicate" => {
                    if let Some(&ai) = item_idx_by_jira.get(this_id) {
                        items[ai].duplicate_of = Some(det_id("item", other_id));
                    }
                }
                // Relates / Cloners / anything else → drop (no Wodo analog).
                _ => {}
            }
        }
    }
    if dropped_links > 0 {
        warnings.push(format!(
            "{dropped_links} issue link(s) referenced an issue outside the import and were dropped"
        ));
    }

    // ── Comments (FLAT — no parent_id) ────────────────────────────────────────
    for issue in issues {
        let Some(&item_idx) = item_idx_by_jira.get(issue.id.as_str()) else {
            continue;
        };
        for c in &issue.fields.comment.comments {
            let author_id = c
                .author
                .as_ref()
                .filter(|a| !a.account_id.is_empty())
                .map(|a| det_id("user", &a.account_id))
                .unwrap_or_else(|| Uuid::nil().to_string());
            let author_name = c
                .author
                .as_ref()
                .map(|a| a.display_name.clone())
                .filter(|n| !n.is_empty());
            let md = c
                .body
                .as_ref()
                .filter(|b| !b.is_null())
                .map(adf_to_markdown)
                .unwrap_or_default();
            let resolved = resolve_media(
                &md,
                &issue.key,
                &issue.fields.attachment,
                &space_id,
                &mut attachments,
                &mut attachment_urls,
                &mut seen_attachments,
                &provenance_user,
                &now,
                &mut warnings,
            );
            items[item_idx].comments.push(ExportComment {
                id: det_id("comment", &c.id),
                author_id,
                author_name,
                content_text: if resolved.trim().is_empty() {
                    None
                } else {
                    Some(resolved.clone())
                },
                content_yjs: markdown_to_content_yjs(&resolved),
                created_at: c.created.clone(),
                edited_at: c.updated.clone(),
                deleted: false,
                deleted_at: None,
                parent_id: None, // Jira comments are flat
            });
        }
    }

    // ── Views: By Status always; By Sprint when sprints exist ─────────────────
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
        documents: Vec::new(),
        attachments,
        users,
        teams: Vec::new(),
        overview_text: None,
        overview_yjs: None,
    };

    JiraConversion {
        export,
        warnings,
        attachment_urls,
    }
}

// =============================================================================
// Users
// =============================================================================

/// Collect every distinct accountId referenced (assignee / reporter / creator /
/// comment author / mention) into deterministic `ExportUser` entries, ordered
/// by accountId.
fn collect_users(issues: &[JiraIssue]) -> Vec<ExportUser> {
    // accountId → (display_name, email)
    let mut by_account: HashMap<String, (String, Option<String>)> = HashMap::new();
    let mut record = |u: &JiraUser| {
        if u.account_id.is_empty() {
            return;
        }
        let entry = by_account
            .entry(u.account_id.clone())
            .or_insert_with(|| (u.display_name.clone(), u.email_address.clone()));
        if entry.0.is_empty() && !u.display_name.is_empty() {
            entry.0 = u.display_name.clone();
        }
        if entry.1.is_none() {
            entry.1 = u.email_address.clone();
        }
    };

    for issue in issues {
        let f = &issue.fields;
        for u in [&f.assignee, &f.reporter, &f.creator].into_iter().flatten() {
            record(u);
        }
        for c in &f.comment.comments {
            if let Some(a) = &c.author {
                record(a);
            }
        }
        for att in &f.attachment {
            if let Some(a) = &att.author {
                record(a);
            }
        }
        // Mentions in description + comment bodies.
        if let Some(desc) = &f.description {
            collect_mention_users(desc, &mut record);
        }
        for c in &f.comment.comments {
            if let Some(b) = &c.body {
                collect_mention_users(b, &mut record);
            }
        }
    }

    let mut accounts: Vec<String> = by_account.keys().cloned().collect();
    accounts.sort();
    accounts
        .into_iter()
        .map(|acc| {
            let (name, email) = by_account.remove(&acc).unwrap();
            ExportUser {
                id: det_id("user", &acc),
                display_name: if name.is_empty() {
                    "Unknown user".to_string()
                } else {
                    name
                },
                email,
            }
        })
        .collect()
}

/// Walk an ADF value, recording any `mention` node's accountId.
fn collect_mention_users<F: FnMut(&JiraUser)>(node: &serde_json::Value, record: &mut F) {
    if node.get("type").and_then(|t| t.as_str()) == Some("mention") {
        if let Some(attrs) = node.get("attrs") {
            let id = attrs.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let text = attrs
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim_start_matches('@');
            if !id.is_empty() {
                record(&JiraUser {
                    account_id: id.to_string(),
                    display_name: text.to_string(),
                    email_address: None,
                });
            }
        }
    }
    if let Some(content) = node.get("content").and_then(|c| c.as_array()) {
        for child in content {
            collect_mention_users(child, record);
        }
    }
}

// =============================================================================
// Label building
// =============================================================================

struct LabelBuild {
    labels: TemplateLabels,
    status_label_id: String,
    status_value_by_status: HashMap<String, String>, // jira status id → value id
    priority_label_id: String,
    priority_value_by_id: HashMap<String, String>, // jira priority id → value id
    type_label_id: String,
    type_value_by_name: HashMap<String, String>, // issuetype name → value id
    component_label_id: String,
    component_value_by_id: HashMap<String, String>, // component id → value id
    tags_label_id: String,
    tag_value_by_name: HashMap<String, String>, // jira label string → value id
    estimate_label_id: String,
    estimate_value_by_key: HashMap<String, String>, // formatted points → value id
}

fn build_labels(issues: &[JiraIssue]) -> LabelBuild {
    let mut order: Vec<String> = Vec::new();
    let mut definitions: HashMap<String, TemplateLabelDef> = HashMap::new();

    // ── Status ────────────────────────────────────────────────────────────────
    // Distinct statuses, ordered by statusCategory (new<indeterminate<done),
    // then by name; completion is BY CATEGORY ("done").
    let status_label_id = det_id("label", "status");
    let mut status_value_by_status: HashMap<String, String> = HashMap::new();
    {
        // id → (name, category_key, category_rank)
        let mut seen: HashMap<String, (String, String, i32)> = HashMap::new();
        for issue in issues {
            let s = &issue.fields.status;
            if s.id.is_empty() {
                continue;
            }
            seen.entry(s.id.clone()).or_insert_with(|| {
                (
                    s.name.clone(),
                    s.status_category.key.clone(),
                    category_rank(&s.status_category.key),
                )
            });
        }
        let mut statuses: Vec<(String, String, String, i32)> = seen
            .into_iter()
            .map(|(id, (name, cat, rank))| (id, name, cat, rank))
            .collect();
        statuses.sort_by(|a, b| a.3.cmp(&b.3).then_with(|| a.1.cmp(&b.1)));

        let mut values_order = Vec::new();
        let mut values = HashMap::new();
        for (id, name, cat, _rank) in &statuses {
            let value_id = det_id("status_value", id);
            values_order.push(value_id.clone());
            values.insert(
                value_id.clone(),
                TemplateLabelValue {
                    id: value_id.clone(),
                    name: name.clone(),
                    color: status_color(cat),
                    is_completion_state: cat == "done",
                    ..Default::default()
                },
            );
            status_value_by_status.insert(id.clone(), value_id);
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
    // Distinct priorities by id; ordered by Jira's canonical severity rank.
    let priority_label_id = det_id("label", "priority");
    let mut priority_value_by_id: HashMap<String, String> = HashMap::new();
    {
        let mut seen: HashMap<String, String> = HashMap::new(); // id → name
        for issue in issues {
            if let Some(p) = &issue.fields.priority {
                if !p.id.is_empty() {
                    seen.entry(p.id.clone()).or_insert_with(|| p.name.clone());
                }
            }
        }
        if !seen.is_empty() {
            let mut prios: Vec<(String, String)> = seen.into_iter().collect();
            // Jira priority ids 1..5 are Highest..Lowest; sort numerically.
            prios.sort_by(|a, b| {
                let ai = a.0.parse::<i64>().unwrap_or(i64::MAX);
                let bi = b.0.parse::<i64>().unwrap_or(i64::MAX);
                ai.cmp(&bi).then_with(|| a.1.cmp(&b.1))
            });
            let mut values_order = Vec::new();
            let mut values = HashMap::new();
            for (id, name) in &prios {
                let value_id = det_id("priority_value", id);
                values_order.push(value_id.clone());
                values.insert(
                    value_id.clone(),
                    TemplateLabelValue {
                        id: value_id.clone(),
                        name: name.clone(),
                        color: priority_color(id),
                        ..Default::default()
                    },
                );
                priority_value_by_id.insert(id.clone(), value_id);
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
    }

    // ── Type ──────────────────────────────────────────────────────────────────
    let type_label_id = det_id("label", "type");
    let mut type_value_by_name: HashMap<String, String> = HashMap::new();
    {
        let mut names: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for issue in issues {
            let n = &issue.fields.issuetype.name;
            if !n.is_empty() && seen.insert(n.clone()) {
                names.push(n.clone());
            }
        }
        names.sort();
        if !names.is_empty() {
            let mut values_order = Vec::new();
            let mut values = HashMap::new();
            for n in &names {
                let value_id = det_id("type_value", n);
                values_order.push(value_id.clone());
                values.insert(
                    value_id.clone(),
                    TemplateLabelValue {
                        id: value_id.clone(),
                        name: n.clone(),
                        color: "#6B7280".to_string(),
                        ..Default::default()
                    },
                );
                type_value_by_name.insert(n.clone(), value_id);
            }
            order.push(type_label_id.clone());
            definitions.insert(
                type_label_id.clone(),
                TemplateLabelDef {
                    id: type_label_id.clone(),
                    name: "Type".to_string(),
                    icon: "tag".to_string(),
                    values_order,
                    values,
                    ..Default::default()
                },
            );
        }
    }

    // ── Component ───────────────────────────────────────────────────────────────
    let component_label_id = det_id("label", "component");
    let mut component_value_by_id: HashMap<String, String> = HashMap::new();
    {
        // id → name, deterministic by id.
        let mut seen: HashMap<String, String> = HashMap::new();
        for issue in issues {
            for c in &issue.fields.components {
                if !c.id.is_empty() {
                    seen.entry(c.id.clone()).or_insert_with(|| c.name.clone());
                }
            }
        }
        if !seen.is_empty() {
            let mut comps: Vec<(String, String)> = seen.into_iter().collect();
            comps.sort_by(|a, b| a.0.cmp(&b.0));
            let mut values_order = Vec::new();
            let mut values = HashMap::new();
            for (id, name) in &comps {
                let value_id = det_id("component_value", id);
                values_order.push(value_id.clone());
                values.insert(
                    value_id.clone(),
                    TemplateLabelValue {
                        id: value_id.clone(),
                        name: name.clone(),
                        color: "#6B7280".to_string(),
                        ..Default::default()
                    },
                );
                component_value_by_id.insert(id.clone(), value_id);
            }
            order.push(component_label_id.clone());
            definitions.insert(
                component_label_id.clone(),
                TemplateLabelDef {
                    id: component_label_id.clone(),
                    name: "Component".to_string(),
                    icon: "folder".to_string(),
                    values_order,
                    values,
                    ..Default::default()
                },
            );
        }
    }

    // ── Tags (Jira labels[]) ────────────────────────────────────────────────────
    let tags_label_id = det_id("label", "tags");
    let mut tag_value_by_name: HashMap<String, String> = HashMap::new();
    {
        let mut names: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for issue in issues {
            for l in &issue.fields.labels {
                if !l.is_empty() && seen.insert(l.clone()) {
                    names.push(l.clone());
                }
            }
        }
        names.sort();
        if !names.is_empty() {
            let mut values_order = Vec::new();
            let mut values = HashMap::new();
            for n in &names {
                let value_id = det_id("tag_value", n);
                values_order.push(value_id.clone());
                values.insert(
                    value_id.clone(),
                    TemplateLabelValue {
                        id: value_id.clone(),
                        name: n.clone(),
                        color: "#6B7280".to_string(),
                        ..Default::default()
                    },
                );
                tag_value_by_name.insert(n.clone(), value_id);
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
    }

    // ── Estimate (story points) ─────────────────────────────────────────────────
    let estimate_label_id = det_id("label", "estimate");
    let mut estimate_value_by_key: HashMap<String, String> = HashMap::new();
    {
        // Distinct rendered point values, numerically ordered.
        let mut nums: Vec<f64> = issues
            .iter()
            .filter_map(|i| i.fields.story_points)
            .collect();
        nums.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mut seen: HashSet<String> = HashSet::new();
        let mut ordered_keys: Vec<(String, f64)> = Vec::new();
        for n in nums {
            let key = estimate_key(n);
            if seen.insert(key.clone()) {
                ordered_keys.push((key, n));
            }
        }
        if !ordered_keys.is_empty() {
            let mut values_order = Vec::new();
            let mut values = HashMap::new();
            for (key, _n) in &ordered_keys {
                let value_id = det_id("estimate_value", key);
                values_order.push(value_id.clone());
                values.insert(
                    value_id.clone(),
                    TemplateLabelValue {
                        id: value_id.clone(),
                        name: key.clone(),
                        color: "#6B7280".to_string(),
                        ..Default::default()
                    },
                );
                estimate_value_by_key.insert(key.clone(), value_id);
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
        status_value_by_status,
        priority_label_id,
        priority_value_by_id,
        type_label_id,
        type_value_by_name,
        component_label_id,
        component_value_by_id,
        tags_label_id,
        tag_value_by_name,
        estimate_label_id,
        estimate_value_by_key,
    }
}

// =============================================================================
// Views
// =============================================================================

fn build_views(status_label_id: &str, has_sprints: bool) -> TemplateViews {
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

    if has_sprints {
        let by_sprint_id = det_id("view", "by-sprint");
        order.push(by_sprint_id.clone());
        definitions.insert(
            by_sprint_id.clone(),
            TemplateViewDef {
                id: by_sprint_id.clone(),
                name: "By Sprint".to_string(),
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

/// Resolve an issue's parent Jira issue id: prefer `fields.parent` (covers both
/// epic→story and subtask→parent), then the Epic Link custom field. Resolves to
/// the parent's Jira numeric id when the parent is in the imported set.
fn parent_jira_id<'a>(f: &'a JiraFields, key_to_id: &HashMap<&str, &'a str>) -> Option<&'a str> {
    if let Some(parent) = &f.parent {
        if !parent.id.is_empty() {
            return Some(parent.id.as_str());
        }
        if let Some(&id) = key_to_id.get(parent.key.as_str()) {
            return Some(id);
        }
    }
    // Epic Link is a key (e.g. "SCRUM-5") — resolve via key map.
    if let Some(epic_key) = &f.epic_link {
        if let Some(&id) = key_to_id.get(epic_key.as_str()) {
            return Some(id);
        }
    }
    None
}

/// The numeric short id from an issue key like `MTR-12` → 12.
fn short_id_from_key(key: &str) -> Option<i64> {
    key.rsplit_once('-')
        .and_then(|(_, n)| n.parse::<i64>().ok())
}

/// Render a story-point number for display + as a stable map key: integers drop
/// the trailing `.0` (5.0 → "5"), fractionals keep one decimal (0.5 → "0.5").
fn estimate_key(points: f64) -> String {
    if points.fract() == 0.0 {
        format!("{}", points as i64)
    } else {
        // Trim trailing zeros beyond what's significant.
        let s = format!("{points}");
        s
    }
}

/// statusCategory ordering: new (0) < indeterminate (1) < done (2); unknown last.
fn category_rank(key: &str) -> i32 {
    match key {
        "new" => 0,
        "indeterminate" => 1,
        "done" => 2,
        _ => 3,
    }
}

fn status_color(category: &str) -> String {
    match category {
        "new" => "#6B7280",           // gray
        "indeterminate" => "#3B82F6", // blue
        "done" => "#22C55E",          // green
        _ => "#6B7280",
    }
    .to_string()
}

/// Jira priority colors by id (1 Highest … 5 Lowest).
fn priority_color(id: &str) -> String {
    match id {
        "1" => "#DC2626", // Highest
        "2" => "#EF4444", // High
        "3" => "#EAB308", // Medium
        "4" => "#3B82F6", // Low
        "5" => "#6B7280", // Lowest
        _ => "#6B7280",
    }
    .to_string()
}

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

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::{build_space_from_export, ImportMaps};
    use serde_json::json;

    const NEW_SPACE: &str = "bbbbbbbb-0000-0000-0000-000000000002";
    const IMPORTER: &str = "cccccccc-0000-0000-0000-000000000003";

    fn user(acc: &str, name: &str) -> JiraUser {
        JiraUser {
            account_id: acc.to_string(),
            display_name: name.to_string(),
            email_address: None,
        }
    }

    fn status(id: &str, name: &str, cat: &str) -> JiraStatus {
        JiraStatus {
            id: id.to_string(),
            name: name.to_string(),
            status_category: JiraStatusCategory {
                key: cat.to_string(),
                name: cat.to_string(),
                id: 0,
            },
        }
    }

    fn issue(key: &str, id: &str, summary: &str) -> JiraIssue {
        JiraIssue {
            id: id.to_string(),
            key: key.to_string(),
            fields: JiraFields {
                summary: summary.to_string(),
                ..Default::default()
            },
        }
    }

    fn project() -> JiraProject {
        JiraProject {
            id: "10001".into(),
            key: "MTR".into(),
            name: "My test RFP".into(),
        }
    }

    #[test]
    fn test_short_id_from_key() {
        assert_eq!(short_id_from_key("MTR-12"), Some(12));
        assert_eq!(short_id_from_key("SCRUM-1"), Some(1));
        assert_eq!(short_id_from_key("nodash"), None);
    }

    #[test]
    fn test_estimate_key_formats_integers() {
        assert_eq!(estimate_key(5.0), "5");
        assert_eq!(estimate_key(8.0), "8");
        assert_eq!(estimate_key(0.5), "0.5");
    }

    #[test]
    fn media_correlates_by_filename_not_id() {
        // Real Jira shape (verified against MTR-5): the ADF media id is a
        // media-services UUID that does NOT equal the REST attachment id, but
        // media.attrs.alt == attachment.filename. Fails on the old id-only match.
        let mut iss = issue("MTR-5", "10005", "Has image");
        iss.fields.status = status("1", "To Do", "new");
        iss.fields.description = Some(json!({
            "version": 1, "type": "doc",
            "content": [
                {"type":"paragraph","content":[{"type":"text","text":"See this image."}]},
                {"type":"mediaSingle","content":[
                    {"type":"media","attrs":{"type":"file","id":"533c764f-media-uuid","alt":"pic.png"}}
                ]}
            ]
        }));
        iss.fields.attachment = vec![JiraAttachment {
            id: "10001".into(),
            filename: "pic.png".into(),
            mime_type: Some("image/png".into()),
            size: 123,
            content: Some("https://jira/rest/api/3/attachment/content/10001".into()),
            ..Default::default()
        }];

        let conv = jira_to_space_export(&project(), &[iss]);

        assert!(
            !conv
                .warnings
                .iter()
                .any(|w| w.contains("no matching attachment")),
            "inline media should resolve by filename, not id: {:?}",
            conv.warnings
        );
        let att_id = det_id("attachment", "10001");
        let att = conv
            .export
            .attachments
            .iter()
            .find(|a| a.id == att_id)
            .expect("attachment minted");
        assert_eq!(att.content_type, "image/png");
        assert_eq!(att.size_bytes, 123);
        assert_eq!(
            conv.attachment_urls.get(&att_id).map(String::as_str),
            Some("https://jira/rest/api/3/attachment/content/10001")
        );
        // Description references the rewritten attachment route, not a degraded src.
        let space = conv.export.space.id.clone();
        let dtext = conv.export.items[0]
            .description_text
            .clone()
            .unwrap_or_default();
        assert!(
            dtext.contains(&format!("/api/spaces/{space}/attachments/{att_id}")),
            "image rewritten in description: {dtext}"
        );
    }

    /// Self-contained inline test exercising every mapping branch, then proving
    /// the produced export imports without panicking.
    #[test]
    fn test_inline_conversion_and_import() {
        // ── Statuses: a "done"-category status NAMED "Decided" (not "Done") ─────
        let st_decided = status("10008", "Decided", "done");
        let st_preparing = status("10004", "Preparing", "indeterminate");
        let st_todo = status("10005", "RFP creation", "new");

        // Rich ADF description with table, panel, mention, link, code, list.
        let rich = json!({
            "type": "doc", "version": 1, "content": [
                {"type": "heading", "attrs": {"level": 2}, "content": [
                    {"type": "text", "text": "CI setup"}]},
                {"type": "paragraph", "content": [
                    {"type": "text", "text": "Need a "},
                    {"type": "text", "text": "robust", "marks": [{"type": "strong"}]},
                    {"type": "text", "text": " pipeline. See "},
                    {"type": "text", "text": "wodo.dev", "marks": [
                        {"type": "link", "attrs": {"href": "https://wodo.dev"}}]},
                    {"type": "text", "text": ". Owner: "},
                    {"type": "mention", "attrs": {"id": "acc-1", "text": "@Timo"}},
                    {"type": "text", "text": "."}]},
                {"type": "bulletList", "content": [
                    {"type": "listItem", "content": [{"type": "paragraph", "content": [
                        {"type": "text", "text": "Lint"}]}]}]},
                {"type": "codeBlock", "attrs": {"language": "rust"}, "content": [
                    {"type": "text", "text": "fn main() {}"}]},
                {"type": "panel", "attrs": {"panelType": "info"}, "content": [
                    {"type": "paragraph", "content": [{"type": "text", "text": "4 GB RAM"}]}]},
                {"type": "table", "content": [
                    {"type": "tableRow", "content": [
                        {"type": "tableHeader", "content": [{"type": "paragraph",
                            "content": [{"type": "text", "text": "Stage"}]}]},
                        {"type": "tableHeader", "content": [{"type": "paragraph",
                            "content": [{"type": "text", "text": "Time"}]}]}]},
                    {"type": "tableRow", "content": [
                        {"type": "tableCell", "content": [{"type": "paragraph",
                            "content": [{"type": "text", "text": "Build"}]}]},
                        {"type": "tableCell", "content": [{"type": "paragraph",
                            "content": [{"type": "text", "text": "3m"}]}]}]}]}
            ]
        });

        // MTR-1: done-by-category, story points 5.0, fixVersion, attachment,
        // a comment, a Duplicate link to MTR-3, priority Medium.
        let mut mtr1 = issue("MTR-1", "10004", "Set up CI pipeline");
        mtr1.fields.status = st_decided.clone();
        mtr1.fields.priority = Some(JiraPriority {
            id: "3".into(),
            name: "Medium".into(),
        });
        mtr1.fields.issuetype = JiraIssueType {
            name: "Task".into(),
            subtask: false,
        };
        mtr1.fields.description = Some(rich);
        mtr1.fields.story_points = Some(5.0);
        mtr1.fields.components = vec![JiraComponent {
            id: "10000".into(),
            name: "Backend".into(),
        }];
        mtr1.fields.fix_versions = vec![JiraVersion {
            id: "10000".into(),
            name: "v1.0".into(),
            description: Some("First release".into()),
            release_date: Some("2026-09-30".into()),
        }];
        mtr1.fields.assignee = Some(user("acc-1", "Timo"));
        mtr1.fields.reporter = Some(user("acc-1", "Timo"));
        mtr1.fields.created = Some("2026-06-22T23:39:41.764+0200".into());
        mtr1.fields.updated = Some("2026-06-22T23:43:14.287+0200".into());
        mtr1.fields.duedate = Some("2026-07-15".into());
        mtr1.fields.attachment = vec![JiraAttachment {
            id: "10000".into(),
            filename: "rfp-notes.txt".into(),
            mime_type: Some("text/plain".into()),
            size: 62,
            content: Some("https://jira.example/attachment/content/10000".into()),
            created: Some("2026-06-22T23:43:13.793+0200".into()),
            author: Some(user("acc-1", "Timo")),
        }];
        // Duplicate link: MTR-1 duplicates MTR-3 (outward)
        mtr1.fields.issue_links = vec![JiraIssueLink {
            r#type: JiraLinkType {
                name: "Duplicate".into(),
            },
            outward_issue: Some(JiraLinkedIssue {
                id: "10006".into(),
                key: "MTR-3".into(),
            }),
            inward_issue: None,
        }];
        mtr1.fields.comment = JiraComments {
            comments: vec![JiraComment {
                id: "10000".into(),
                author: Some(user("acc-1", "Timo")),
                body: Some(json!({"type": "doc", "version": 1, "content": [
                    {"type": "paragraph", "content": [
                        {"type": "text", "text": "Pipeline draft is up."}]}]})),
                created: Some("2026-06-22T23:42:18.996+0200".into()),
                updated: Some("2026-06-22T23:42:18.996+0200".into()),
            }],
        };

        // MTR-2: null description, null priority, TWO components + TWO labels
        // (first-wins + warn), points 8.0, an Epic via parent (covers parent),
        // and a Blocks link from MTR-3 lands here.
        let mut mtr2 = issue("MTR-2", "10005", "Implement RFP API");
        mtr2.fields.status = st_preparing.clone();
        mtr2.fields.priority = None; // null priority
        mtr2.fields.issuetype = JiraIssueType {
            name: "Task".into(),
            subtask: false,
        };
        mtr2.fields.description = None;
        mtr2.fields.story_points = Some(8.0);
        mtr2.fields.components = vec![
            JiraComponent {
                id: "10000".into(),
                name: "Backend".into(),
            },
            JiraComponent {
                id: "10001".into(),
                name: "Frontend".into(),
            },
        ];
        mtr2.fields.labels = vec!["api".into(), "backend".into()];

        // MTR-3: To Do, Blocks MTR-2 (outward) + Relates to MTR-1 (dropped).
        let mut mtr3 = issue("MTR-3", "10006", "PDF export crashes");
        mtr3.fields.status = st_todo.clone();
        mtr3.fields.priority = Some(JiraPriority {
            id: "4".into(),
            name: "Low".into(),
        });
        mtr3.fields.issuetype = JiraIssueType {
            name: "Task".into(),
            subtask: false,
        };
        mtr3.fields.issue_links = vec![
            JiraIssueLink {
                r#type: JiraLinkType {
                    name: "Blocks".into(),
                },
                outward_issue: Some(JiraLinkedIssue {
                    id: "10005".into(),
                    key: "MTR-2".into(),
                }),
                inward_issue: None,
            },
            JiraIssueLink {
                r#type: JiraLinkType {
                    name: "Relates".into(),
                },
                outward_issue: Some(JiraLinkedIssue {
                    id: "10005".into(),
                    key: "MTR-2".into(),
                }),
                inward_issue: None,
            },
        ];

        // MTR-6: a SUBTASK whose parent is MTR-2 (parent covers subtasks too),
        // plus an inline sprint → cycle, exercising By Sprint view.
        let mut mtr6 = issue("MTR-6", "10009", "Write integration tests");
        mtr6.fields.status = st_todo.clone();
        mtr6.fields.issuetype = JiraIssueType {
            name: "Sub-task".into(),
            subtask: true,
        };
        mtr6.fields.parent = Some(JiraParent {
            id: "10005".into(),
            key: "MTR-2".into(),
        });
        mtr6.fields.sprints = Some(vec![JiraSprint {
            id: 3,
            name: "Importer Fixture Sprint".into(),
            state: "future".into(),
            start_date: Some("2026-06-22T09:00:00.000Z".into()),
            end_date: Some("2026-07-06T17:00:00.000Z".into()),
        }]);

        let issues = vec![mtr1, mtr2, mtr3, mtr6];
        let conv = jira_to_space_export(&project(), &issues);
        let export = &conv.export;

        // ── done-by-category: "Decided" is a completion state ───────────────────
        let status_label = &export.labels.definitions[&det_id("label", "status")];
        let decided_v = &status_label.values[&det_id("status_value", "10008")];
        assert_eq!(decided_v.name, "Decided");
        assert!(
            decided_v.is_completion_state,
            "done-category status (named Decided) is completion"
        );
        let todo_v = &status_label.values[&det_id("status_value", "10005")];
        assert!(
            !todo_v.is_completion_state,
            "new-category is not completion"
        );

        // ── Items ───────────────────────────────────────────────────────────────
        let i1 = export
            .items
            .iter()
            .find(|i| i.id == det_id("item", "10004"))
            .unwrap();
        let i2 = export
            .items
            .iter()
            .find(|i| i.id == det_id("item", "10005"))
            .unwrap();
        let i6 = export
            .items
            .iter()
            .find(|i| i.id == det_id("item", "10009"))
            .unwrap();

        // short_id from key
        assert_eq!(i1.short_id, Some(1));
        assert_eq!(i6.short_id, Some(6));

        // null priority on MTR-2 → no priority value
        assert!(
            !i2.labels.contains_key(&det_id("label", "priority")),
            "null priority → no value"
        );
        assert_eq!(
            i1.labels.get(&det_id("label", "priority")),
            Some(&det_id("priority_value", "3")),
            "Medium priority value set"
        );

        // Type label set
        assert_eq!(
            i1.labels.get(&det_id("label", "type")),
            Some(&det_id("type_value", "Task"))
        );
        assert_eq!(
            i6.labels.get(&det_id("label", "type")),
            Some(&det_id("type_value", "Sub-task"))
        );

        // Component first-wins + warning (MTR-2 had 2)
        assert_eq!(
            i2.labels.get(&det_id("label", "component")),
            Some(&det_id("component_value", "10000")),
            "first component wins"
        );
        assert!(
            conv.warnings.iter().any(|w| w.contains("components")),
            "component first-wins warning: {:?}",
            conv.warnings
        );
        // Tags first-wins + warning (MTR-2 had 2 labels)
        assert_eq!(
            i2.labels.get(&det_id("label", "tags")),
            Some(&det_id("tag_value", "api")),
            "first label wins (api < backend)"
        );
        assert!(
            conv.warnings.iter().any(|w| w.contains("labels")),
            "tags first-wins warning: {:?}",
            conv.warnings
        );

        // story-points → Estimate (5.0 → "5", 8.0 → "8")
        let est_label = &export.labels.definitions[&det_id("label", "estimate")];
        assert_eq!(est_label.values[&det_id("estimate_value", "5")].name, "5");
        assert_eq!(
            i1.labels.get(&det_id("label", "estimate")),
            Some(&det_id("estimate_value", "5"))
        );
        assert_eq!(
            i2.labels.get(&det_id("label", "estimate")),
            Some(&det_id("estimate_value", "8"))
        );

        // fixVersion → milestone (deadline = releaseDate)
        assert_eq!(i1.milestone_id, Some(det_id("milestone", "10000")));
        assert_eq!(
            export.milestones.definitions[&det_id("milestone", "10000")]
                .deadline
                .as_deref(),
            Some("2026-09-30")
        );

        // sprint → cycle, item.cycle_id set, By Sprint view seeded
        assert_eq!(i6.cycle_id, Some(det_id("cycle", "3")));
        assert_eq!(export.cycles.len(), 1);
        assert_eq!(export.cycles[0].name, "Importer Fixture Sprint");
        assert!(
            export
                .views
                .definitions
                .values()
                .any(|v| v.name == "By Sprint"),
            "By Sprint view seeded when sprints exist"
        );
        assert!(export
            .views
            .definitions
            .values()
            .any(|v| v.name == "By Status"));

        // parent: subtask MTR-6 → MTR-2
        assert_eq!(i6.parent_id, Some(det_id("item", "10005")));

        // Links: MTR-3 Blocks MTR-2 ⇒ MTR-2.blocked_by has MTR-3
        assert_eq!(i2.blocked_by, vec![det_id("item", "10006")]);
        // MTR-1 Duplicate MTR-3 ⇒ MTR-1.duplicate_of = MTR-3
        assert_eq!(i1.duplicate_of, Some(det_id("item", "10006")));
        // Relates was dropped (no analog) and not counted as dangling (endpoint present)
        assert!(
            !conv
                .warnings
                .iter()
                .any(|w| w.contains("outside the import")),
            "no dangling-link warning: {:?}",
            conv.warnings
        );

        // assignee / created_by / dates
        assert_eq!(i1.assignee_user_ids, vec![det_id("user", "acc-1")]);
        assert_eq!(i1.created_by, Some(det_id("user", "acc-1")));
        assert_eq!(i1.due_date.as_deref(), Some("2026-07-15"));
        assert_eq!(
            i1.created_at.as_deref(),
            Some("2026-06-22T23:39:41.764+0200")
        );

        // ── Rich ADF description converted (table/panel/mention/link/code) ──────
        let dtext = i1.description_text.as_deref().unwrap();
        assert!(dtext.contains("## CI setup"), "heading: {dtext}");
        assert!(dtext.contains("**robust**"), "bold: {dtext}");
        assert!(
            dtext.contains("[wodo.dev](https://wodo.dev)"),
            "link: {dtext}"
        );
        assert!(dtext.contains("@Timo"), "mention: {dtext}");
        assert!(dtext.contains("```rust"), "code fence: {dtext}");
        assert!(dtext.contains("> 4 GB RAM"), "panel→blockquote: {dtext}");
        assert!(dtext.contains("| Stage | Time |"), "table header: {dtext}");
        assert!(dtext.contains("- Lint"), "bullet: {dtext}");
        assert!(i1.description_yjs.is_some(), "description yjs encoded");

        // ── Comment (flat, no parent) ───────────────────────────────────────────
        assert_eq!(i1.comments.len(), 1);
        let c = &i1.comments[0];
        assert_eq!(c.id, det_id("comment", "10000"));
        assert_eq!(c.author_id, det_id("user", "acc-1"));
        assert_eq!(c.author_name.as_deref(), Some("Timo"));
        assert_eq!(c.parent_id, None, "Jira comments are flat");
        assert!(c
            .content_text
            .as_deref()
            .unwrap()
            .contains("Pipeline draft"));

        // ── Attachment (size + mime set, content URL recorded) ──────────────────
        let att = export
            .attachments
            .iter()
            .find(|a| a.id == det_id("attachment", "10000"))
            .unwrap();
        assert_eq!(att.filename, "rfp-notes.txt");
        assert_eq!(att.content_type, "text/plain");
        assert_eq!(att.size_bytes, 62);
        assert_eq!(
            conv.attachment_urls.get(&att.id).map(String::as_str),
            Some("https://jira.example/attachment/content/10000")
        );

        // ── Space + users ───────────────────────────────────────────────────────
        assert_eq!(export.space.short_id_prefix.as_deref(), Some("MTR"));
        assert_eq!(export.space.name, "My test RFP");
        assert_eq!(export.space.region, "eu");
        assert!(export.users.iter().any(|u| u.id == det_id("user", "acc-1")));

        // ── PROVE IT IMPORTS ────────────────────────────────────────────────────
        let result = build_space_from_export(export, NEW_SPACE, IMPORTER, &ImportMaps::default());
        assert!(
            result.space_doc.transact().get_map("config").is_some(),
            "imported space doc has config"
        );
        assert!(result.max_short_id >= 6, "short_ids carried through import");
    }

    /// Dev smoke test against the local golden Jira sample. No-ops in CI (returns
    /// early when the fixtures are absent).
    #[test]
    fn test_golden_sample_round_trips() {
        let crate_dir = env!("CARGO_MANIFEST_DIR");
        let fixtures = std::path::Path::new(crate_dir).join("../../../tests/fixtures/jira");
        for (file, min_items) in [("mtr_issues.json", 6), ("scrum_issues.json", 6)] {
            let path = fixtures.join(file);
            if !path.exists() {
                eprintln!("golden sample absent ({path:?}); skipping {file}");
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            #[derive(Deserialize)]
            struct Search {
                #[serde(default)]
                issues: Vec<JiraIssue>,
            }
            let search: Search = serde_json::from_str(&text).unwrap();
            assert!(
                search.issues.len() >= min_items,
                "{file}: expected >={min_items} issues, got {}",
                search.issues.len()
            );
            // Project from the first issue's embedded project field.
            let project = search.issues[0].fields.project.clone();
            let conv = jira_to_space_export(&project, &search.issues);
            eprintln!(
                "golden {file}: {} items, {} attachments, {} cycles, {} warnings",
                conv.export.items.len(),
                conv.export.attachments.len(),
                conv.export.cycles.len(),
                conv.warnings.len()
            );
            for w in &conv.warnings {
                eprintln!("  warn: {w}");
            }
            assert!(conv.export.items.len() >= min_items);

            let result =
                build_space_from_export(&conv.export, NEW_SPACE, IMPORTER, &ImportMaps::default());
            assert!(result.space_doc.transact().get_map("config").is_some());
            eprintln!(
                "  import: max_short_id={}, warnings={}",
                result.max_short_id,
                result.warnings.len()
            );
        }
    }
}
