//! Space export JSON types (v2)
//!
//! These types represent a complete snapshot of a space's data for data portability.
//! Unlike templates (which use date offsets and exclude archived items), exports
//! preserve absolute dates and include all data including archived items and comments.
//!
//! v2 additions: an `attachments` metadata array (join key to the attachments ZIP,
//! whose entries are named `attachments/{uuid}/{filename}`), `users`/`teams` snapshot
//! manifests for every referenced UUID, and full-fidelity comments (`content_yjs`,
//! `author_name`, `edited_at`, `deleted_at`).

use crate::template::types::{
    TemplateArchiveConfig, TemplateCycleConfig, TemplateLabels, TemplateViews,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Root export structure — complete space snapshot
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpaceExport {
    pub format: String,
    pub exported_at: String,
    pub space: ExportSpaceMetadata,
    pub labels: TemplateLabels,
    pub milestones: ExportMilestones,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cycle_config: Option<TemplateCycleConfig>,
    pub cycles: Vec<ExportCycle>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive_config: Option<TemplateArchiveConfig>,
    pub views: TemplateViews,
    pub items: Vec<ExportItem>,
    pub documents: Vec<ExportDocument>,
    /// Postgres-sourced; filled by the caller after serialization (the
    /// serializer itself is Yjs-only). Same for `users` and `teams`.
    pub attachments: Vec<ExportAttachment>,
    pub users: Vec<ExportUser>,
    pub teams: Vec<ExportTeam>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overview_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overview_yjs: Option<String>,
}

/// Space metadata — Postgres fields provided by caller, Yjs fields filled by serializer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportSpaceMetadata {
    pub id: String,
    pub name: String,
    pub slug: String,
    pub region: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short_id_prefix: Option<String>,
    pub short_id_visible: bool,
    pub created_at: String,
}

/// Caller-provided space metadata (from Postgres). The serializer adds Yjs-derived
/// fields (short_id_prefix, short_id_visible) to produce the final ExportSpaceMetadata.
#[derive(Debug, Clone)]
pub struct ExportSpaceMetadataInput {
    pub id: String,
    pub name: String,
    pub slug: String,
    pub region: String,
    pub created_at: String,
}

/// Milestones with absolute dates (not offsets)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportMilestones {
    pub order: Vec<String>,
    pub definitions: HashMap<String, ExportMilestoneDef>,
}

/// A single milestone definition with absolute deadline
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportMilestoneDef {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<String>,
    pub deprecated: bool,
}

/// A cycle instance (not just config)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportCycle {
    pub id: String,
    pub name: String,
    pub start_date: String,
    pub end_date: String,
    pub archived: bool,
}

/// An exported item with absolute dates, assignees, all relationships
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExportItem {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short_id: Option<i64>,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description_yjs: Option<String>,
    #[serde(default)]
    pub labels: HashMap<String, String>,
    #[serde(default)]
    pub assignee_user_ids: Vec<String>,
    #[serde(default)]
    pub assignee_team_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due_date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub milestone_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cycle_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub blocked_by: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duplicate_of: Option<String>,
    pub archived: bool,
    pub deep_archived: bool,
    /// Provenance — preserved on import when present (older exports lack
    /// these; the importer + import time are used instead). `archived_at` is
    /// normalized to RFC 3339 (stored internally as epoch millis).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<String>,
    /// Completion snapshot: the prompt text at the moment the item was
    /// completed, and the note the user entered (dual form like descriptions).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_note_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_note_yjs: Option<String>,
    #[serde(default)]
    pub comments: Vec<ExportComment>,
}

/// A comment on an item.
///
/// `content_text` is a plain-text extraction for human/tooling readability;
/// `content_yjs` is the authoritative XmlFragment as a base64 Yjs update
/// (same dual form as item descriptions). `author_name` is the snapshot
/// maintained in the comments blob by the author-name backfill, frozen at
/// export time. Soft-deleted comments keep their content (v1 behavior).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportComment {
    pub id: String,
    pub author_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_yjs: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edited_at: Option<String>,
    pub deleted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
}

/// Attachment metadata from the Postgres `attachments` table. The `id` is the
/// join key to the attachments ZIP, whose entries are named
/// `attachments/{id}/{filename}`. The two artifacts are exported at different
/// moments — importers must tolerate refs missing from the ZIP and vice versa.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportAttachment {
    pub id: String,
    pub filename: String,
    pub content_type: String,
    pub size_bytes: i64,
    pub uploaded_by: String,
    pub uploaded_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document_id: Option<String>,
    /// True when the attachment is no longer referenced by any content and is
    /// pending deletion by the orphan sweep.
    pub orphaned: bool,
}

/// Snapshot of a referenced user (assignee, comment author, or uploader) at
/// export time. Tombstoned accounts export as "Former user" with no email;
/// hard-deleted users are simply absent (importers fall back to the
/// per-comment `author_name` snapshot).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportUser {
    pub id: String,
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

/// Snapshot of a referenced team at export time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportTeam {
    pub id: String,
    pub name: String,
}

/// An exported document with content and full metadata
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExportDocument {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub labels: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_yjs: Option<String>,
    pub archived: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub owner_user_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub owner_team_ids: Vec<String>,
    /// At most one of the two parent anchors is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_item_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_milestone_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_cadence_days: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reviewed_at: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_template: bool,
    /// Lineage: source document UUID (within the same export). Remapped to
    /// the regenerated document UUID on import; dropped if the source is
    /// absent from the export.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<String>,
}
