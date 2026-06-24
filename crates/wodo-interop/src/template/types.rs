//! Template JSON schema types (v2)
//!
//! These types map to the template content blob stored in regional S3
//! (metadata lives in the `templates` table). All IDs are the original UUIDs
//! from the source space — UUIDs are preserved as-is and remapped at
//! instantiation.
//!
//! v2 additions (WDO-180): `source_space_id` + `attachments` manifest on the
//! root, `attachment_ids` on items. v3 additions: `comments` on items.
//! All serde-defaulted so older payloads keep deserializing (and instantiate
//! without the newer features, as before).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Current schema version written by the serializer.
pub const TEMPLATE_SCHEMA_VERSION: u32 = 3;

/// Root template content matching schema_version 2
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateContent {
    pub schema_version: u32,
    /// Source space UUID (v2). Needed at instantiation to rewrite inline
    /// attachment URLs (`/api/spaces/{source}/attachments/{id}`) inside the
    /// Yjs blobs. Stored in the content — the `templates.source_space_id`
    /// column is `ON DELETE SET NULL` and the rewrite must outlive the
    /// source space.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_space_id: Option<String>,
    /// Attachment manifest (v2). IDs are original source-space attachment
    /// UUIDs; blobs live at `orgs/{org}/templates/{template_id}/attachments/{id}`.
    /// The serializer emits ID-only entries (metadata zeroed); the save
    /// handler enriches them from Postgres and drops unresolvable refs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<TemplateAttachment>,
    pub labels: TemplateLabels,
    pub milestones: TemplateMilestones,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cycle_config: Option<TemplateCycleConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub short_id_config: Option<TemplateShortIdConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive_config: Option<TemplateArchiveConfig>,
    pub views: TemplateViews,
    #[serde(default)]
    pub items: Vec<TemplateItem>,
    #[serde(default)]
    pub documents: Vec<TemplateDocument>,
    /// Space overview content as base64-encoded Yjs binary blob
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overview_yjs: Option<String>,
}

/// Attachment manifest entry (v2). Metadata snapshot from the `attachments`
/// table at save time; the blob is a template-owned S3 copy with the
/// template's lifecycle (no `attachments` table row, invisible to the
/// orphan sweep).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateAttachment {
    pub id: String,
    #[serde(default)]
    pub filename: String,
    #[serde(default)]
    pub content_type: String,
    #[serde(default)]
    pub size_bytes: i64,
}

/// Labels configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateLabels {
    pub order: Vec<String>,
    pub primary_label_id: Option<String>,
    pub definitions: HashMap<String, TemplateLabelDef>,
}

/// A single label definition
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TemplateLabelDef {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub icon: String,
    pub values_order: Vec<String>,
    pub values: HashMap<String, TemplateLabelValue>,
    /// Deprecated labels appear only in exports (templates skip them — they
    /// are history, not shape). `deprecated_by` is a user UUID.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deprecated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deprecated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deprecated_by: Option<String>,
}

/// A single label value
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TemplateLabelValue {
    pub id: String,
    pub name: String,
    pub color: String,
    #[serde(default)]
    pub is_completion_state: bool,
    /// Prompt shown when an item is moved to this completion state (empty/
    /// absent = don't prompt). Carried by both templates and exports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_prompt: Option<String>,
    /// Deprecated values appear only in exports (see `TemplateLabelDef`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deprecated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deprecated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deprecated_by: Option<String>,
}

/// Milestones configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateMilestones {
    pub order: Vec<String>,
    pub definitions: HashMap<String, TemplateMilestoneDef>,
}

/// A single milestone definition with date as offset from T=0
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateMilestoneDef {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Days offset from the reference date (T=0). Null if milestone has no deadline.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset_days: Option<i64>,
}

/// Cycle configuration (instances are auto-generated, not stored)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateCycleConfig {
    pub enabled: bool,
    pub pattern: String,
    pub start_day: String,
    pub prefix: String,
    pub generate_ahead: i64,
    pub retain_past: i64,
}

/// Short ID configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateShortIdConfig {
    pub prefix: String,
    pub visible: bool,
}

/// Archive configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateArchiveConfig {
    pub migration_days: i64,
}

/// Saved views configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateViews {
    pub order: Vec<String>,
    pub definitions: HashMap<String, TemplateViewDef>,
}

/// A single saved view definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateViewDef {
    pub id: String,
    pub name: String,
    pub content_type: String,
    pub view_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column_grouping: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub row_grouping: Option<String>,
    #[serde(default)]
    pub sort_order: String,
    #[serde(default)]
    pub show_archived: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filters: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zoom_level: Option<String>,
}

/// A template item with dates as offsets from T=0
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateItem {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub labels: HashMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub due_date_offset_days: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_date_offset_days: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub milestone_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cycle_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub blocked_by: Vec<String>,
    /// Item description as base64-encoded Yjs binary blob
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description_yjs: Option<String>,
    /// The item's explicit `attachments` Y.Array as ordered references into
    /// the root manifest (v2). Entries without a manifest match (attachment
    /// deleted between content edit and save) are skipped at instantiation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachment_ids: Vec<String>,
    /// Item comments (v3, opt-in via `include_comments`). Ordered: roots in
    /// `threads_order` sequence, each followed by its replies. Soft-deleted
    /// comments are excluded at save time.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub comments: Vec<TemplateComment>,
}

/// A template comment (v3). Identity-bound state is reduced to a display
/// snapshot: no `author_id` is carried — the instantiator writes the nil
/// UUID, which the comment-author backfill skips (no users row), so the
/// `author_name` snapshot is permanent and nobody in the new space owns the
/// comment (the edit/delete window is keyed to author_id → effectively
/// read-only sample content). `edited_at` is deliberately dropped: curated
/// content, not history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateComment {
    /// Original comment UUID — carried over at instantiation (comments live
    /// only in Yjs and are per-space isolated, same rule as item IDs).
    pub id: String,
    /// Display-name snapshot from the source space's comments blob.
    #[serde(default)]
    pub author_name: String,
    /// Comment body as base64-encoded Yjs binary (XmlFragment), same
    /// encoding as item descriptions. Inline image refs participate in the
    /// v2 attachment manifest and get rewritten at instantiation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_yjs: Option<String>,
    /// Seconds relative to the reference date at T00:00:00Z. Rehydrated as
    /// `start_date + offset` — preserves date, time-of-day, and thread
    /// ordering in one field. May be negative.
    pub created_at_offset_seconds: i64,
    /// Root comment ID when this is a reply; None for root comments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
}

/// Template owners-default — mirrors the doc's `owners` shape in
/// `data.yjs documents.{id}.owners`. Pre-fills the doc with this set
/// on creation; "creator" is a sentinel string the instantiator resolves
/// to the creating user's UUID at instantiation time. WDO-124 B6.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TemplateOwnersDefault {
    /// Either explicit UUIDs or the sentinel "creator" (case-sensitive)
    /// to mean "the user creating the space from this template".
    #[serde(default)]
    pub users: Vec<String>,
    #[serde(default)]
    pub teams: Vec<String>,
}

/// A template document.
///
/// WDO-124 Milestone B6 extends the struct with the B-phase primitives so
/// the ~10 default templates shipped with the Minimal space template can
/// declare cadence, ownership-default, and `is_template = true` directly.
/// All new fields are optional / default-able so pre-B6 template JSONB
/// payloads continue to deserialize.
///
/// **What this struct deliberately does NOT carry** (identity-bound state
/// that wouldn't make sense in a new space cloned from this template):
///
/// - `owners.users / owners.teams` (user UUIDs) — preserved structurally
///   via `owners_default` with the `"creator"` sentinel, not by copying
///   actual UUIDs. A new space lives in a different org with different
///   members.
/// - `last_reviewed_at` — the original doc's review stamp would lie about
///   freshness in the new doc; baseline starts fresh from `created_at`.
/// - `forked_from` — points at a doc in the original space; would dangle
///   in the new space.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateDocument {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub labels: HashMap<String, String>,
    /// Document content as base64-encoded Yjs binary blob
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_yjs: Option<String>,

    /// Pre-filled owner set at instantiation. `users` may contain the
    /// sentinel string "creator" (resolved to the creating user's UUID).
    #[serde(default, skip_serializing_if = "is_default_owners")]
    pub owners_default: TemplateOwnersDefault,

    /// Cadence in days (e.g. 90 for a Runbook). `None` = no cadence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_cadence_days: Option<i32>,

    /// `true` for templates living in the Templates tab of a space.
    /// Pre-B6 payloads default to `false`.
    #[serde(default)]
    pub is_template: bool,

    /// Parent item UUID, when the doc is anchored to a unit of work
    /// (WDO-124 B1). Item UUIDs survive template instantiation verbatim
    /// (Yjs blob isolation — see `instantiator.rs` module doc), so this
    /// reference resolves correctly in the new space without remapping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_item_id: Option<String>,

    /// Parent milestone UUID, same reasoning as `parent_item_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_milestone_id: Option<String>,
}

fn is_default_owners(o: &TemplateOwnersDefault) -> bool {
    o.users.is_empty() && o.teams.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_template_content_roundtrip() {
        let content = TemplateContent {
            schema_version: 1,
            source_space_id: None,
            attachments: vec![],
            labels: TemplateLabels {
                order: vec!["label-1".into()],
                primary_label_id: Some("label-1".into()),
                definitions: {
                    let mut m = HashMap::new();
                    m.insert(
                        "label-1".into(),
                        TemplateLabelDef {
                            id: "label-1".into(),
                            name: "Status".into(),
                            description: String::new(),
                            icon: "circle".into(),
                            values_order: vec!["val-1".into()],
                            values: {
                                let mut v = HashMap::new();
                                v.insert(
                                    "val-1".into(),
                                    TemplateLabelValue {
                                        id: "val-1".into(),
                                        name: "Ready".into(),
                                        color: "#3B82F6".into(),
                                        is_completion_state: false,
                                        ..Default::default()
                                    },
                                );
                                v
                            },
                            ..Default::default()
                        },
                    );
                    m
                },
            },
            milestones: TemplateMilestones {
                order: vec![],
                definitions: HashMap::new(),
            },
            cycle_config: None,
            short_id_config: Some(TemplateShortIdConfig {
                prefix: "PRJ".into(),
                visible: true,
            }),
            archive_config: None,
            views: TemplateViews {
                order: vec![],
                definitions: HashMap::new(),
            },
            items: vec![],
            documents: vec![],
            overview_yjs: None,
        };

        let json = serde_json::to_string(&content).unwrap();
        let deserialized: TemplateContent = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.schema_version, 1);
        assert_eq!(deserialized.labels.order.len(), 1);
        assert_eq!(deserialized.labels.definitions["label-1"].name, "Status");
    }

    #[test]
    fn test_minimal_template_content() {
        let content = TemplateContent {
            schema_version: 1,
            source_space_id: None,
            attachments: vec![],
            labels: TemplateLabels {
                order: vec![],
                primary_label_id: None,
                definitions: HashMap::new(),
            },
            milestones: TemplateMilestones {
                order: vec![],
                definitions: HashMap::new(),
            },
            cycle_config: None,
            short_id_config: None,
            archive_config: None,
            views: TemplateViews {
                order: vec![],
                definitions: HashMap::new(),
            },
            items: vec![],
            documents: vec![],
            overview_yjs: None,
        };

        let json = serde_json::to_string(&content).unwrap();
        let deserialized: TemplateContent = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.schema_version, 1);
        assert!(deserialized.items.is_empty());
        assert!(deserialized.documents.is_empty());
    }

    #[test]
    fn test_item_with_relationships() {
        let item = TemplateItem {
            id: "item-1".into(),
            title: "Example task".into(),
            labels: {
                let mut m = HashMap::new();
                m.insert("label-1".into(), "val-1".into());
                m
            },
            due_date_offset_days: Some(7),
            start_date_offset_days: Some(0),
            milestone_id: Some("ms-1".into()),
            cycle_id: None,
            parent_id: None,
            blocked_by: vec!["item-2".into()],
            description_yjs: Some("base64encodedblob".into()),
            attachment_ids: vec![],
            comments: vec![],
        };

        let json = serde_json::to_string(&item).unwrap();
        let deserialized: TemplateItem = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.blocked_by, vec!["item-2"]);
        assert_eq!(deserialized.due_date_offset_days, Some(7));
    }
}
