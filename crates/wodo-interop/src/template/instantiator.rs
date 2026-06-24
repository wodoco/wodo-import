//! Template instantiation: TemplateContent → Yjs documents
//!
//! Converts a template's JSON content into a well-formed SpaceData Y.Doc and
//! separate document Y.Docs, with rehydrated dates.
//!
//! Item UUIDs are preserved — each space is an isolated Yjs document, so item
//! UUIDs only need to be unique within their own space. Document UUIDs are
//! regenerated because they need Postgres rows (`documents` table PK), and
//! multiple spaces instantiated from the same template must not collide.
//! All references to document UUIDs in rich text blobs (docEmbed nodeviews)
//! are remapped via byte-level replacement during decode.

use super::types::*;
use base64::Engine;
use chrono::{NaiveDate, Utc};
use std::collections::HashMap;
use std::sync::Arc;
use yrs::types::map::MapPrelim;
use yrs::updates::decoder::Decode;
use yrs::{
    Any, Array, ArrayPrelim, Doc, Map, ReadTxn, Transact, Update, WriteTxn, XmlFragmentPrelim,
};

/// A document entry with its new (regenerated) UUID and title.
///
/// Used by the hydration handler to create Postgres rows with fresh UUIDs
/// that won't collide with the source space's documents.
pub struct DocumentManifestEntry {
    pub new_id: String,
    pub title: String,
}

/// An attachment to copy into the new space (WDO-180). The hydration handler
/// copies the blob from the template's S3 prefix to the new space's
/// attachment key and inserts the `attachments` Postgres row.
pub struct AttachmentManifestEntry {
    /// Original attachment UUID (key under the template's S3 prefix)
    pub old_id: String,
    /// Fresh UUID for the new space's copy (already substituted into the
    /// instantiated Yjs blobs' inline image URLs)
    pub new_id: String,
    pub filename: String,
    pub content_type: String,
    pub size_bytes: i64,
}

/// Result of template instantiation
pub struct InstantiationResult {
    /// The main SpaceData Yjs document (contains items, documents metadata, config)
    pub space_doc: Doc,
    /// Separate Yjs documents for each document's content, keyed by new document ID
    pub document_docs: Vec<(String, Doc)>,
    /// Manifest of documents with regenerated UUIDs (for Postgres INSERT)
    pub document_manifest: Vec<DocumentManifestEntry>,
    /// Manifest of attachments to copy (for S3 copy + Postgres INSERT)
    pub attachment_manifest: Vec<AttachmentManifestEntry>,
    /// The comments.yjs doc, when any item carries template comments (v3).
    /// Persisted by the hydration handler to the space's comments blob.
    pub comments_doc: Option<Doc>,
}

/// Instantiate a template into Yjs documents.
///
/// # Arguments
/// * `content` - The template content (from the S3 content blob)
/// * `start_date` - T=0 anchor for rehydrating date offsets
/// * `short_id_prefix` - Override for the space's short ID prefix (or use template's)
/// * `created_by` - UUID of the user creating the space
/// * `new_space_id` - UUID of the space being created; combined with the
///   template's `source_space_id` to rewrite inline attachment URLs
///   (`/api/spaces/{source}/attachments/{id}`) in the Yjs blobs. `None`
///   (or a v1 template without `source_space_id`) skips space-ID rewriting.
pub fn instantiate_template(
    content: &TemplateContent,
    start_date: NaiveDate,
    short_id_prefix: Option<&str>,
    created_by: &str,
    new_space_id: Option<&str>,
) -> InstantiationResult {
    // Build old→new UUID map for documents. Items keep their original UUIDs
    // (they only live in Yjs), but documents need fresh UUIDs for Postgres PKs.
    let doc_id_map: HashMap<String, String> = content
        .documents
        .iter()
        .map(|doc| (doc.id.clone(), uuid::Uuid::new_v4().to_string()))
        .collect();

    // Fresh UUIDs for template attachments — the copied blobs get new
    // identities in the new space (WDO-180)
    let attachment_id_map: HashMap<String, String> = content
        .attachments
        .iter()
        .map(|a| (a.id.clone(), uuid::Uuid::new_v4().to_string()))
        .collect();

    // Combined byte-substitution map for the rich text blobs: docEmbed
    // UUIDs, attachment UUIDs, and the source space UUID (the latter two
    // rewrite inline `/api/spaces/{space}/attachments/{id}` image URLs).
    // Attachment refs that didn't make the save-time manifest keep their
    // UUID but get the new space ID → a clean 404 in the new space instead
    // of a silent cross-space dependency.
    let mut blob_uuid_map = doc_id_map.clone();
    blob_uuid_map.extend(attachment_id_map.clone());
    if let (Some(src_space), Some(new_space)) = (content.source_space_id.as_deref(), new_space_id) {
        if src_space != new_space {
            blob_uuid_map.insert(src_space.to_string(), new_space.to_string());
        }
    }

    let space_doc = Doc::new();
    {
        let mut txn = space_doc.transact_mut();

        // Config map
        let config = txn.get_or_insert_map("config");

        // Labels
        build_labels(&mut txn, &config, &content.labels);

        // Milestones
        build_milestones(&mut txn, &config, content, start_date);

        // Cycle config
        build_cycle_config(&mut txn, &config, content.cycle_config.as_ref());

        // Short ID config
        let prefix = short_id_prefix
            .or(content.short_id_config.as_ref().map(|c| c.prefix.as_str()))
            .unwrap_or("PRJ");
        config.insert(
            &mut txn,
            Arc::<str>::from("short_id_prefix"),
            Any::String(Arc::from(prefix)),
        );
        let visible = content
            .short_id_config
            .as_ref()
            .map(|c| c.visible)
            .unwrap_or(true);
        config.insert(
            &mut txn,
            Arc::<str>::from("short_id_visible"),
            Any::Bool(visible),
        );

        // Archive config
        if let Some(ac) = &content.archive_config {
            let ac_map: yrs::MapRef =
                config.insert(&mut txn, "archive_config", MapPrelim::default());
            ac_map.insert(
                &mut txn,
                Arc::<str>::from("migration_days"),
                Any::Number(ac.migration_days as f64),
            );
        }

        // Views
        build_views(&mut txn, &config, &content.views, &clean_filters);

        // Items (descriptions may contain docEmbed/attachment refs → remap)
        build_items(
            &mut txn,
            content,
            start_date,
            created_by,
            &blob_uuid_map,
            &attachment_id_map,
        );

        // Documents (metadata only — content is in separate docs, uses new UUIDs)
        build_documents(&mut txn, content, created_by, &doc_id_map);

        // Overview — restore from binary blob (may contain docEmbed/attachment refs → remap)
        if let Some(ref overview_b64) = content.overview_yjs {
            let frag: yrs::XmlFragmentRef = txn.get_or_insert_xml_fragment("description");
            let _ = decode_base64_to_xml_fragment(
                overview_b64,
                "content",
                &mut txn,
                &frag,
                &blob_uuid_map,
            );
        }
    }

    // Build separate document Yjs docs from binary blobs (keyed by new UUID)
    let document_docs = content
        .documents
        .iter()
        .filter_map(|doc| {
            let content_b64 = doc.content_yjs.as_deref()?;
            if content_b64.is_empty() {
                return None;
            }
            let new_id = doc_id_map.get(&doc.id)?;
            let doc_yjs = decode_base64_to_doc(content_b64, &blob_uuid_map).ok()?;
            Some((new_id.clone(), doc_yjs))
        })
        .collect();

    // Build manifest for Postgres INSERTs
    let document_manifest = content
        .documents
        .iter()
        .filter_map(|doc| {
            let new_id = doc_id_map.get(&doc.id)?;
            Some(DocumentManifestEntry {
                new_id: new_id.clone(),
                title: doc.title.clone(),
            })
        })
        .collect();

    // Manifest of attachment blobs for the hydration handler to copy
    let attachment_manifest = content
        .attachments
        .iter()
        .filter_map(|a| {
            let new_id = attachment_id_map.get(&a.id)?;
            Some(AttachmentManifestEntry {
                old_id: a.id.clone(),
                new_id: new_id.clone(),
                filename: a.filename.clone(),
                content_type: a.content_type.clone(),
                size_bytes: a.size_bytes,
            })
        })
        .collect();

    // Comments doc (v3) — built when any item carries template comments
    let comments_doc = build_comments_doc(content, start_date, &blob_uuid_map);

    InstantiationResult {
        space_doc,
        document_docs,
        document_manifest,
        attachment_manifest,
        comments_doc,
    }
}

// ─── Labels ──────────────────────────────────────────────────────────────────

pub(crate) fn build_labels(
    txn: &mut yrs::TransactionMut,
    config: &yrs::MapRef,
    labels: &TemplateLabels,
) {
    let labels_map: yrs::MapRef = config.insert(txn, "labels", MapPrelim::default());
    let labels_order: yrs::ArrayRef = config.insert(txn, "labels_order", ArrayPrelim::default());

    for id in &labels.order {
        if let Some(label_def) = labels.definitions.get(id) {
            let label: yrs::MapRef =
                labels_map.insert(txn, label_def.id.as_str(), MapPrelim::default());

            label.insert(
                txn,
                Arc::<str>::from("id"),
                Any::String(Arc::from(label_def.id.as_str())),
            );
            label.insert(
                txn,
                Arc::<str>::from("name"),
                Any::String(Arc::from(label_def.name.as_str())),
            );
            label.insert(
                txn,
                Arc::<str>::from("description"),
                Any::String(Arc::from(label_def.description.as_str())),
            );
            label.insert(
                txn,
                Arc::<str>::from("icon"),
                Any::String(Arc::from(label_def.icon.as_str())),
            );

            // Values
            let values: yrs::MapRef = label.insert(txn, "values", MapPrelim::default());
            let values_order: yrs::ArrayRef =
                label.insert(txn, "values_order", ArrayPrelim::default());

            for val_id in &label_def.values_order {
                if let Some(val_def) = label_def.values.get(val_id) {
                    let val: yrs::MapRef =
                        values.insert(txn, val_def.id.as_str(), MapPrelim::default());

                    val.insert(
                        txn,
                        Arc::<str>::from("id"),
                        Any::String(Arc::from(val_def.id.as_str())),
                    );
                    val.insert(
                        txn,
                        Arc::<str>::from("name"),
                        Any::String(Arc::from(val_def.name.as_str())),
                    );
                    val.insert(
                        txn,
                        Arc::<str>::from("color"),
                        Any::String(Arc::from(val_def.color.as_str())),
                    );
                    if val_def.is_completion_state {
                        val.insert(
                            txn,
                            Arc::<str>::from("is_completion_state"),
                            Any::Bool(true),
                        );
                    }
                    if let Some(ref prompt) = val_def.completion_prompt {
                        val.insert(
                            txn,
                            Arc::<str>::from("completion_prompt"),
                            Any::String(Arc::from(prompt.as_str())),
                        );
                    }
                    write_deprecation(
                        txn,
                        &val,
                        val_def.deprecated,
                        val_def.deprecated_at.as_deref(),
                        val_def.deprecated_by.as_deref(),
                    );

                    values_order.push_back(txn, Any::String(Arc::from(val_def.id.as_str())));
                }
            }

            write_deprecation(
                txn,
                &label,
                label_def.deprecated,
                label_def.deprecated_at.as_deref(),
                label_def.deprecated_by.as_deref(),
            );

            labels_order.push_back(txn, Any::String(Arc::from(label_def.id.as_str())));
        }
    }

    // Primary label ID
    if let Some(ref primary) = labels.primary_label_id {
        config.insert(
            txn,
            Arc::<str>::from("primary_label_id"),
            Any::String(Arc::from(primary.as_str())),
        );
    }
}

/// Write deprecation state on a label or label-value Y.Map (only present in
/// exports — templates never carry deprecated entries).
fn write_deprecation(
    txn: &mut yrs::TransactionMut,
    map: &yrs::MapRef,
    deprecated: bool,
    deprecated_at: Option<&str>,
    deprecated_by: Option<&str>,
) {
    if !deprecated {
        return;
    }
    map.insert(txn, Arc::<str>::from("deprecated"), Any::Bool(true));
    if let Some(at) = deprecated_at {
        map.insert(
            txn,
            Arc::<str>::from("deprecated_at"),
            Any::String(Arc::from(at)),
        );
    }
    if let Some(by) = deprecated_by {
        map.insert(
            txn,
            Arc::<str>::from("deprecated_by"),
            Any::String(Arc::from(by)),
        );
    }
}

// ─── Milestones ──────────────────────────────────────────────────────────────

fn build_milestones(
    txn: &mut yrs::TransactionMut,
    config: &yrs::MapRef,
    content: &TemplateContent,
    start_date: NaiveDate,
) {
    let ms_map: yrs::MapRef = config.insert(txn, "milestones", MapPrelim::default());
    let ms_order: yrs::ArrayRef = config.insert(txn, "milestones_order", ArrayPrelim::default());

    for id in &content.milestones.order {
        if let Some(ms_def) = content.milestones.definitions.get(id) {
            let ms: yrs::MapRef = ms_map.insert(txn, ms_def.id.as_str(), MapPrelim::default());

            ms.insert(
                txn,
                Arc::<str>::from("id"),
                Any::String(Arc::from(ms_def.id.as_str())),
            );
            ms.insert(
                txn,
                Arc::<str>::from("name"),
                Any::String(Arc::from(ms_def.name.as_str())),
            );

            if let Some(ref desc) = ms_def.description {
                ms.insert(
                    txn,
                    Arc::<str>::from("description"),
                    Any::String(Arc::from(desc.as_str())),
                );
            }

            // Rehydrate deadline from offset
            if let Some(offset) = ms_def.offset_days {
                let deadline = start_date + chrono::Duration::days(offset);
                ms.insert(
                    txn,
                    Arc::<str>::from("deadline"),
                    Any::String(Arc::from(deadline.format("%Y-%m-%d").to_string())),
                );
            }

            ms_order.push_back(txn, Any::String(Arc::from(ms_def.id.as_str())));
        }
    }
}

// ─── Cycle Config ────────────────────────────────────────────────────────────

pub(crate) fn build_cycle_config(
    txn: &mut yrs::TransactionMut,
    config: &yrs::MapRef,
    cycle_config: Option<&TemplateCycleConfig>,
) {
    if let Some(cc) = cycle_config {
        let cc_map: yrs::MapRef = config.insert(txn, "cycle_config", MapPrelim::default());
        cc_map.insert(txn, Arc::<str>::from("enabled"), Any::Bool(cc.enabled));
        cc_map.insert(
            txn,
            Arc::<str>::from("pattern"),
            Any::String(Arc::from(cc.pattern.as_str())),
        );
        cc_map.insert(
            txn,
            Arc::<str>::from("start_day"),
            Any::String(Arc::from(cc.start_day.as_str())),
        );
        cc_map.insert(
            txn,
            Arc::<str>::from("prefix"),
            Any::String(Arc::from(cc.prefix.as_str())),
        );
        cc_map.insert(
            txn,
            Arc::<str>::from("generate_ahead"),
            Any::Number(cc.generate_ahead as f64),
        );
        cc_map.insert(
            txn,
            Arc::<str>::from("retain_past"),
            Any::Number(cc.retain_past as f64),
        );
        // next_number resets to 1
        cc_map.insert(txn, Arc::<str>::from("next_number"), Any::Number(1.0));
    }
}

// ─── Views ───────────────────────────────────────────────────────────────────

/// Build saved views. `filter_transform` decides what happens to each view's
/// filter string: template instantiation passes `clean_filters` (user/team/
/// cycle filters don't apply to a fresh space), the import passes a
/// remapping transform (those filters DO survive an import — see
/// `import::remap_view_filters`). Returning None drops the filters entirely.
pub(crate) fn build_views(
    txn: &mut yrs::TransactionMut,
    config: &yrs::MapRef,
    views: &TemplateViews,
    filter_transform: &dyn Fn(&str) -> Option<String>,
) {
    let views_map: yrs::MapRef = config.insert(txn, "shared_views", MapPrelim::default());
    let views_order: yrs::ArrayRef =
        config.insert(txn, "shared_views_order", ArrayPrelim::default());

    for id in &views.order {
        if let Some(view_def) = views.definitions.get(id) {
            let view: yrs::MapRef =
                views_map.insert(txn, view_def.id.as_str(), MapPrelim::default());

            view.insert(
                txn,
                Arc::<str>::from("id"),
                Any::String(Arc::from(view_def.id.as_str())),
            );
            view.insert(
                txn,
                Arc::<str>::from("name"),
                Any::String(Arc::from(view_def.name.as_str())),
            );
            view.insert(
                txn,
                Arc::<str>::from("content_type"),
                Any::String(Arc::from(view_def.content_type.as_str())),
            );
            view.insert(
                txn,
                Arc::<str>::from("view_type"),
                Any::String(Arc::from(view_def.view_type.as_str())),
            );

            // Column/row grouping — preserved as-is (references label IDs)
            if let Some(ref cg) = view_def.column_grouping {
                view.insert(
                    txn,
                    Arc::<str>::from("column_grouping"),
                    Any::String(Arc::from(cg.as_str())),
                );
            }
            if let Some(ref rg) = view_def.row_grouping {
                view.insert(
                    txn,
                    Arc::<str>::from("row_grouping"),
                    Any::String(Arc::from(rg.as_str())),
                );
            }

            view.insert(
                txn,
                Arc::<str>::from("sort_order"),
                Any::String(Arc::from(view_def.sort_order.as_str())),
            );
            view.insert(
                txn,
                Arc::<str>::from("show_archived"),
                Any::Bool(view_def.show_archived),
            );

            // Filters — through the caller's transform
            if let Some(ref filters) = view_def.filters {
                if let Some(cleaned) = filter_transform(filters) {
                    view.insert(
                        txn,
                        Arc::<str>::from("filters"),
                        Any::String(Arc::from(cleaned.as_str())),
                    );
                }
            }

            if let Some(ref zl) = view_def.zoom_level {
                view.insert(
                    txn,
                    Arc::<str>::from("zoom_level"),
                    Any::String(Arc::from(zl.as_str())),
                );
            }

            views_order.push_back(txn, Any::String(Arc::from(view_def.id.as_str())));
        }
    }
}

/// Clean a view filter string by removing filters that don't apply to a new space.
///
/// - Cycle filters (`cy:...`) — dropped (cycles are auto-generated fresh)
/// - User filters (`u:...`) — dropped (different users in new space)
/// - Team filters (`t:...`) — dropped (different teams in new space)
/// - All other filters preserved as-is (label, milestone, date, parent)
fn clean_filters(filter_str: &str) -> Option<String> {
    if filter_str.is_empty() {
        return None;
    }

    let kept: Vec<&str> = filter_str
        .split(',')
        .map(|p| p.trim())
        .filter(|p| {
            !p.is_empty() && !p.starts_with("cy:") && !p.starts_with("u:") && !p.starts_with("t:")
        })
        .collect();

    if kept.is_empty() {
        None
    } else {
        Some(kept.join(","))
    }
}

// ─── Items ───────────────────────────────────────────────────────────────────

fn build_items(
    txn: &mut yrs::TransactionMut,
    content: &TemplateContent,
    start_date: NaiveDate,
    created_by: &str,
    blob_uuid_map: &HashMap<String, String>,
    attachment_id_map: &HashMap<String, String>,
) {
    // Manifest metadata by original attachment ID, for rebuilding the
    // items' explicit attachments Y.Arrays
    let manifest_by_old: HashMap<&str, &TemplateAttachment> = content
        .attachments
        .iter()
        .map(|a| (a.id.as_str(), a))
        .collect();
    let items_map = txn.get_or_insert_map("items");
    let items_order: yrs::ArrayRef = txn.get_or_insert_array("items_order");
    let now = Utc::now().to_rfc3339();

    for template_item in &content.items {
        let id = &template_item.id;
        let item: yrs::MapRef = items_map.insert(txn, id.as_str(), MapPrelim::default());

        item.insert(
            txn,
            Arc::<str>::from("id"),
            Any::String(Arc::from(id.as_str())),
        );
        item.insert(
            txn,
            Arc::<str>::from("title"),
            Any::String(Arc::from(template_item.title.as_str())),
        );

        // Labels
        let labels: yrs::MapRef = item.insert(txn, "labels", MapPrelim::default());
        for (label_id, value_id) in &template_item.labels {
            labels.insert(
                txn,
                Arc::<str>::from(label_id.as_str()),
                Any::String(Arc::from(value_id.as_str())),
            );
        }

        // Dates — rehydrate from offsets
        if let Some(offset) = template_item.due_date_offset_days {
            let date = start_date + chrono::Duration::days(offset);
            item.insert(
                txn,
                Arc::<str>::from("due_date"),
                Any::String(Arc::from(date.format("%Y-%m-%d").to_string())),
            );
        }
        if let Some(offset) = template_item.start_date_offset_days {
            let date = start_date + chrono::Duration::days(offset);
            item.insert(
                txn,
                Arc::<str>::from("start_date"),
                Any::String(Arc::from(date.format("%Y-%m-%d").to_string())),
            );
        }

        // Milestone
        if let Some(ref ms_id) = template_item.milestone_id {
            item.insert(
                txn,
                Arc::<str>::from("milestone_id"),
                Any::String(Arc::from(ms_id.as_str())),
            );
        }

        // Cycle
        if let Some(ref cycle_id) = template_item.cycle_id {
            item.insert(
                txn,
                Arc::<str>::from("cycle_id"),
                Any::String(Arc::from(cycle_id.as_str())),
            );
        }

        // Parent
        if let Some(ref parent_id) = template_item.parent_id {
            item.insert(
                txn,
                Arc::<str>::from("parent_id"),
                Any::String(Arc::from(parent_id.as_str())),
            );
        }

        // Blocked by
        if !template_item.blocked_by.is_empty() {
            let blocked_by: yrs::ArrayRef = item.insert(txn, "blocked_by", ArrayPrelim::default());
            for blocker_id in &template_item.blocked_by {
                blocked_by.push_back(txn, Any::String(Arc::from(blocker_id.as_str())));
            }
        }

        // Description — restore from binary blob (remap doc/attachment/space
        // UUIDs in embeds and inline image URLs)
        if let Some(ref desc_b64) = template_item.description_yjs {
            let desc: yrs::XmlFragmentRef =
                item.insert(txn, "description", XmlFragmentPrelim::default());
            let _ = decode_base64_to_xml_fragment(desc_b64, "content", txn, &desc, blob_uuid_map);
        }

        // Explicit attachments Y.Array — rebuilt from the manifest with
        // remapped IDs, in the item's original order. Refs without a
        // manifest match (deleted before save) are skipped.
        let resolved: Vec<&TemplateAttachment> = template_item
            .attachment_ids
            .iter()
            .filter_map(|old_id| manifest_by_old.get(old_id.as_str()).copied())
            .collect();
        if !resolved.is_empty() {
            let attachments: yrs::ArrayRef =
                item.insert(txn, "attachments", ArrayPrelim::default());
            for entry in resolved {
                let Some(new_id) = attachment_id_map.get(&entry.id) else {
                    continue;
                };
                let amap: yrs::MapRef = attachments.push_back(txn, MapPrelim::default());
                amap.insert(
                    txn,
                    Arc::<str>::from("id"),
                    Any::String(Arc::from(new_id.as_str())),
                );
                amap.insert(
                    txn,
                    Arc::<str>::from("filename"),
                    Any::String(Arc::from(entry.filename.as_str())),
                );
                amap.insert(
                    txn,
                    Arc::<str>::from("content_type"),
                    Any::String(Arc::from(entry.content_type.as_str())),
                );
                amap.insert(
                    txn,
                    Arc::<str>::from("size_bytes"),
                    Any::Number(entry.size_bytes as f64),
                );
            }
        }

        // Metadata fields
        item.insert(
            txn,
            Arc::<str>::from("created_by"),
            Any::String(Arc::from(created_by)),
        );
        item.insert(
            txn,
            Arc::<str>::from("created_at"),
            Any::String(Arc::from(now.as_str())),
        );
        item.insert(
            txn,
            Arc::<str>::from("updated_at"),
            Any::String(Arc::from(now.as_str())),
        );

        items_order.push_back(txn, Any::String(Arc::from(id.as_str())));
    }
}

// ─── Documents ───────────────────────────────────────────────────────────────

fn build_documents(
    txn: &mut yrs::TransactionMut,
    content: &TemplateContent,
    created_by: &str,
    doc_id_map: &HashMap<String, String>,
) {
    let docs_map = txn.get_or_insert_map("documents");
    let docs_order: yrs::ArrayRef = txn.get_or_insert_array("documents_order");
    let now = Utc::now().to_rfc3339();

    for template_doc in &content.documents {
        let new_id = match doc_id_map.get(&template_doc.id) {
            Some(id) => id.as_str(),
            None => continue,
        };
        let doc: yrs::MapRef = docs_map.insert(txn, new_id, MapPrelim::default());

        doc.insert(txn, Arc::<str>::from("id"), Any::String(Arc::from(new_id)));
        doc.insert(
            txn,
            Arc::<str>::from("title"),
            Any::String(Arc::from(template_doc.title.as_str())),
        );

        // Labels
        let labels: yrs::MapRef = doc.insert(txn, "labels", MapPrelim::default());
        for (label_id, value_id) in &template_doc.labels {
            labels.insert(
                txn,
                Arc::<str>::from(label_id.as_str()),
                Any::String(Arc::from(value_id.as_str())),
            );
        }

        // Metadata
        doc.insert(
            txn,
            Arc::<str>::from("created_by"),
            Any::String(Arc::from(created_by)),
        );
        doc.insert(
            txn,
            Arc::<str>::from("created_at"),
            Any::String(Arc::from(now.as_str())),
        );
        doc.insert(
            txn,
            Arc::<str>::from("updated_at"),
            Any::String(Arc::from(now.as_str())),
        );

        // WDO-124 Milestone B6 — write the doc primitive defaults from the
        // template. Mirrors `client/src/collab/document.gleam :: init_document_ymap`
        // so a doc instantiated from a template looks identical to a doc
        // created fresh through the Yjs client API.
        let owners_map: yrs::MapRef = doc.insert(txn, "owners", MapPrelim::default());
        let users_arr: yrs::ArrayRef = owners_map.insert(txn, "users", yrs::ArrayPrelim::default());
        for user_ref in &template_doc.owners_default.users {
            // Resolve the `creator` sentinel to the user creating the space.
            let resolved = if user_ref == "creator" {
                created_by
            } else {
                user_ref.as_str()
            };
            users_arr.push_back(txn, Any::String(Arc::from(resolved)));
        }
        let teams_arr: yrs::ArrayRef = owners_map.insert(txn, "teams", yrs::ArrayPrelim::default());
        for team_ref in &template_doc.owners_default.teams {
            teams_arr.push_back(txn, Any::String(Arc::from(team_ref.as_str())));
        }

        // Parent references survive because item / milestone UUIDs are
        // preserved verbatim through instantiation (see module doc-comment).
        // WDO-124 B-follow-up: doc → parent_*_id was the gap that prompted
        // this fix; the slot was previously hard-coded to Null on every doc.
        match &template_doc.parent_item_id {
            Some(item_id) => doc.insert(
                txn,
                Arc::<str>::from("parent_item_id"),
                Any::String(Arc::from(item_id.as_str())),
            ),
            None => doc.insert(txn, Arc::<str>::from("parent_item_id"), Any::Null),
        };
        match &template_doc.parent_milestone_id {
            Some(ms_id) => doc.insert(
                txn,
                Arc::<str>::from("parent_milestone_id"),
                Any::String(Arc::from(ms_id.as_str())),
            ),
            None => doc.insert(txn, Arc::<str>::from("parent_milestone_id"), Any::Null),
        };

        match template_doc.review_cadence_days {
            Some(days) => doc.insert(
                txn,
                Arc::<str>::from("review_cadence_days"),
                Any::BigInt(days as i64),
            ),
            None => doc.insert(txn, Arc::<str>::from("review_cadence_days"), Any::Null),
        };
        // `last_reviewed_at` and `forked_from` are deliberately NOT carried
        // from the template (see TemplateDocument doc-comment): both are
        // identity-bound state that would lie in the new space. New docs
        // start fresh — cadence baseline = `created_at`, lineage absent.
        doc.insert(txn, Arc::<str>::from("last_reviewed_at"), Any::Null);

        doc.insert(
            txn,
            Arc::<str>::from("is_template"),
            Any::Bool(template_doc.is_template),
        );
        doc.insert(txn, Arc::<str>::from("forked_from"), Any::Null);

        docs_order.push_back(txn, Any::String(Arc::from(new_id)));
    }
}

// ─── Comments (v3) ───────────────────────────────────────────────────────────

/// Build the comments.yjs doc from template comments, mirroring the structure
/// the client and the MCP add_comment path write: `threads[item_id][comment_id]`
/// Y.Maps plus `threads_order[item_id]` root arrays.
///
/// Authorship: nil `author_id` + the `author_name` snapshot. The comment-author
/// backfill skips author IDs without a users row, so the snapshot is permanent,
/// and the edit/delete window (keyed to author_id) never matches — template
/// comments are effectively read-only sample content.
///
/// Comment bodies are decoded through `blob_uuid_map`, so inline image URLs
/// get the same attachment/space UUID rewrite as item descriptions.
fn build_comments_doc(
    content: &TemplateContent,
    start_date: NaiveDate,
    blob_uuid_map: &HashMap<String, String>,
) -> Option<Doc> {
    if content.items.iter().all(|i| i.comments.is_empty()) {
        return None;
    }

    let start_epoch = start_date
        .and_hms_opt(0, 0, 0)
        .expect("midnight is valid")
        .and_utc();
    let nil_author = uuid::Uuid::nil().to_string();

    let doc = Doc::new();
    {
        let mut txn = doc.transact_mut();
        let threads = txn.get_or_insert_map("threads");
        let threads_order = txn.get_or_insert_map("threads_order");

        for item in &content.items {
            if item.comments.is_empty() {
                continue;
            }
            let item_threads: yrs::MapRef =
                threads.insert(&mut txn, item.id.as_str(), MapPrelim::default());
            let order: yrs::ArrayRef =
                threads_order.insert(&mut txn, item.id.as_str(), ArrayPrelim::default());

            for comment in &item.comments {
                let cmap: yrs::MapRef =
                    item_threads.insert(&mut txn, comment.id.as_str(), MapPrelim::default());
                cmap.insert(
                    &mut txn,
                    Arc::<str>::from("id"),
                    Any::String(Arc::from(comment.id.as_str())),
                );
                cmap.insert(
                    &mut txn,
                    Arc::<str>::from("item_id"),
                    Any::String(Arc::from(item.id.as_str())),
                );
                cmap.insert(
                    &mut txn,
                    Arc::<str>::from("author_id"),
                    Any::String(Arc::from(nil_author.as_str())),
                );
                cmap.insert(
                    &mut txn,
                    Arc::<str>::from("author_name"),
                    Any::String(Arc::from(comment.author_name.as_str())),
                );
                let created_at = (start_epoch
                    + chrono::Duration::seconds(comment.created_at_offset_seconds))
                .to_rfc3339();
                cmap.insert(
                    &mut txn,
                    Arc::<str>::from("created_at"),
                    Any::String(Arc::from(created_at.as_str())),
                );
                cmap.insert(&mut txn, Arc::<str>::from("deleted"), Any::Bool(false));
                if let Some(ref parent_id) = comment.parent_id {
                    cmap.insert(
                        &mut txn,
                        Arc::<str>::from("parent_id"),
                        Any::String(Arc::from(parent_id.as_str())),
                    );
                }

                let frag: yrs::XmlFragmentRef =
                    cmap.insert(&mut txn, "content", XmlFragmentPrelim::default());
                if let Some(ref content_b64) = comment.content_yjs {
                    let _ = decode_base64_to_xml_fragment(
                        content_b64,
                        "content",
                        &mut txn,
                        &frag,
                        blob_uuid_map,
                    );
                }

                if comment.parent_id.is_none() {
                    order.push_back(&mut txn, Any::String(Arc::from(comment.id.as_str())));
                }
            }
        }
    }
    Some(doc)
}

// ─── Binary decode helpers ───────────────────────────────────────────────────

/// Replace all occurrences of old UUIDs with new ones in raw bytes.
///
/// UUIDs are 36-byte fixed-length ASCII strings (`xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`).
/// Since old and new are both 36 bytes, replacement is length-preserving and won't
/// corrupt the Yjs binary structure. Handles docEmbed references and inline
/// attachment image URLs (`/api/spaces/{space}/attachments/{id}` — both the
/// space UUID and the attachment UUID are entries in the map) in rich text.
pub fn remap_uuids_in_blob(bytes: &mut [u8], uuid_map: &HashMap<String, String>) {
    for (old_id, new_id) in uuid_map {
        let old_bytes = old_id.as_bytes();
        let new_bytes = new_id.as_bytes();
        // Hard-asserted (not `debug_assert!`) so the invariant fires under
        // `cargo test --release` too — the canonical workflow here. The
        // 36-byte slice window below relies on this; without the assert
        // the function silently no-ops on any non-UUID input.
        assert_eq!(old_bytes.len(), 36, "Old UUID should be 36 bytes");
        assert_eq!(new_bytes.len(), 36, "New UUID should be 36 bytes");

        // Scan for all occurrences and replace in-place
        let mut pos = 0;
        while pos + 36 <= bytes.len() {
            if &bytes[pos..pos + 36] == old_bytes {
                bytes[pos..pos + 36].copy_from_slice(new_bytes);
                pos += 36;
            } else {
                pos += 1;
            }
        }
    }
}

/// Decode base64 → remap document UUIDs → Yjs Doc. Returns the Doc.
pub(crate) fn decode_base64_to_doc(
    base64_str: &str,
    uuid_map: &HashMap<String, String>,
) -> Result<Doc, String> {
    let mut bytes = base64::engine::general_purpose::STANDARD
        .decode(base64_str)
        .map_err(|e| format!("base64 decode error: {}", e))?;

    remap_uuids_in_blob(&mut bytes, uuid_map);

    let doc = Doc::new();
    {
        let mut txn = doc.transact_mut();
        let update =
            Update::decode_v1(&bytes).map_err(|e| format!("Yjs update decode error: {}", e))?;
        txn.apply_update(update);
    }
    Ok(doc)
}

/// Decode base64 → remap document UUIDs → temp Doc → deep-copy XmlFragment into target.
///
/// The `fragment_key` is the key under which the fragment was stored in the temp doc
/// during serialization (always "content").
pub(crate) fn decode_base64_to_xml_fragment(
    base64_str: &str,
    fragment_key: &str,
    dst_txn: &mut yrs::TransactionMut,
    dst_frag: &yrs::XmlFragmentRef,
    uuid_map: &HashMap<String, String>,
) -> Result<(), String> {
    let mut bytes = base64::engine::general_purpose::STANDARD
        .decode(base64_str)
        .map_err(|e| format!("base64 decode error: {}", e))?;

    remap_uuids_in_blob(&mut bytes, uuid_map);

    let temp_doc = Doc::new();
    {
        let mut txn = temp_doc.transact_mut();
        let update =
            Update::decode_v1(&bytes).map_err(|e| format!("Yjs update decode error: {}", e))?;
        txn.apply_update(update);
    }

    let src_txn = temp_doc.transact();
    let src_frag = src_txn
        .get_xml_fragment(fragment_key)
        .ok_or_else(|| format!("No '{}' fragment in decoded doc", fragment_key))?;

    crate::yrs_xml_copy::deep_copy_xml_fragment(&src_frag, &src_txn, dst_frag, dst_txn, 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use yrs::types::xml::{XmlElementPrelim, XmlTextPrelim};
    use yrs::{GetString, StateVector, Xml, XmlFragment};

    /// Create a base64-encoded Yjs blob containing an XmlFragment under "content".
    fn make_xml_blob(build: impl FnOnce(&mut yrs::TransactionMut, &yrs::XmlFragmentRef)) -> String {
        let doc = Doc::new();
        {
            let mut txn = doc.transact_mut();
            let frag: yrs::XmlFragmentRef = txn.get_or_insert_xml_fragment("content");
            build(&mut txn, &frag);
        }
        let bytes = {
            let txn = doc.transact();
            txn.encode_state_as_update_v1(&StateVector::default())
        };
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    }

    /// Create a base64-encoded Yjs blob for a standalone document.
    fn make_doc_blob(build: impl FnOnce(&mut yrs::TransactionMut, &yrs::XmlFragmentRef)) -> String {
        make_xml_blob(build)
    }

    fn make_test_content() -> TemplateContent {
        let mut label_values = HashMap::new();
        label_values.insert(
            "val-1".into(),
            TemplateLabelValue {
                id: "val-1".into(),
                name: "Ready".into(),
                color: "#3B82F6".into(),
                is_completion_state: false,
                ..Default::default()
            },
        );
        label_values.insert(
            "val-2".into(),
            TemplateLabelValue {
                id: "val-2".into(),
                name: "Done".into(),
                color: "#22C55E".into(),
                is_completion_state: true,
                completion_prompt: Some("What shipped?".into()),
                ..Default::default()
            },
        );

        let mut label_defs = HashMap::new();
        label_defs.insert(
            "label-1".into(),
            TemplateLabelDef {
                id: "label-1".into(),
                name: "Status".into(),
                description: "Track progress".into(),
                icon: "circle".into(),
                values_order: vec!["val-1".into(), "val-2".into()],
                values: label_values,
                ..Default::default()
            },
        );

        let mut ms_defs = HashMap::new();
        ms_defs.insert(
            "ms-1".into(),
            TemplateMilestoneDef {
                id: "ms-1".into(),
                name: "Alpha".into(),
                description: Some("First release".into()),
                offset_days: Some(14),
            },
        );

        let mut view_defs = HashMap::new();
        view_defs.insert(
            "view-1".into(),
            TemplateViewDef {
                id: "view-1".into(),
                name: "Board".into(),
                content_type: "items".into(),
                view_type: "board".into(),
                column_grouping: Some("label-1".into()),
                row_grouping: Some("label-1".into()),
                sort_order: "0".into(),
                show_archived: false,
                filters: Some("label-1.val-1,ms:ms-1,d:week".into()),
                zoom_level: Some("w".into()),
            },
        );

        TemplateContent {
            schema_version: 1,
            source_space_id: None,
            attachments: vec![],
            labels: TemplateLabels {
                order: vec!["label-1".into()],
                primary_label_id: Some("label-1".into()),
                definitions: label_defs,
            },
            milestones: TemplateMilestones {
                order: vec!["ms-1".into()],
                definitions: ms_defs,
            },
            cycle_config: Some(TemplateCycleConfig {
                enabled: true,
                pattern: "biweekly".into(),
                start_day: "monday".into(),
                prefix: "Sprint".into(),
                generate_ahead: 4,
                retain_past: 1,
            }),
            short_id_config: Some(TemplateShortIdConfig {
                prefix: "PRJ".into(),
                visible: true,
            }),
            archive_config: Some(TemplateArchiveConfig { migration_days: 30 }),
            views: TemplateViews {
                order: vec!["view-1".into()],
                definitions: view_defs,
            },
            items: vec![
                TemplateItem {
                    id: "item-1".into(),
                    title: "Setup project".into(),
                    labels: {
                        let mut m = HashMap::new();
                        m.insert("label-1".into(), "val-1".into());
                        m
                    },
                    due_date_offset_days: Some(7),
                    start_date_offset_days: Some(0),
                    milestone_id: Some("ms-1".into()),
                    cycle_id: Some("cycle-1".into()),
                    parent_id: None,
                    blocked_by: vec![],
                    description_yjs: Some(make_xml_blob(|txn, frag| {
                        let p: yrs::XmlElementRef =
                            frag.push_back(txn, XmlElementPrelim::empty("paragraph"));
                        let _: yrs::XmlTextRef =
                            p.push_back(txn, XmlTextPrelim::new("First step."));
                    })),
                    attachment_ids: vec![],
                    comments: vec![],
                },
                TemplateItem {
                    id: "item-2".into(),
                    title: "Write docs".into(),
                    labels: HashMap::new(),
                    due_date_offset_days: Some(14),
                    start_date_offset_days: None,
                    milestone_id: None,
                    cycle_id: None,
                    parent_id: Some("item-1".into()),
                    blocked_by: vec!["item-1".into()],
                    description_yjs: None,
                    attachment_ids: vec![],
                    comments: vec![],
                },
            ],
            documents: vec![TemplateDocument {
                id: "11111111-1111-1111-1111-111111111111".into(),
                title: "Getting Started".into(),
                labels: HashMap::new(),
                parent_item_id: None,
                parent_milestone_id: None,
                content_yjs: Some(make_doc_blob(|txn, frag| {
                    let h: yrs::XmlElementRef =
                        frag.push_back(txn, XmlElementPrelim::empty("heading"));
                    h.insert_attribute(txn, "level", "1");
                    let _: yrs::XmlTextRef = h.push_back(txn, XmlTextPrelim::new("Welcome"));
                    let p: yrs::XmlElementRef =
                        frag.push_back(txn, XmlElementPrelim::empty("paragraph"));
                    let _: yrs::XmlTextRef = p.push_back(txn, XmlTextPrelim::new("Hello world"));
                })),
                owners_default: Default::default(),
                review_cadence_days: None,
                is_template: false,
            }],
            overview_yjs: None,
        }
    }

    #[test]
    fn test_instantiate_preserves_uuids() {
        let content = make_test_content();
        let start_date = NaiveDate::parse_from_str("2026-03-01", "%Y-%m-%d").unwrap();

        let result = instantiate_template(&content, start_date, None, "user-123", None);
        let txn = result.space_doc.transact();

        // All original UUIDs should be preserved exactly
        let items = txn.get_map("items").expect("items map");
        assert!(
            items.get(&txn, "item-1").is_some(),
            "item-1 should keep its UUID"
        );
        assert!(
            items.get(&txn, "item-2").is_some(),
            "item-2 should keep its UUID"
        );

        let config = txn.get_map("config").expect("config map");
        let labels = match config.get(&txn, "labels") {
            Some(yrs::Out::YMap(m)) => m,
            _ => panic!("labels map missing"),
        };
        assert!(
            labels.get(&txn, "label-1").is_some(),
            "label-1 should keep its UUID"
        );

        let milestones = match config.get(&txn, "milestones") {
            Some(yrs::Out::YMap(m)) => m,
            _ => panic!("milestones map missing"),
        };
        assert!(
            milestones.get(&txn, "ms-1").is_some(),
            "ms-1 should keep its UUID"
        );

        let views = match config.get(&txn, "shared_views") {
            Some(yrs::Out::YMap(m)) => m,
            _ => panic!("views map missing"),
        };
        assert!(
            views.get(&txn, "view-1").is_some(),
            "view-1 should keep its UUID"
        );

        // Document docs should use regenerated ID (not the original)
        assert_eq!(result.document_docs.len(), 1);
        assert_ne!(
            result.document_docs[0].0, "11111111-1111-1111-1111-111111111111",
            "Document UUID should be regenerated, not preserved"
        );
        // Manifest should match
        assert_eq!(result.document_manifest.len(), 1);
        assert_eq!(
            result.document_manifest[0].new_id, result.document_docs[0].0,
            "Manifest new_id should match document_docs key"
        );
        assert_eq!(result.document_manifest[0].title, "Getting Started");
    }

    #[test]
    fn test_instantiate_preserves_references() {
        let content = make_test_content();
        let start_date = NaiveDate::parse_from_str("2026-03-01", "%Y-%m-%d").unwrap();

        let result = instantiate_template(&content, start_date, None, "user-123", None);
        let txn = result.space_doc.transact();
        let items = txn.get_map("items").expect("items map");

        // item-2's parent_id should still point to item-1
        let item2 = match items.get(&txn, "item-2") {
            Some(yrs::Out::YMap(m)) => m,
            _ => panic!("item-2 missing"),
        };
        match item2.get(&txn, "parent_id") {
            Some(yrs::Out::Any(yrs::Any::String(s))) => {
                assert_eq!(
                    s.to_string(),
                    "item-1",
                    "parent_id reference should be preserved"
                );
            }
            _ => panic!("parent_id missing"),
        }

        // item-2's blocked_by should still contain item-1
        match item2.get(&txn, "blocked_by") {
            Some(yrs::Out::YArray(arr)) => {
                let entries: Vec<String> = arr
                    .iter(&txn)
                    .filter_map(|v| match v {
                        yrs::Out::Any(yrs::Any::String(s)) => Some(s.to_string()),
                        _ => None,
                    })
                    .collect();
                assert_eq!(
                    entries,
                    vec!["item-1"],
                    "blocked_by reference should be preserved"
                );
            }
            _ => panic!("blocked_by missing"),
        }

        // item-1's label assignment should reference original label/value IDs
        let item1 = match items.get(&txn, "item-1") {
            Some(yrs::Out::YMap(m)) => m,
            _ => panic!("item-1 missing"),
        };
        let labels = match item1.get(&txn, "labels") {
            Some(yrs::Out::YMap(m)) => m,
            _ => panic!("labels missing"),
        };
        match labels.get(&txn, "label-1") {
            Some(yrs::Out::Any(yrs::Any::String(s))) => {
                assert_eq!(
                    s.to_string(),
                    "val-1",
                    "label value reference should be preserved"
                );
            }
            _ => panic!("label-1 value missing on item"),
        }
    }

    #[test]
    fn test_instantiate_full_template() {
        let content = make_test_content();
        let start_date = NaiveDate::parse_from_str("2026-03-01", "%Y-%m-%d").unwrap();

        let result = instantiate_template(&content, start_date, None, "user-123", None);
        let txn = result.space_doc.transact();

        // Check primary_label_id preserved
        let config = txn.get_map("config").expect("config map");
        match config.get(&txn, "primary_label_id") {
            Some(yrs::Out::Any(yrs::Any::String(s))) => {
                assert_eq!(s.to_string(), "label-1");
            }
            _ => panic!("primary_label_id missing"),
        }

        // Check milestone deadline rehydrated
        let milestones = match config.get(&txn, "milestones") {
            Some(yrs::Out::YMap(m)) => m,
            _ => panic!("milestones map missing"),
        };
        let ms = match milestones.get(&txn, "ms-1") {
            Some(yrs::Out::YMap(m)) => m,
            _ => panic!("milestone missing"),
        };
        match ms.get(&txn, "deadline") {
            Some(yrs::Out::Any(yrs::Any::String(s))) => {
                assert_eq!(s.to_string(), "2026-03-15"); // start_date + 14 days
            }
            _ => panic!("milestone deadline missing"),
        }

        // Check item-1 due date rehydrated
        let items = txn.get_map("items").expect("items map");
        let item1 = match items.get(&txn, "item-1") {
            Some(yrs::Out::YMap(m)) => m,
            _ => panic!("item-1 missing"),
        };
        match item1.get(&txn, "due_date") {
            Some(yrs::Out::Any(yrs::Any::String(s))) => {
                assert_eq!(s.to_string(), "2026-03-08"); // start_date + 7 days
            }
            _ => panic!("due_date missing"),
        }
        match item1.get(&txn, "start_date") {
            Some(yrs::Out::Any(yrs::Any::String(s))) => {
                assert_eq!(s.to_string(), "2026-03-01"); // start_date + 0 days
            }
            _ => panic!("start_date missing"),
        }

        // Check milestone reference on item
        match item1.get(&txn, "milestone_id") {
            Some(yrs::Out::Any(yrs::Any::String(s))) => {
                assert_eq!(s.to_string(), "ms-1");
            }
            _ => panic!("milestone_id missing"),
        }
    }

    #[test]
    fn test_instantiate_empty_template() {
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

        let start_date = NaiveDate::parse_from_str("2026-03-01", "%Y-%m-%d").unwrap();
        let result = instantiate_template(&content, start_date, None, "user-123", None);

        let txn = result.space_doc.transact();
        let items = txn.get_map("items");
        assert!(
            items.is_none() || {
                let m = items.unwrap();
                m.iter(&txn).count() == 0
            }
        );
        assert!(result.document_docs.is_empty());
        assert!(result.document_manifest.is_empty());
    }

    #[test]
    fn test_date_rehydration() {
        let start = NaiveDate::parse_from_str("2026-03-01", "%Y-%m-%d").unwrap();

        let date = start + chrono::Duration::days(14);
        assert_eq!(date.format("%Y-%m-%d").to_string(), "2026-03-15");

        let date = start + chrono::Duration::days(0);
        assert_eq!(date.format("%Y-%m-%d").to_string(), "2026-03-01");

        let date = start + chrono::Duration::days(-5);
        assert_eq!(date.format("%Y-%m-%d").to_string(), "2026-02-24");
    }

    #[test]
    fn test_view_filters_cleaned() {
        let content = make_test_content();
        let start_date = NaiveDate::parse_from_str("2026-03-01", "%Y-%m-%d").unwrap();
        let result = instantiate_template(&content, start_date, None, "user-123", None);

        let txn = result.space_doc.transact();
        let config = txn.get_map("config").unwrap();
        let views = match config.get(&txn, "shared_views") {
            Some(yrs::Out::YMap(m)) => m,
            _ => panic!("views missing"),
        };

        let view = match views.get(&txn, "view-1") {
            Some(yrs::Out::YMap(m)) => m,
            _ => panic!("view missing"),
        };

        // column_grouping preserved (references label-1)
        match view.get(&txn, "column_grouping") {
            Some(yrs::Out::Any(yrs::Any::String(s))) => {
                assert_eq!(s.to_string(), "label-1");
            }
            _ => panic!("column_grouping missing"),
        }

        // Filters: "label-1.val-1,ms:ms-1,d:week" — all kept (no cy:/u:/t:)
        match view.get(&txn, "filters") {
            Some(yrs::Out::Any(yrs::Any::String(s))) => {
                let filter_str = s.to_string();
                assert!(
                    filter_str.contains("label-1.val-1"),
                    "Label filter should be preserved"
                );
                assert!(
                    filter_str.contains("ms:ms-1"),
                    "Milestone filter should be preserved"
                );
                assert!(
                    filter_str.contains("d:week"),
                    "Date filter should be preserved"
                );
            }
            _ => panic!("filters missing"),
        }
    }

    #[test]
    fn test_clean_filters_drops_inapplicable() {
        // cy:, u:, t: should be dropped
        assert_eq!(clean_filters("cy:cycle-1"), None);
        assert_eq!(clean_filters("u:user-1"), None);
        assert_eq!(clean_filters("t:team-1"), None);

        // Mixed: keep label and date, drop user and cycle
        assert_eq!(
            clean_filters("label-1.val-1,u:user-1,cy:cycle-1,d:week"),
            Some("label-1.val-1,d:week".into())
        );

        // All kept
        assert_eq!(
            clean_filters("label-1.val-1,ms:ms-1,d:overdue,p:has"),
            Some("label-1.val-1,ms:ms-1,d:overdue,p:has".into())
        );

        // Empty
        assert_eq!(clean_filters(""), None);
    }

    #[test]
    fn test_instantiate_item_description_content() {
        let content = make_test_content();
        let start_date = NaiveDate::parse_from_str("2026-03-01", "%Y-%m-%d").unwrap();
        let result = instantiate_template(&content, start_date, None, "user-123", None);

        let txn = result.space_doc.transact();
        let items = txn.get_map("items").expect("items map");
        let item1 = match items.get(&txn, "item-1") {
            Some(yrs::Out::YMap(m)) => m,
            _ => panic!("item-1 missing"),
        };

        let desc = match item1.get(&txn, "description") {
            Some(yrs::Out::YXmlFragment(f)) => f,
            _ => panic!("description missing or not XmlFragment"),
        };
        let xml_str = desc.get_string(&txn);
        assert!(
            xml_str.contains("First step."),
            "Description should contain 'First step.'"
        );
        assert!(
            xml_str.contains("paragraph"),
            "Description should preserve paragraph element"
        );
    }

    #[test]
    fn test_instantiate_overview() {
        let mut content = make_test_content();
        content.overview_yjs = Some(make_xml_blob(|txn, frag| {
            let p: yrs::XmlElementRef = frag.push_back(txn, XmlElementPrelim::empty("paragraph"));
            let _: yrs::XmlTextRef = p.push_back(txn, XmlTextPrelim::new("Space overview content"));
        }));

        let start_date = NaiveDate::parse_from_str("2026-03-01", "%Y-%m-%d").unwrap();
        let result = instantiate_template(&content, start_date, None, "user-123", None);

        let txn = result.space_doc.transact();
        let overview = txn
            .get_xml_fragment("description")
            .expect("overview fragment missing");
        let xml_str = overview.get_string(&txn);
        assert!(xml_str.contains("Space overview content"));
    }

    #[test]
    fn test_short_id_prefix_override() {
        let content = make_test_content();
        let start_date = NaiveDate::parse_from_str("2026-03-01", "%Y-%m-%d").unwrap();

        // With override
        let result = instantiate_template(&content, start_date, Some("MNP"), "user-123", None);
        let txn = result.space_doc.transact();
        let config = txn.get_map("config").unwrap();
        match config.get(&txn, "short_id_prefix") {
            Some(yrs::Out::Any(yrs::Any::String(s))) => assert_eq!(s.to_string(), "MNP"),
            _ => panic!("short_id_prefix missing"),
        }

        // Without override — uses template's prefix
        let result = instantiate_template(&content, start_date, None, "user-123", None);
        let txn = result.space_doc.transact();
        let config = txn.get_map("config").unwrap();
        match config.get(&txn, "short_id_prefix") {
            Some(yrs::Out::Any(yrs::Any::String(s))) => assert_eq!(s.to_string(), "PRJ"),
            _ => panic!("short_id_prefix missing"),
        }
    }

    #[test]
    fn test_instantiate_cycle_config() {
        let content = make_test_content();
        let start_date = NaiveDate::parse_from_str("2026-03-01", "%Y-%m-%d").unwrap();
        let result = instantiate_template(&content, start_date, None, "user-123", None);

        let txn = result.space_doc.transact();
        let config = txn.get_map("config").expect("config map");
        let cc = match config.get(&txn, "cycle_config") {
            Some(yrs::Out::YMap(m)) => m,
            _ => panic!("cycle_config missing"),
        };

        match cc.get(&txn, "enabled") {
            Some(yrs::Out::Any(yrs::Any::Bool(b))) => assert!(b),
            _ => panic!("enabled missing"),
        }
        match cc.get(&txn, "pattern") {
            Some(yrs::Out::Any(yrs::Any::String(s))) => assert_eq!(s.to_string(), "biweekly"),
            _ => panic!("pattern missing"),
        }
        match cc.get(&txn, "next_number") {
            Some(yrs::Out::Any(yrs::Any::Number(n))) => assert_eq!(n as i64, 1),
            _ => panic!("next_number missing"),
        }
    }

    #[test]
    fn test_instantiate_archive_config() {
        let content = make_test_content();
        let start_date = NaiveDate::parse_from_str("2026-03-01", "%Y-%m-%d").unwrap();
        let result = instantiate_template(&content, start_date, None, "user-123", None);

        let txn = result.space_doc.transact();
        let config = txn.get_map("config").expect("config map");
        let ac = match config.get(&txn, "archive_config") {
            Some(yrs::Out::YMap(m)) => m,
            _ => panic!("archive_config missing"),
        };

        match ac.get(&txn, "migration_days") {
            Some(yrs::Out::Any(yrs::Any::Number(n))) => assert_eq!(n as i64, 30),
            _ => panic!("migration_days missing"),
        }
    }

    #[test]
    fn test_instantiate_item_cycle_id() {
        let content = make_test_content();
        let start_date = NaiveDate::parse_from_str("2026-03-01", "%Y-%m-%d").unwrap();
        let result = instantiate_template(&content, start_date, None, "user-123", None);

        let txn = result.space_doc.transact();
        let items = txn.get_map("items").expect("items map");

        let item1 = match items.get(&txn, "item-1") {
            Some(yrs::Out::YMap(m)) => m,
            _ => panic!("item-1 missing"),
        };
        match item1.get(&txn, "cycle_id") {
            Some(yrs::Out::Any(yrs::Any::String(s))) => {
                assert_eq!(s.to_string(), "cycle-1");
            }
            _ => panic!("cycle_id missing"),
        }

        // item-2 has no cycle_id
        let item2 = match items.get(&txn, "item-2") {
            Some(yrs::Out::YMap(m)) => m,
            _ => panic!("item-2 missing"),
        };
        assert!(item2.get(&txn, "cycle_id").is_none());
    }

    #[test]
    fn test_instantiate_document_content() {
        let content = make_test_content();
        let start_date = NaiveDate::parse_from_str("2026-03-01", "%Y-%m-%d").unwrap();
        let result = instantiate_template(&content, start_date, None, "user-123", None);

        assert_eq!(result.document_docs.len(), 1);
        let (doc_id, doc_yjs) = &result.document_docs[0];
        assert_ne!(
            doc_id, "11111111-1111-1111-1111-111111111111",
            "Document UUID should be regenerated"
        );

        let txn = doc_yjs.transact();
        let content_frag = txn
            .get_xml_fragment("content")
            .expect("content fragment missing");

        let xml_str = content_frag.get_string(&txn);
        assert!(xml_str.contains("Welcome"));
        assert!(xml_str.contains("Hello world"));
        assert!(xml_str.contains("heading"));
    }

    #[test]
    fn test_instantiate_view_row_grouping() {
        let content = make_test_content();
        let start_date = NaiveDate::parse_from_str("2026-03-01", "%Y-%m-%d").unwrap();
        let result = instantiate_template(&content, start_date, None, "user-123", None);

        let txn = result.space_doc.transact();
        let config = txn.get_map("config").unwrap();
        let views = match config.get(&txn, "shared_views") {
            Some(yrs::Out::YMap(m)) => m,
            _ => panic!("views missing"),
        };

        let view = match views.get(&txn, "view-1") {
            Some(yrs::Out::YMap(m)) => m,
            _ => panic!("view missing"),
        };

        match view.get(&txn, "row_grouping") {
            Some(yrs::Out::Any(yrs::Any::String(s))) => {
                assert_eq!(s.to_string(), "label-1");
            }
            _ => panic!("row_grouping missing"),
        }

        match view.get(&txn, "zoom_level") {
            Some(yrs::Out::Any(yrs::Any::String(s))) => {
                assert_eq!(s.to_string(), "w");
            }
            _ => panic!("zoom_level missing"),
        }
    }

    // =========================================================================
    // WDO-124 Milestone B6 — TemplateDocument B-phase primitive instantiation.
    //
    // Confirms that a template doc carrying owners_default / review_cadence_days
    // / is_template gets those primitives written into the new doc's Y.Map
    // when the space is instantiated. The "creator" sentinel resolves to the
    // creating user's UUID. Unaffected pre-B6 templates default to inert
    // values (no owners, no cadence, not template).
    // =========================================================================

    fn make_minimal_template_with_doc(doc: TemplateDocument) -> TemplateContent {
        TemplateContent {
            schema_version: 1,
            source_space_id: None,
            attachments: vec![],
            labels: crate::template::types::TemplateLabels {
                order: vec![],
                primary_label_id: None,
                definitions: HashMap::new(),
            },
            milestones: crate::template::types::TemplateMilestones {
                order: vec![],
                definitions: HashMap::new(),
            },
            cycle_config: None,
            short_id_config: None,
            archive_config: None,
            views: crate::template::types::TemplateViews {
                order: vec![],
                definitions: HashMap::new(),
            },
            items: vec![],
            documents: vec![doc],
            overview_yjs: None,
        }
    }

    fn first_doc_map<'a>(txn: &'a yrs::Transaction<'a>) -> yrs::MapRef {
        let documents = txn.get_map("documents").expect("documents map");
        let mut iter = documents.iter(txn);
        let (_id, val) = iter.next().expect("at least one doc");
        match val {
            yrs::Out::YMap(m) => m,
            _ => panic!("doc is not a Y.Map"),
        }
    }

    #[test]
    fn b6_template_doc_writes_owners_default_to_ymap() {
        let template_doc = TemplateDocument {
            id: "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa".into(),
            title: "Runbook".into(),
            labels: HashMap::new(),
            content_yjs: None,
            owners_default: crate::template::types::TemplateOwnersDefault {
                users: vec!["creator".into(), "user-x".into()],
                teams: vec!["team-ops".into()],
            },
            review_cadence_days: Some(90),
            is_template: true,
            parent_item_id: None,
            parent_milestone_id: None,
        };
        let content = make_minimal_template_with_doc(template_doc);
        let result = instantiate_template(
            &content,
            NaiveDate::parse_from_str("2026-05-01", "%Y-%m-%d").unwrap(),
            None,
            "alice-uuid",
            None,
        );
        let txn = result.space_doc.transact();
        let doc_map = first_doc_map(&txn);
        let _ = &result;

        // Owners nested map with users + teams arrays.
        let owners_map = match doc_map.get(&txn, "owners") {
            Some(yrs::Out::YMap(m)) => m,
            _ => panic!("owners not a Y.Map"),
        };
        let users: Vec<String> = match owners_map.get(&txn, "users") {
            Some(yrs::Out::YArray(arr)) => arr
                .iter(&txn)
                .filter_map(|out| match out {
                    yrs::Out::Any(yrs::Any::String(s)) => Some(s.to_string()),
                    _ => None,
                })
                .collect(),
            _ => vec![],
        };
        // "creator" resolves to alice-uuid; explicit UUIDs pass through.
        assert_eq!(users, vec!["alice-uuid".to_string(), "user-x".to_string()]);

        let teams: Vec<String> = match owners_map.get(&txn, "teams") {
            Some(yrs::Out::YArray(arr)) => arr
                .iter(&txn)
                .filter_map(|out| match out {
                    yrs::Out::Any(yrs::Any::String(s)) => Some(s.to_string()),
                    _ => None,
                })
                .collect(),
            _ => vec![],
        };
        assert_eq!(teams, vec!["team-ops".to_string()]);

        // Cadence + template flag.
        match doc_map.get(&txn, "review_cadence_days") {
            Some(yrs::Out::Any(yrs::Any::BigInt(n))) => assert_eq!(n, 90),
            other => panic!("review_cadence_days expected BigInt(90), got {:?}", other),
        }
        match doc_map.get(&txn, "is_template") {
            Some(yrs::Out::Any(yrs::Any::Bool(b))) => assert!(b),
            other => panic!("is_template expected true, got {:?}", other),
        }
    }

    #[test]
    fn b6_template_doc_without_primitives_initialises_inert_defaults() {
        // Pre-B6 templates (or new docs with no primitives) must instantiate
        // with empty owners, null cadence, is_template=false — same shape a
        // freshly-created doc through the Yjs client API would have.
        let template_doc = TemplateDocument {
            id: "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb".into(),
            title: "Plain doc".into(),
            labels: HashMap::new(),
            content_yjs: None,
            owners_default: Default::default(),
            review_cadence_days: None,
            is_template: false,
            parent_item_id: None,
            parent_milestone_id: None,
        };
        let content = make_minimal_template_with_doc(template_doc);
        let result = instantiate_template(
            &content,
            NaiveDate::parse_from_str("2026-05-01", "%Y-%m-%d").unwrap(),
            None,
            "alice-uuid",
            None,
        );
        let txn = result.space_doc.transact();
        let doc_map = first_doc_map(&txn);
        let _ = &result;

        // Owners map exists but both arrays are empty.
        let owners_map = match doc_map.get(&txn, "owners") {
            Some(yrs::Out::YMap(m)) => m,
            _ => panic!("owners not a Y.Map"),
        };
        let users_len = match owners_map.get(&txn, "users") {
            Some(yrs::Out::YArray(arr)) => arr.len(&txn),
            _ => 0,
        };
        assert_eq!(users_len, 0);

        // Cadence is null.
        match doc_map.get(&txn, "review_cadence_days") {
            Some(yrs::Out::Any(yrs::Any::Null)) => {}
            None => {}
            other => panic!("review_cadence_days expected Null, got {:?}", other),
        }

        // is_template defaults to false.
        match doc_map.get(&txn, "is_template") {
            Some(yrs::Out::Any(yrs::Any::Bool(false))) => {}
            other => panic!("is_template expected false, got {:?}", other),
        }
    }

    // =========================================================================
    // WDO-124 B-follow-up: structural-vs-identity template fidelity.
    //
    // Verifies that:
    //   - `parent_item_id` / `parent_milestone_id` survive a template
    //     instantiation (structural anchors — item/milestone UUIDs are
    //     preserved verbatim per this module's doc-comment).
    //   - `last_reviewed_at` / `forked_from` are reset to Null on
    //     instantiation even when the source template doc had them set
    //     (identity-bound state would lie in the new space).
    //
    // See `TemplateDocument` doc-comment in `types.rs` for the rule.
    // =========================================================================

    #[test]
    fn b_follow_up_parent_refs_survive_instantiation() {
        let item_uuid = "40000000-0000-0000-0000-000000000001".to_string();
        let ms_uuid = "30000000-0000-0000-0000-000000000001".to_string();

        let template_doc = TemplateDocument {
            id: "cccccccc-cccc-cccc-cccc-cccccccccccc".into(),
            title: "Architecture Guide".into(),
            labels: HashMap::new(),
            content_yjs: None,
            owners_default: Default::default(),
            review_cadence_days: None,
            is_template: false,
            parent_item_id: Some(item_uuid.clone()),
            parent_milestone_id: Some(ms_uuid.clone()),
        };
        let content = make_minimal_template_with_doc(template_doc);
        let result = instantiate_template(
            &content,
            NaiveDate::parse_from_str("2026-05-01", "%Y-%m-%d").unwrap(),
            None,
            "creator-uuid",
            None,
        );

        let txn = result.space_doc.transact();
        let doc_map = first_doc_map(&txn);
        let _ = &result;

        match doc_map.get(&txn, "parent_item_id") {
            Some(yrs::Out::Any(yrs::Any::String(s))) => assert_eq!(s.as_ref(), item_uuid),
            other => panic!("parent_item_id expected {item_uuid:?}, got {other:?}"),
        }
        match doc_map.get(&txn, "parent_milestone_id") {
            Some(yrs::Out::Any(yrs::Any::String(s))) => assert_eq!(s.as_ref(), ms_uuid),
            other => panic!("parent_milestone_id expected {ms_uuid:?}, got {other:?}"),
        }
    }

    #[test]
    fn b_follow_up_identity_bound_fields_are_null_on_instantiation() {
        // last_reviewed_at and forked_from aren't fields on TemplateDocument
        // at all (the rule starts at the struct), but the instantiator still
        // writes Null to those Y.Map keys for shape consistency with a doc
        // created fresh through the client API. This test pins that
        // behavior — a future "helpful" change that started carrying them
        // would fail here.
        let template_doc = TemplateDocument {
            id: "dddddddd-dddd-dddd-dddd-dddddddddddd".into(),
            title: "Plain doc".into(),
            labels: HashMap::new(),
            content_yjs: None,
            owners_default: Default::default(),
            review_cadence_days: Some(90),
            is_template: false,
            parent_item_id: None,
            parent_milestone_id: None,
        };
        let content = make_minimal_template_with_doc(template_doc);
        let result = instantiate_template(
            &content,
            NaiveDate::parse_from_str("2026-05-01", "%Y-%m-%d").unwrap(),
            None,
            "creator-uuid",
            None,
        );

        let txn = result.space_doc.transact();
        let doc_map = first_doc_map(&txn);
        let _ = &result;

        match doc_map.get(&txn, "last_reviewed_at") {
            Some(yrs::Out::Any(yrs::Any::Null)) => {}
            other => panic!("last_reviewed_at expected Null, got {other:?}"),
        }
        match doc_map.get(&txn, "forked_from") {
            Some(yrs::Out::Any(yrs::Any::Null)) => {}
            other => panic!("forked_from expected Null, got {other:?}"),
        }
    }
    // ─── Attachment copy + URL rewrite (WDO-180, schema v2) ──────────────────

    const SRC_SPACE: &str = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
    const NEW_SPACE: &str = "99999999-8888-7777-6666-555555555555";
    const ATT_A: &str = "11111111-2222-3333-4444-555555555555";
    const ATT_B: &str = "66666666-7777-8888-9999-000000000000";

    fn image_src(space_id: &str, attachment_id: &str) -> String {
        format!("/api/spaces/{}/attachments/{}", space_id, attachment_id)
    }

    /// v2 content: manifest with ATT_A; item-1 description embeds ATT_A
    /// (manifested) and ATT_B (not manifested) inline; explicit Y.Array
    /// ref to ATT_A.
    fn make_attachment_content() -> TemplateContent {
        let mut content = make_test_content();
        content.source_space_id = Some(SRC_SPACE.to_string());
        content.attachments = vec![TemplateAttachment {
            id: ATT_A.to_string(),
            filename: "diagram.png".to_string(),
            content_type: "image/png".to_string(),
            size_bytes: 1234,
        }];
        content.items[0].description_yjs = Some(make_xml_blob(|txn, frag| {
            let img: yrs::XmlElementRef = frag.push_back(txn, XmlElementPrelim::empty("image"));
            img.insert_attribute(txn, "src", image_src(SRC_SPACE, ATT_A).as_str());
            let img2: yrs::XmlElementRef = frag.push_back(txn, XmlElementPrelim::empty("image"));
            img2.insert_attribute(txn, "src", image_src(SRC_SPACE, ATT_B).as_str());
        }));
        content.items[0].attachment_ids = vec![ATT_A.to_string()];
        content
    }

    fn item1_description_xml(result: &InstantiationResult) -> String {
        let txn = result.space_doc.transact();
        let items = txn.get_map("items").expect("items map");
        let item1 = match items.get(&txn, "item-1") {
            Some(yrs::Out::YMap(m)) => m,
            _ => panic!("item-1 missing"),
        };
        let desc = match item1.get(&txn, "description") {
            Some(yrs::Out::YXmlFragment(f)) => f,
            _ => panic!("description missing"),
        };
        desc.get_string(&txn)
    }

    #[test]
    fn test_attachment_url_rewrite_and_manifest() {
        let content = make_attachment_content();
        let start_date = NaiveDate::parse_from_str("2026-03-01", "%Y-%m-%d").unwrap();
        let result = instantiate_template(&content, start_date, None, "user-123", Some(NEW_SPACE));

        // Manifest: one entry with a fresh UUID and the metadata snapshot
        assert_eq!(result.attachment_manifest.len(), 1);
        let entry = &result.attachment_manifest[0];
        assert_eq!(entry.old_id, ATT_A);
        assert_ne!(entry.new_id, ATT_A);
        assert_eq!(entry.filename, "diagram.png");
        assert_eq!(entry.content_type, "image/png");
        assert_eq!(entry.size_bytes, 1234);

        let xml = item1_description_xml(&result);

        // Manifested ref: both space and attachment UUID rewritten
        assert!(
            xml.contains(&image_src(NEW_SPACE, &entry.new_id)),
            "manifested src should be fully rewritten, got: {}",
            xml
        );
        // Un-manifested ref: space rewritten (clean 404 in the new space
        // instead of a silent cross-space dependency), attachment UUID kept
        assert!(
            xml.contains(&image_src(NEW_SPACE, ATT_B)),
            "un-manifested src should get the new space ID, got: {}",
            xml
        );
        assert!(
            !xml.contains(SRC_SPACE),
            "no source-space refs may survive, got: {}",
            xml
        );

        // Explicit Y.Array rebuilt with the remapped ID + manifest metadata
        let txn = result.space_doc.transact();
        let items = txn.get_map("items").expect("items map");
        let item1 = match items.get(&txn, "item-1") {
            Some(yrs::Out::YMap(m)) => m,
            _ => panic!("item-1 missing"),
        };
        let arr = match item1.get(&txn, "attachments") {
            Some(yrs::Out::YArray(a)) => a,
            _ => panic!("attachments array missing"),
        };
        assert_eq!(arr.len(&txn), 1);
        let amap = match arr.get(&txn, 0) {
            Some(yrs::Out::YMap(m)) => m,
            _ => panic!("attachment entry not a map"),
        };
        let read_str = |key: &str| match amap.get(&txn, key) {
            Some(yrs::Out::Any(Any::String(s))) => s.to_string(),
            other => panic!("missing string field {}: {:?}", key, other),
        };
        assert_eq!(read_str("id"), entry.new_id);
        assert_eq!(read_str("filename"), "diagram.png");
        assert_eq!(read_str("content_type"), "image/png");
        match amap.get(&txn, "size_bytes") {
            Some(yrs::Out::Any(Any::Number(n))) => assert_eq!(n as i64, 1234),
            other => panic!("missing size_bytes: {:?}", other),
        }
    }

    #[test]
    fn test_v1_content_skips_attachment_handling() {
        // v1 templates: no source_space_id, no manifest → inline refs pass
        // through untouched (today's behavior preserved)
        let mut content = make_test_content();
        content.items[0].description_yjs = Some(make_xml_blob(|txn, frag| {
            let img: yrs::XmlElementRef = frag.push_back(txn, XmlElementPrelim::empty("image"));
            img.insert_attribute(txn, "src", image_src(SRC_SPACE, ATT_A).as_str());
        }));
        let start_date = NaiveDate::parse_from_str("2026-03-01", "%Y-%m-%d").unwrap();
        let result = instantiate_template(&content, start_date, None, "user-123", Some(NEW_SPACE));

        assert!(result.attachment_manifest.is_empty());
        let xml = item1_description_xml(&result);
        assert!(
            xml.contains(&image_src(SRC_SPACE, ATT_A)),
            "v1 refs must pass through unmodified, got: {}",
            xml
        );

        // And no attachments Y.Array is created
        let txn = result.space_doc.transact();
        let items = txn.get_map("items").expect("items map");
        let item1 = match items.get(&txn, "item-1") {
            Some(yrs::Out::YMap(m)) => m,
            _ => panic!("item-1 missing"),
        };
        assert!(item1.get(&txn, "attachments").is_none());
    }
    // ─── Comment instantiation (WDO-180, schema v3) ───────────────────────────

    #[test]
    fn test_comments_doc_built_with_nil_author_and_rewrite() {
        let mut content = make_attachment_content();
        content.items[0].comments = vec![
            TemplateComment {
                id: "root-1".into(),
                author_name: "Alice Anders".into(),
                content_yjs: Some(make_xml_blob(|txn, frag| {
                    let img: yrs::XmlElementRef =
                        frag.push_back(txn, XmlElementPrelim::empty("image"));
                    img.insert_attribute(txn, "src", image_src(SRC_SPACE, ATT_A).as_str());
                })),
                created_at_offset_seconds: 3600,
                parent_id: None,
            },
            TemplateComment {
                id: "reply-1".into(),
                author_name: "Bob B".into(),
                content_yjs: Some(make_xml_blob(|txn, frag| {
                    let p: yrs::XmlElementRef =
                        frag.push_back(txn, XmlElementPrelim::empty("paragraph"));
                    let _: yrs::XmlTextRef = p.push_back(txn, XmlTextPrelim::new("Agreed."));
                })),
                created_at_offset_seconds: 7200,
                parent_id: Some("root-1".into()),
            },
        ];

        let start_date = NaiveDate::parse_from_str("2026-03-01", "%Y-%m-%d").unwrap();
        let result = instantiate_template(&content, start_date, None, "user-123", Some(NEW_SPACE));

        let new_att_id = result.attachment_manifest[0].new_id.clone();
        let comments_doc = result.comments_doc.expect("comments doc expected");
        let txn = comments_doc.transact();

        let threads = txn.get_map("threads").expect("threads map");
        let item_threads = match threads.get(&txn, "item-1") {
            Some(yrs::Out::YMap(m)) => m,
            _ => panic!("item-1 threads missing"),
        };

        let root = match item_threads.get(&txn, "root-1") {
            Some(yrs::Out::YMap(m)) => m,
            _ => panic!("root-1 missing"),
        };
        let read_str = |map: &yrs::MapRef, key: &str| match map.get(&txn, key) {
            Some(yrs::Out::Any(Any::String(s))) => s.to_string(),
            other => panic!("missing string field {}: {:?}", key, other),
        };
        assert_eq!(
            read_str(&root, "author_id"),
            uuid::Uuid::nil().to_string(),
            "nil author — backfill skips it, edit window never matches"
        );
        assert_eq!(read_str(&root, "author_name"), "Alice Anders");
        assert_eq!(read_str(&root, "item_id"), "item-1");
        assert_eq!(read_str(&root, "created_at"), "2026-03-01T01:00:00+00:00");
        assert!(matches!(
            root.get(&txn, "deleted"),
            Some(yrs::Out::Any(Any::Bool(false)))
        ));
        assert!(root.get(&txn, "parent_id").is_none());

        // Comment body got the attachment URL rewrite
        let body = match root.get(&txn, "content") {
            Some(yrs::Out::YXmlFragment(f)) => f.get_string(&txn),
            _ => panic!("content fragment missing"),
        };
        assert!(
            body.contains(&image_src(NEW_SPACE, &new_att_id)),
            "comment body src must be rewritten, got: {}",
            body
        );

        let reply = match item_threads.get(&txn, "reply-1") {
            Some(yrs::Out::YMap(m)) => m,
            _ => panic!("reply-1 missing"),
        };
        assert_eq!(read_str(&reply, "parent_id"), "root-1");
        assert_eq!(read_str(&reply, "created_at"), "2026-03-01T02:00:00+00:00");

        // threads_order holds roots only
        let threads_order = txn.get_map("threads_order").expect("threads_order");
        let order = match threads_order.get(&txn, "item-1") {
            Some(yrs::Out::YArray(a)) => a,
            _ => panic!("order array missing"),
        };
        let order_ids: Vec<String> = order
            .iter(&txn)
            .filter_map(|v| match v {
                yrs::Out::Any(Any::String(s)) => Some(s.to_string()),
                _ => None,
            })
            .collect();
        assert_eq!(order_ids, vec!["root-1"]);
    }

    #[test]
    fn test_no_comments_no_comments_doc() {
        let content = make_test_content();
        let start_date = NaiveDate::parse_from_str("2026-03-01", "%Y-%m-%d").unwrap();
        let result = instantiate_template(&content, start_date, None, "user-123", None);
        assert!(result.comments_doc.is_none());
    }
}
