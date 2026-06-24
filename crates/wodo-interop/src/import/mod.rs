//! Space import: SpaceExport (v2) → Yjs documents (WDO-180 Phase B)
//!
//! Rebuilds a complete space from an export archive's `data.json`: items
//! (absolute dates, shallow- and deep-archived state, preserved short IDs),
//! comments (with user mapping), documents, cycles as instances, and all
//! configuration. This is template instantiation generalized — it reuses the
//! instantiator's builders and binary decode/remap helpers.
//!
//! UUID handling mirrors instantiation: item and comment UUIDs carry over
//! (Yjs-isolated, per-space), document and attachment UUIDs are regenerated
//! (Postgres PKs / S3 keys), and the byte-level remap rewrites doc embeds,
//! inline attachment URLs, the space ID, and email-mapped user UUIDs inside
//! the rich text blobs.
//!
//! User mapping (the automatic, read-only ladder): the caller resolves the
//! export's user manifest against the target org and passes the result in
//! `ImportMaps`.
//! Unresolvable assignees are dropped; unresolvable comment authors fall back
//! to the nil UUID with the exported name snapshot (the template-comments
//! pattern — the backfill leaves it permanent).

use crate::export::types::{ExportComment, ExportItem, SpaceExport};
use crate::filter_token::remap_filters_for_import;
use crate::template::instantiator::{
    build_cycle_config, build_labels, build_views, decode_base64_to_doc,
    decode_base64_to_xml_fragment, DocumentManifestEntry,
};
use crate::template::types::{TemplateLabels, TemplateViews};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use yrs::types::map::MapPrelim;
use yrs::{Any, Array, ArrayPrelim, Doc, Map, Out, Transact, WriteTxn, XmlFragmentPrelim};

/// Resolved identity mappings, built by the caller from the export's
/// users/teams manifests against the target organization.
#[derive(Debug, Default)]
pub struct ImportMaps {
    /// Old user UUID → user UUID valid in the target org (identity entries
    /// for same-instance imports are fine and common).
    pub user_map: HashMap<String, String>,
    /// Old team UUID → team UUID valid in the target org.
    pub team_map: HashMap<String, String>,
}

/// Tally of fidelity losses during the build, turned into human-readable
/// warnings at the end.
#[derive(Default)]
struct ImportCounters {
    dropped_assignees: usize,
    dropped_teams: usize,
    /// Items/documents whose original creator has no match here; the
    /// importer is recorded as creator instead.
    fallback_creators: usize,
    dropped_doc_owners: usize,
    dropped_doc_parents: usize,
    dropped_filter_tokens: usize,
}

/// Result of building a space from an export.
pub struct ImportBuildResult {
    /// Active space data doc (items incl. shallow-archived, documents
    /// metadata, config)
    pub space_doc: Doc,
    /// Deep archive doc (deep-archived items + their comment threads);
    /// None when the export has no deep-archived items
    pub archive_doc: Option<Doc>,
    /// comments.yjs doc for active/shallow-archived items' threads
    pub comments_doc: Option<Doc>,
    /// Separate Yjs docs per document, keyed by NEW document ID
    pub document_docs: Vec<(String, Doc)>,
    /// Documents with regenerated UUIDs (for Postgres INSERT)
    pub document_manifest: Vec<DocumentManifestEntry>,
    /// Old → new attachment UUIDs (already substituted into the blobs);
    /// the run phase copies archive entries through this map
    pub attachment_id_map: HashMap<String, String>,
    /// Highest imported short_id — seed `spaces.short_id_counter` past this
    pub max_short_id: i64,
    /// Human-readable fidelity warnings for the import report
    pub warnings: Vec<String>,
}

/// Build all Yjs documents for a new space from an export snapshot.
///
/// Pure in-memory construction: no I/O. The caller persists the docs,
/// creates Postgres rows, copies attachment blobs, and seeds the short-ID
/// counter.
pub fn build_space_from_export(
    export: &SpaceExport,
    new_space_id: &str,
    importer_id: &str,
    maps: &ImportMaps,
) -> ImportBuildResult {
    let mut warnings: Vec<String> = Vec::new();

    // Fresh UUIDs for documents (Postgres PKs) and attachments (S3 keys +
    // attachments rows)
    let doc_id_map: HashMap<String, String> = export
        .documents
        .iter()
        .map(|d| (d.id.clone(), uuid::Uuid::new_v4().to_string()))
        .collect();
    let attachment_id_map: HashMap<String, String> = export
        .attachments
        .iter()
        .map(|a| (a.id.clone(), uuid::Uuid::new_v4().to_string()))
        .collect();

    // Byte-substitution map for rich text blobs: doc embeds, inline
    // attachment URLs (attachment UUID + source space UUID), and
    // remapped user UUIDs (mentions, embedded references). Only well-formed
    // UUIDs may enter it — the byte-level remap hard-asserts 36-byte keys,
    // and a hand-crafted export with a non-UUID id must not panic the
    // import task (the id still gets regenerated; it just can't be
    // referenced inside rich text, which a non-UUID id never is).
    let is_uuid = |s: &str| uuid::Uuid::parse_str(s).is_ok();
    let mut blob_uuid_map: HashMap<String, String> = doc_id_map
        .iter()
        .chain(attachment_id_map.iter())
        .filter(|(old, _)| is_uuid(old))
        .map(|(old, new)| (old.clone(), new.clone()))
        .collect();
    if export.space.id != new_space_id && is_uuid(&export.space.id) {
        blob_uuid_map.insert(export.space.id.clone(), new_space_id.to_string());
    }
    for (old, new) in &maps.user_map {
        if old != new && is_uuid(old) {
            blob_uuid_map.insert(old.clone(), new.clone());
        }
    }

    let now = chrono::Utc::now();
    let now_str = now.to_rfc3339();
    let now_ms = now.timestamp_millis();

    let mut counters = ImportCounters::default();
    let mut unmapped_authors: HashSet<String> = HashSet::new();

    // Existence sets for validating document parent links (item and
    // milestone UUIDs carry over verbatim, so presence in the export is the
    // only thing to check).
    let item_ids: HashSet<&str> = export.items.iter().map(|i| i.id.as_str()).collect();
    let milestone_ids: HashSet<&str> = export
        .milestones
        .definitions
        .keys()
        .map(|k| k.as_str())
        .collect();

    // ── Active space doc ─────────────────────────────────────────────────
    let space_doc = Doc::new();
    {
        let mut txn = space_doc.transact_mut();
        let config = txn.get_or_insert_map("config");

        let remapped_labels = remap_label_deprecators(&export.labels, maps);
        build_labels(&mut txn, &config, &remapped_labels);
        build_milestones_absolute(&mut txn, &config, export);
        // View filters survive an import (unlike template instantiation):
        // cycle/label/milestone tokens carry over verbatim, user/team tokens
        // are remapped through the mapping ladder (unmappable ones dropped).
        let remapped_views = remap_view_filters(&export.views, maps, &mut counters);
        build_views(&mut txn, &config, &remapped_views, &|f| {
            if f.is_empty() {
                None
            } else {
                Some(f.to_string())
            }
        });
        build_cycle_config(&mut txn, &config, export.cycle_config.as_ref());
        build_cycles_instances(&mut txn, &config, export);

        if let Some(ref prefix) = export.space.short_id_prefix {
            config.insert(
                &mut txn,
                Arc::<str>::from("short_id_prefix"),
                Any::String(Arc::from(prefix.as_str())),
            );
        }
        config.insert(
            &mut txn,
            Arc::<str>::from("short_id_visible"),
            Any::Bool(export.space.short_id_visible),
        );
        if let Some(ref ac) = export.archive_config {
            let ac_map: yrs::MapRef =
                config.insert(&mut txn, "archive_config", MapPrelim::default());
            ac_map.insert(
                &mut txn,
                Arc::<str>::from("migration_days"),
                Any::Number(ac.migration_days as f64),
            );
        }

        // Assignee display cache: the client resolves assignee user/team ids to
        // names/avatars through config.assignee_cache (see
        // client/src/collab/assignee_cache.gleam). Without it every imported
        // assignee renders as "Unknown user", so seed it from the export's
        // user/team manifest.
        build_assignee_cache(&mut txn, &config, export, maps, &now_str);

        // Active + shallow-archived items
        for item in export.items.iter().filter(|i| !i.deep_archived) {
            write_item(
                &mut txn,
                item,
                importer_id,
                &now_str,
                now_ms,
                &blob_uuid_map,
                maps,
                &mut counters,
            );
        }

        // Documents metadata (new UUIDs)
        build_documents_metadata(
            &mut txn,
            export,
            &doc_id_map,
            importer_id,
            &now_str,
            maps,
            &item_ids,
            &milestone_ids,
            &mut counters,
        );

        // Overview
        if let Some(ref overview_b64) = export.overview_yjs {
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

    // ── Deep archive doc ─────────────────────────────────────────────────
    let deep_items: Vec<&ExportItem> = export.items.iter().filter(|i| i.deep_archived).collect();
    let archive_doc = if deep_items.is_empty() {
        None
    } else {
        let doc = Doc::new();
        {
            let mut txn = doc.transact_mut();
            for item in &deep_items {
                write_item(
                    &mut txn,
                    item,
                    importer_id,
                    &now_str,
                    now_ms,
                    &blob_uuid_map,
                    maps,
                    &mut counters,
                );
            }
            // Deep-archived items' comment threads live inside the archive doc
            write_comment_threads(
                &mut txn,
                deep_items.iter().copied(),
                maps,
                &blob_uuid_map,
                &mut unmapped_authors,
            );
        }
        Some(doc)
    };

    // ── comments.yjs (active/shallow items) ──────────────────────────────
    let active_with_comments: Vec<&ExportItem> = export
        .items
        .iter()
        .filter(|i| !i.deep_archived && !i.comments.is_empty())
        .collect();
    let comments_doc = if active_with_comments.is_empty() {
        None
    } else {
        let doc = Doc::new();
        {
            let mut txn = doc.transact_mut();
            write_comment_threads(
                &mut txn,
                active_with_comments.iter().copied(),
                maps,
                &blob_uuid_map,
                &mut unmapped_authors,
            );
        }
        Some(doc)
    };

    // ── Per-document content docs ────────────────────────────────────────
    let document_docs: Vec<(String, Doc)> = export
        .documents
        .iter()
        .filter_map(|d| {
            let content_b64 = d.content_yjs.as_deref()?;
            let new_id = doc_id_map.get(&d.id)?;
            let doc = decode_base64_to_doc(content_b64, &blob_uuid_map).ok()?;
            Some((new_id.clone(), doc))
        })
        .collect();

    let document_manifest = export
        .documents
        .iter()
        .filter_map(|d| {
            let new_id = doc_id_map.get(&d.id)?;
            Some(DocumentManifestEntry {
                new_id: new_id.clone(),
                title: d.title.clone(),
            })
        })
        .collect();

    let max_short_id = export
        .items
        .iter()
        .filter_map(|i| i.short_id)
        .max()
        .unwrap_or(0);

    if counters.dropped_assignees > 0 {
        warnings.push(format!(
            "{} item assignments referenced users with no match in this organization and were dropped",
            counters.dropped_assignees
        ));
    }
    if counters.dropped_teams > 0 {
        warnings.push(format!(
            "{} team assignments referenced teams with no match in this organization and were dropped",
            counters.dropped_teams
        ));
    }
    if !unmapped_authors.is_empty() {
        warnings.push(format!(
            "{} comment authors had no match in this organization; their comments keep the exported name without an account",
            unmapped_authors.len()
        ));
    }
    if counters.fallback_creators > 0 {
        warnings.push(format!(
            "{} items or documents were created by users with no match in this organization; the importer is recorded as creator",
            counters.fallback_creators
        ));
    }
    if counters.dropped_doc_owners > 0 {
        warnings.push(format!(
            "{} document owners had no match in this organization and were dropped",
            counters.dropped_doc_owners
        ));
    }
    if counters.dropped_doc_parents > 0 {
        warnings.push(format!(
            "{} document parent links referenced items or milestones missing from the export and were dropped",
            counters.dropped_doc_parents
        ));
    }
    if counters.dropped_filter_tokens > 0 {
        warnings.push(format!(
            "{} saved-view filters referenced people or teams with no match in this organization and were removed",
            counters.dropped_filter_tokens
        ));
    }

    ImportBuildResult {
        space_doc,
        archive_doc,
        comments_doc,
        document_docs,
        document_manifest,
        attachment_id_map,
        max_short_id,
        warnings,
    }
}

/// Write one item into a doc's `items` map + `items_order` array.
#[allow(clippy::too_many_arguments)]
fn write_item(
    txn: &mut yrs::TransactionMut,
    item: &ExportItem,
    importer_id: &str,
    now_str: &str,
    now_ms: i64,
    blob_uuid_map: &HashMap<String, String>,
    maps: &ImportMaps,
    counters: &mut ImportCounters,
) {
    let items_map = txn.get_or_insert_map("items");
    let items_order: yrs::ArrayRef = txn.get_or_insert_array("items_order");

    let ymap: yrs::MapRef = items_map.insert(txn, item.id.as_str(), MapPrelim::default());
    fn set_str(txn: &mut yrs::TransactionMut, m: &yrs::MapRef, k: &str, v: &str) {
        m.insert(txn, Arc::<str>::from(k), Any::String(Arc::from(v)));
    }

    set_str(txn, &ymap, "id", &item.id);
    set_str(txn, &ymap, "title", &item.title);

    if let Some(short_id) = item.short_id {
        // Preserved: the space is new so there are no collisions, and the
        // short-ID counter is seeded past the max by the run phase.
        // Stored as Any::Number (f64) to match the persistence backfill
        // (doc_operations::set_item_short_id) — the Gleam client decodes
        // short_id with `decode.int`, which rejects a Yjs BigInt.
        ymap.insert(
            txn,
            Arc::<str>::from("short_id"),
            Any::Number(short_id as f64),
        );
    }

    let labels: yrs::MapRef = ymap.insert(txn, "labels", MapPrelim::default());
    for (label_id, value_id) in &item.labels {
        labels.insert(
            txn,
            Arc::<str>::from(label_id.as_str()),
            Any::String(Arc::from(value_id.as_str())),
        );
    }

    for (key, value) in [
        ("due_date", &item.due_date),
        ("start_date", &item.start_date),
        ("milestone_id", &item.milestone_id),
        ("cycle_id", &item.cycle_id),
        ("parent_id", &item.parent_id),
        ("duplicate_of", &item.duplicate_of),
    ] {
        if let Some(v) = value {
            set_str(txn, &ymap, key, v);
        }
    }

    if !item.blocked_by.is_empty() {
        let arr: yrs::ArrayRef = ymap.insert(txn, "blocked_by", ArrayPrelim::default());
        for blocker in &item.blocked_by {
            arr.push_back(txn, Any::String(Arc::from(blocker.as_str())));
        }
    }

    // Assignees through the mapping ladder: unresolvable refs are dropped
    let mapped_users: Vec<String> = item
        .assignee_user_ids
        .iter()
        .filter_map(|uid| match maps.user_map.get(uid) {
            Some(new) => Some(new.clone()),
            None => {
                counters.dropped_assignees += 1;
                None
            }
        })
        .collect();
    let mapped_teams: Vec<String> = item
        .assignee_team_ids
        .iter()
        .filter_map(|tid| match maps.team_map.get(tid) {
            Some(new) => Some(new.clone()),
            None => {
                counters.dropped_teams += 1;
                None
            }
        })
        .collect();
    if !mapped_users.is_empty() || !mapped_teams.is_empty() {
        let assignees: yrs::MapRef = ymap.insert(txn, "assignees", MapPrelim::default());
        let users_arr: yrs::ArrayRef = assignees.insert(txn, "users", ArrayPrelim::default());
        for uid in &mapped_users {
            users_arr.push_back(txn, Any::String(Arc::from(uid.as_str())));
        }
        let teams_arr: yrs::ArrayRef = assignees.insert(txn, "teams", ArrayPrelim::default());
        for tid in &mapped_teams {
            teams_arr.push_back(txn, Any::String(Arc::from(tid.as_str())));
        }
    }

    if item.archived {
        ymap.insert(txn, Arc::<str>::from("archived"), Any::Bool(true));
        // Preserve the original archive time when the export carries it
        // (internally epoch millis); older exports restart the clock now.
        // A preserved old archived_at means the deep-archive sweep may move
        // the item soon after import — correct: it genuinely is that old.
        let archived_ms = item
            .archived_at
            .as_deref()
            .and_then(parse_rfc3339_millis)
            .unwrap_or(now_ms);
        ymap.insert(
            txn,
            Arc::<str>::from("archived_at"),
            Any::BigInt(archived_ms),
        );
    }

    if let Some(ref desc_b64) = item.description_yjs {
        let frag: yrs::XmlFragmentRef =
            ymap.insert(txn, "description", XmlFragmentPrelim::default());
        let _ = decode_base64_to_xml_fragment(desc_b64, "content", txn, &frag, blob_uuid_map);
    }

    // Completion snapshot (the note can contain mentions → blob remap)
    if let Some(ref prompt) = item.completion_prompt {
        set_str(txn, &ymap, "completion_prompt", prompt);
    }
    if let Some(ref note_b64) = item.completion_note_yjs {
        let frag: yrs::XmlFragmentRef =
            ymap.insert(txn, "completion_note", XmlFragmentPrelim::default());
        let _ = decode_base64_to_xml_fragment(note_b64, "content", txn, &frag, blob_uuid_map);
    }

    // Provenance: preserved when the export carries it (creator through the
    // mapping ladder, importer fallback); importer + now otherwise.
    let created_by = mapped_creator_or_importer(&item.created_by, maps, importer_id, counters);
    set_str(txn, &ymap, "created_by", created_by);
    set_str(
        txn,
        &ymap,
        "created_at",
        item.created_at.as_deref().unwrap_or(now_str),
    );
    set_str(
        txn,
        &ymap,
        "updated_at",
        item.updated_at.as_deref().unwrap_or(now_str),
    );

    items_order.push_back(txn, Any::String(Arc::from(item.id.as_str())));
}

/// RFC 3339 timestamp string → epoch millis (the internal `archived_at`
/// representation on items).
fn parse_rfc3339_millis(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp_millis())
}

/// Resolve a preserved `created_by` through the user map; unmappable (or
/// absent) creators fall back to the importer. Items and documents have no
/// creator-name snapshot, so the comment-style nil-UUID fallback isn't
/// available here.
fn mapped_creator_or_importer<'a>(
    created_by: &'a Option<String>,
    maps: &'a ImportMaps,
    importer_id: &'a str,
    counters: &mut ImportCounters,
) -> &'a str {
    match created_by.as_deref() {
        Some(orig) => match maps.user_map.get(orig) {
            Some(new) => new.as_str(),
            None => {
                counters.fallback_creators += 1;
                importer_id
            }
        },
        None => importer_id,
    }
}

/// Write comment threads (threads + threads_order maps) for the given items
/// into a doc. Comment UUIDs and absolute timestamps carry over; authors go
/// through the mapping ladder with the nil-UUID + name-snapshot fallback.
fn write_comment_threads<'a>(
    txn: &mut yrs::TransactionMut,
    items: impl Iterator<Item = &'a ExportItem>,
    maps: &ImportMaps,
    blob_uuid_map: &HashMap<String, String>,
    unmapped_authors: &mut HashSet<String>,
) {
    let nil_author = uuid::Uuid::nil().to_string();
    let threads = txn.get_or_insert_map("threads");
    let threads_order = txn.get_or_insert_map("threads_order");

    for item in items {
        if item.comments.is_empty() {
            continue;
        }
        let item_threads: yrs::MapRef = threads.insert(txn, item.id.as_str(), MapPrelim::default());
        let order: yrs::ArrayRef =
            threads_order.insert(txn, item.id.as_str(), ArrayPrelim::default());

        for comment in &item.comments {
            write_comment(
                txn,
                &item_threads,
                item.id.as_str(),
                comment,
                maps,
                blob_uuid_map,
                &nil_author,
                unmapped_authors,
            );
            if comment.parent_id.is_none() && !comment.deleted {
                order.push_back(txn, Any::String(Arc::from(comment.id.as_str())));
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn write_comment(
    txn: &mut yrs::TransactionMut,
    item_threads: &yrs::MapRef,
    item_id: &str,
    comment: &ExportComment,
    maps: &ImportMaps,
    blob_uuid_map: &HashMap<String, String>,
    nil_author: &str,
    unmapped_authors: &mut HashSet<String>,
) {
    let cmap: yrs::MapRef = item_threads.insert(txn, comment.id.as_str(), MapPrelim::default());
    fn set_str(txn: &mut yrs::TransactionMut, m: &yrs::MapRef, k: &str, v: &str) {
        m.insert(txn, Arc::<str>::from(k), Any::String(Arc::from(v)));
    }

    set_str(txn, &cmap, "id", &comment.id);
    set_str(txn, &cmap, "item_id", item_id);

    let author_id = match maps.user_map.get(&comment.author_id) {
        Some(new) => new.as_str(),
        None => {
            unmapped_authors.insert(comment.author_id.clone());
            nil_author
        }
    };
    set_str(txn, &cmap, "author_id", author_id);
    set_str(
        txn,
        &cmap,
        "author_name",
        comment.author_name.as_deref().unwrap_or("Unknown user"),
    );
    if let Some(ref created_at) = comment.created_at {
        set_str(txn, &cmap, "created_at", created_at);
    }
    if let Some(ref edited_at) = comment.edited_at {
        set_str(txn, &cmap, "edited_at", edited_at);
    }
    cmap.insert(txn, Arc::<str>::from("deleted"), Any::Bool(comment.deleted));
    if let Some(ref deleted_at) = comment.deleted_at {
        set_str(txn, &cmap, "deleted_at", deleted_at);
    }
    if let Some(ref parent_id) = comment.parent_id {
        set_str(txn, &cmap, "parent_id", parent_id);
    }

    let frag: yrs::XmlFragmentRef = cmap.insert(txn, "content", XmlFragmentPrelim::default());
    if let Some(ref content_b64) = comment.content_yjs {
        let _ = decode_base64_to_xml_fragment(content_b64, "content", txn, &frag, blob_uuid_map);
    }
}

/// Seed the assignee display cache (`config.assignee_cache`) from the export's
/// user/team manifest. The client reads names/avatars from here to render
/// assignee chips (client/src/collab/assignee_cache.gleam); an import that skips
/// it shows every assignee as "Unknown user" until the user is re-picked.
///
/// Ids go through the mapping ladder — unresolvable entries are omitted, the
/// same policy as the assignee drop — and `removed_at` is left empty (active).
/// The display name is the export's snapshot (no avatar URL travels in the
/// manifest); a later re-pick refreshes both from the live members list.
fn build_assignee_cache(
    txn: &mut yrs::TransactionMut,
    config: &yrs::MapRef,
    export: &SpaceExport,
    maps: &ImportMaps,
    now_str: &str,
) {
    fn set_str(txn: &mut yrs::TransactionMut, m: &yrs::MapRef, k: &str, v: &str) {
        m.insert(txn, Arc::<str>::from(k), Any::String(Arc::from(v)));
    }

    let cache: yrs::MapRef = config.insert(txn, "assignee_cache", MapPrelim::default());

    let users: yrs::MapRef = cache.insert(txn, "users", MapPrelim::default());
    for u in &export.users {
        let Some(mapped) = maps.user_map.get(&u.id) else {
            continue;
        };
        let entry: yrs::MapRef = users.insert(txn, mapped.as_str(), MapPrelim::default());
        set_str(txn, &entry, "name", &u.display_name);
        set_str(txn, &entry, "avatar_url", "");
        set_str(txn, &entry, "cached_at", now_str);
        set_str(txn, &entry, "removed_at", "");
    }

    let teams: yrs::MapRef = cache.insert(txn, "teams", MapPrelim::default());
    for t in &export.teams {
        let Some(mapped) = maps.team_map.get(&t.id) else {
            continue;
        };
        let entry: yrs::MapRef = teams.insert(txn, mapped.as_str(), MapPrelim::default());
        set_str(txn, &entry, "name", &t.name);
        entry.insert(txn, Arc::<str>::from("member_count"), Any::Number(0.0));
        set_str(txn, &entry, "cached_at", now_str);
        set_str(txn, &entry, "removed_at", "");
    }
}

/// Milestones with absolute deadlines (export carries dates, not offsets).
/// Deprecated milestones are restored with their flag.
fn build_milestones_absolute(
    txn: &mut yrs::TransactionMut,
    config: &yrs::MapRef,
    export: &SpaceExport,
) {
    let ms_map: yrs::MapRef = config.insert(txn, "milestones", MapPrelim::default());
    let ms_order: yrs::ArrayRef = config.insert(txn, "milestones_order", ArrayPrelim::default());

    for id in &export.milestones.order {
        let Some(def) = export.milestones.definitions.get(id) else {
            continue;
        };
        let ms: yrs::MapRef = ms_map.insert(txn, def.id.as_str(), MapPrelim::default());
        ms.insert(
            txn,
            Arc::<str>::from("id"),
            Any::String(Arc::from(def.id.as_str())),
        );
        ms.insert(
            txn,
            Arc::<str>::from("name"),
            Any::String(Arc::from(def.name.as_str())),
        );
        if let Some(ref desc) = def.description {
            ms.insert(
                txn,
                Arc::<str>::from("description"),
                Any::String(Arc::from(desc.as_str())),
            );
        }
        if let Some(ref deadline) = def.deadline {
            ms.insert(
                txn,
                Arc::<str>::from("deadline"),
                Any::String(Arc::from(deadline.as_str())),
            );
        }
        if def.deprecated {
            ms.insert(txn, Arc::<str>::from("deprecated"), Any::Bool(true));
        }
        ms_order.push_back(txn, Any::String(Arc::from(def.id.as_str())));
    }
}

/// Cycle instances (export carries them; templates don't). The generator's
/// next_number continues past the highest existing cycle number so generation
/// never reissues a name that's already seeded.
fn build_cycles_instances(
    txn: &mut yrs::TransactionMut,
    config: &yrs::MapRef,
    export: &SpaceExport,
) {
    if export.cycles.is_empty() {
        return;
    }
    let cycles_map: yrs::MapRef = config.insert(txn, "cycles", MapPrelim::default());
    for cycle in &export.cycles {
        let c: yrs::MapRef = cycles_map.insert(txn, cycle.id.as_str(), MapPrelim::default());
        c.insert(
            txn,
            Arc::<str>::from("id"),
            Any::String(Arc::from(cycle.id.as_str())),
        );
        c.insert(
            txn,
            Arc::<str>::from("name"),
            Any::String(Arc::from(cycle.name.as_str())),
        );
        c.insert(
            txn,
            Arc::<str>::from("start_date"),
            Any::String(Arc::from(cycle.start_date.as_str())),
        );
        c.insert(
            txn,
            Arc::<str>::from("end_date"),
            Any::String(Arc::from(cycle.end_date.as_str())),
        );
        if cycle.archived {
            c.insert(txn, Arc::<str>::from("archived"), Any::Bool(true));
        }
    }
    if let Some(Out::YMap(cc)) = config.get(txn, "cycle_config") {
        // next_number names the first GENERATED cycle. Continue past the highest
        // number already present (parsed from the trailing integer of each name,
        // e.g. "Sprint 4" → 4) so generation doesn't reissue a seeded name. Fall
        // back to count+1 when no name carries a number.
        let next_number = export
            .cycles
            .iter()
            .filter_map(|c| trailing_number(&c.name))
            .max()
            .map(|n| n + 1)
            .unwrap_or(export.cycles.len() as i64 + 1);
        cc.insert(
            txn,
            Arc::<str>::from("next_number"),
            Any::Number(next_number as f64),
        );
    }
}

/// The trailing integer of a cycle name ("Sprint 12" → 12), if any. Used to
/// continue generated-cycle numbering past seeded cycles.
fn trailing_number(name: &str) -> Option<i64> {
    let digits: String = name
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        None
    } else {
        digits.chars().rev().collect::<String>().parse().ok()
    }
}

/// Clone the export's labels with `deprecated_by` mapped through the user
/// ladder. An unmappable deprecator is omitted while the deprecation flag
/// and timestamp stay — that it's deprecated matters more than who did it.
fn remap_label_deprecators(labels: &TemplateLabels, maps: &ImportMaps) -> TemplateLabels {
    let mut labels = labels.clone();
    for def in labels.definitions.values_mut() {
        def.deprecated_by = def
            .deprecated_by
            .take()
            .and_then(|by| maps.user_map.get(&by).cloned());
        for val in def.values.values_mut() {
            val.deprecated_by = val
                .deprecated_by
                .take()
                .and_then(|by| maps.user_map.get(&by).cloned());
        }
    }
    labels
}

/// Clone the export's views with `u:`/`t:` filter tokens remapped through
/// the mapping ladder (unmappable tokens dropped and counted); all other
/// tokens carry over verbatim — cycles, labels, and milestones keep their
/// UUIDs on import.
fn remap_view_filters(
    views: &TemplateViews,
    maps: &ImportMaps,
    counters: &mut ImportCounters,
) -> TemplateViews {
    let mut views = views.clone();
    for def in views.definitions.values_mut() {
        if let Some(filters) = def.filters.take() {
            def.filters = remap_filters_for_import(
                &filters,
                &maps.user_map,
                &maps.team_map,
                &mut counters.dropped_filter_tokens,
            );
        }
    }
    views
}

/// Documents metadata in the space doc (new UUIDs; content lives in the
/// separate per-document docs). Restores the full metadata set: owners
/// (through the mapping ladder), parent links (validated against the
/// export), review cadence, the template flag, lineage, and provenance.
#[allow(clippy::too_many_arguments)]
fn build_documents_metadata(
    txn: &mut yrs::TransactionMut,
    export: &SpaceExport,
    doc_id_map: &HashMap<String, String>,
    importer_id: &str,
    now_str: &str,
    maps: &ImportMaps,
    item_ids: &HashSet<&str>,
    milestone_ids: &HashSet<&str>,
    counters: &mut ImportCounters,
) {
    if export.documents.is_empty() {
        return;
    }
    let docs_map = txn.get_or_insert_map("documents");
    let docs_order: yrs::ArrayRef = txn.get_or_insert_array("documents_order");

    for doc in &export.documents {
        let Some(new_id) = doc_id_map.get(&doc.id) else {
            continue;
        };
        let dmap: yrs::MapRef = docs_map.insert(txn, new_id.as_str(), MapPrelim::default());
        fn set_str(txn: &mut yrs::TransactionMut, m: &yrs::MapRef, k: &str, v: &str) {
            m.insert(txn, Arc::<str>::from(k), Any::String(Arc::from(v)));
        }
        set_str(txn, &dmap, "id", new_id);
        set_str(txn, &dmap, "title", &doc.title);
        let labels: yrs::MapRef = dmap.insert(txn, "labels", MapPrelim::default());
        for (label_id, value_id) in &doc.labels {
            labels.insert(
                txn,
                Arc::<str>::from(label_id.as_str()),
                Any::String(Arc::from(value_id.as_str())),
            );
        }
        if doc.archived {
            dmap.insert(txn, Arc::<str>::from("archived"), Any::Bool(true));
            if let Some(ref archived_at) = doc.archived_at {
                set_str(txn, &dmap, "archived_at", archived_at);
            }
        }

        // Owners through the mapping ladder (same policy as assignees)
        let owner_users: Vec<&String> = doc
            .owner_user_ids
            .iter()
            .filter_map(|uid| match maps.user_map.get(uid) {
                Some(new) => Some(new),
                None => {
                    counters.dropped_doc_owners += 1;
                    None
                }
            })
            .collect();
        let owner_teams: Vec<&String> = doc
            .owner_team_ids
            .iter()
            .filter_map(|tid| match maps.team_map.get(tid) {
                Some(new) => Some(new),
                None => {
                    counters.dropped_doc_owners += 1;
                    None
                }
            })
            .collect();
        if !owner_users.is_empty() || !owner_teams.is_empty() {
            let owners: yrs::MapRef = dmap.insert(txn, "owners", MapPrelim::default());
            let users_arr: yrs::ArrayRef = owners.insert(txn, "users", ArrayPrelim::default());
            for uid in &owner_users {
                users_arr.push_back(txn, Any::String(Arc::from(uid.as_str())));
            }
            let teams_arr: yrs::ArrayRef = owners.insert(txn, "teams", ArrayPrelim::default());
            for tid in &owner_teams {
                teams_arr.push_back(txn, Any::String(Arc::from(tid.as_str())));
            }
        }

        // Parent links: item/milestone UUIDs carry over verbatim, but only
        // when the anchor actually exists in the export
        if let Some(ref parent_item) = doc.parent_item_id {
            if item_ids.contains(parent_item.as_str()) {
                set_str(txn, &dmap, "parent_item_id", parent_item);
            } else {
                counters.dropped_doc_parents += 1;
            }
        }
        if let Some(ref parent_ms) = doc.parent_milestone_id {
            if milestone_ids.contains(parent_ms.as_str()) {
                set_str(txn, &dmap, "parent_milestone_id", parent_ms);
            } else {
                counters.dropped_doc_parents += 1;
            }
        }

        if let Some(days) = doc.review_cadence_days {
            dmap.insert(
                txn,
                Arc::<str>::from("review_cadence_days"),
                Any::Number(days as f64),
            );
        }
        if let Some(ref reviewed) = doc.last_reviewed_at {
            set_str(txn, &dmap, "last_reviewed_at", reviewed);
        }
        if doc.is_template {
            dmap.insert(txn, Arc::<str>::from("is_template"), Any::Bool(true));
        }
        // Lineage through the regenerated doc UUIDs; a fork source absent
        // from the export is meaningless lineage — dropped silently
        if let Some(new_fork) = doc.forked_from.as_ref().and_then(|f| doc_id_map.get(f)) {
            set_str(txn, &dmap, "forked_from", new_fork);
        }

        let created_by = mapped_creator_or_importer(&doc.created_by, maps, importer_id, counters);
        set_str(txn, &dmap, "created_by", created_by);
        set_str(
            txn,
            &dmap,
            "created_at",
            doc.created_at.as_deref().unwrap_or(now_str),
        );
        set_str(
            txn,
            &dmap,
            "updated_at",
            doc.updated_at.as_deref().unwrap_or(now_str),
        );
        docs_order.push_back(txn, Any::String(Arc::from(new_id.as_str())));
    }
}

/// External-tool → SpaceExport converters (Linear, …). Pure, DB-free.
pub mod convert;

/// Fetch a Linear team over GraphQL and package it as an import-wizard ZIP.
pub mod linear_fetch;

/// Fetch a Jira project over REST and package it as an import-wizard ZIP.
pub mod jira_fetch;

/// Truncate `s` to at most `n` bytes for inclusion in an error/log message,
/// appending `…` when shortened. Backs up to a UTF-8 char boundary first, so a
/// non-ASCII byte at offset `n` (accented text or an emoji in an error body)
/// can't split a multibyte sequence and panic. Shared by the fetchers.
pub(crate) fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut end = n;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::truncate;

    #[test]
    fn truncate_backs_up_to_char_boundary() {
        assert_eq!(truncate("hello", 10), "hello"); // within limit → unchanged
        assert_eq!(truncate("é", 1), "…"); // byte 1 splits é → back up to 0
        assert_eq!(truncate("a😀b", 2), "a…"); // byte 2 splits the emoji → "a"
    }
}
