//! Atlassian Document Format (ADF) → Markdown walker.
//!
//! Pure, DB-free. A recursive walker over the JSON ADF tree Jira returns for
//! issue descriptions and comment bodies, emitting Markdown that
//! [`crate::markdown::write_markdown_to_fragment`] then consumes (it handles
//! headings, lists, tables, code, blockquote, bold/italic/strike/code, links,
//! images, and `[[WDO-n]]` item-refs). The walker is total: unknown node types
//! recurse into their children and never panic.
//!
//! Inline images (`media` nodes) emit a `jira-media:<id>` sentinel URL; the Jira
//! converter ([`crate::import::convert::jira`]) resolves that against the
//! issue's `fields.attachment[]` after the fact.

use serde_json::Value;

/// Convert an ADF document (the `description`/`body` JSON value) into Markdown.
///
/// Accepts any ADF node; the typical entry is a `doc` node. Returns Markdown
/// with blocks separated by blank lines and a trailing newline trimmed.
pub fn adf_to_markdown(adf: &Value) -> String {
    let mut out = String::new();
    walk_block_container(adf, &mut out);
    // Normalize: collapse 3+ blank lines to one, trim trailing whitespace.
    out.trim_end().to_string()
}

/// Walk a node treated as a container of block-level children, appending each
/// block followed by a blank line. The node itself may be a `doc`, a `panel`,
/// a list item, a table cell, etc. — anything whose `content` is a sequence of
/// blocks.
fn walk_block_container(node: &Value, out: &mut String) {
    let Some(content) = node.get("content").and_then(Value::as_array) else {
        // No block children — if it's a bare text/inline node, render inline.
        let inline = render_inline_nodes(std::slice::from_ref(node));
        if !inline.is_empty() {
            push_block(out, &inline);
        }
        return;
    };
    for child in content {
        walk_block(child, out);
    }
}

/// Append a finished block to `out`, ensuring blocks are blank-line separated.
fn push_block(out: &mut String, block: &str) {
    if block.is_empty() {
        return;
    }
    if !out.is_empty() {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
    }
    out.push_str(block);
}

/// Walk a single block-level node and append its Markdown to `out`.
fn walk_block(node: &Value, out: &mut String) {
    let node_type = node.get("type").and_then(Value::as_str).unwrap_or("");
    match node_type {
        "heading" => {
            let level = node
                .get("attrs")
                .and_then(|a| a.get("level"))
                .and_then(Value::as_u64)
                .unwrap_or(1)
                .clamp(1, 6) as usize;
            let text = render_inline_nodes(child_nodes(node));
            push_block(out, &format!("{} {}", "#".repeat(level), text));
        }
        "paragraph" => {
            let text = render_inline_nodes(child_nodes(node));
            // A paragraph may carry only a media node etc.; emit even if it has
            // block-ish inline content, but skip truly empty paragraphs.
            push_block(out, &text);
        }
        "bulletList" => {
            let block = render_list(node, false);
            push_block(out, &block);
        }
        "orderedList" => {
            let block = render_list(node, true);
            push_block(out, &block);
        }
        "codeBlock" => {
            let lang = node
                .get("attrs")
                .and_then(|a| a.get("language"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let code = collect_plain_text(node);
            push_block(out, &format!("```{lang}\n{code}\n```"));
        }
        "panel" | "blockquote" => {
            // panel{panelType} and blockquote both render as a Markdown
            // blockquote: each contained block's lines get a `> ` prefix.
            let mut inner = String::new();
            walk_block_container(node, &mut inner);
            let quoted = inner
                .trim_end()
                .lines()
                .map(|l| {
                    if l.is_empty() {
                        ">".to_string()
                    } else {
                        format!("> {l}")
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            push_block(out, &quoted);
        }
        "table" => {
            let block = render_table(node);
            push_block(out, &block);
        }
        "rule" => {
            push_block(out, "---");
        }
        "mediaSingle" | "mediaGroup" => {
            // Media wrappers — render their media children as image blocks.
            let text = render_inline_nodes(child_nodes(node));
            push_block(out, &text);
        }
        "" => {
            // Untyped — recurse defensively.
            walk_block_container(node, out);
        }
        _ => {
            // Unknown block type: if it has block children, recurse; otherwise
            // treat it as inline so its text survives.
            if node.get("content").and_then(Value::as_array).is_some() {
                let inline = render_inline_nodes(child_nodes(node));
                // Heuristic: if rendering inline produced nothing block-like,
                // fall back to descending as blocks.
                if inline.trim().is_empty() {
                    walk_block_container(node, out);
                } else {
                    push_block(out, &inline);
                }
            } else {
                let inline = render_inline_nodes(std::slice::from_ref(node));
                push_block(out, &inline);
            }
        }
    }
}

/// Render a bullet/ordered list node to Markdown (one line per `listItem`).
/// Each list item's first paragraph supplies the line; nested blocks beyond the
/// first paragraph are flattened with a space (the markdown writer is flat).
fn render_list(node: &Value, ordered: bool) -> String {
    let mut lines = Vec::new();
    let mut n = 1;
    for item in child_nodes(node) {
        if item.get("type").and_then(Value::as_str) != Some("listItem") {
            continue;
        }
        // Join all block children of the list item into one line of inline text.
        let mut parts = Vec::new();
        for block in child_nodes(item) {
            let text = render_inline_nodes(child_nodes(block));
            if !text.is_empty() {
                parts.push(text);
            }
        }
        let text = parts.join(" ");
        if ordered {
            lines.push(format!("{n}. {text}"));
            n += 1;
        } else {
            lines.push(format!("- {text}"));
        }
    }
    lines.join("\n")
}

/// Render an ADF `table` to a GFM table. The first row becomes the header
/// (whether its cells are `tableHeader` or `tableCell`); a separator row is
/// synthesized; remaining rows are data rows.
fn render_table(node: &Value) -> String {
    let rows: Vec<&Value> = child_nodes(node)
        .iter()
        .filter(|r| r.get("type").and_then(Value::as_str) == Some("tableRow"))
        .collect();
    if rows.is_empty() {
        return String::new();
    }

    let render_row = |row: &Value| -> Vec<String> {
        child_nodes(row)
            .iter()
            .filter(|c| {
                matches!(
                    c.get("type").and_then(Value::as_str),
                    Some("tableHeader") | Some("tableCell")
                )
            })
            .map(|cell| {
                // A cell holds blocks; flatten to a single inline string and
                // escape pipes so the GFM table stays well-formed.
                let mut parts = Vec::new();
                for block in child_nodes(cell) {
                    let text = render_inline_nodes(child_nodes(block));
                    if !text.is_empty() {
                        parts.push(text);
                    }
                }
                parts.join(" ").replace('|', "\\|")
            })
            .collect()
    };

    let header = render_row(rows[0]);
    let num_cols = header.len().max(1);
    let mut lines = Vec::new();
    lines.push(format!("| {} |", pad_cells(&header, num_cols).join(" | ")));
    lines.push(format!(
        "| {} |",
        std::iter::repeat_n("---", num_cols)
            .collect::<Vec<_>>()
            .join(" | ")
    ));
    for row in &rows[1..] {
        let cells = render_row(row);
        lines.push(format!("| {} |", pad_cells(&cells, num_cols).join(" | ")));
    }
    lines.join("\n")
}

/// Pad/truncate a cell row to exactly `n` columns.
fn pad_cells(cells: &[String], n: usize) -> Vec<String> {
    let mut out: Vec<String> = cells.iter().take(n).cloned().collect();
    while out.len() < n {
        out.push(String::new());
    }
    out
}

/// Render a sequence of inline nodes (text, mention, emoji, media, …) to a
/// Markdown string, applying text marks.
fn render_inline_nodes(nodes: &[Value]) -> String {
    let mut out = String::new();
    for node in nodes {
        render_inline_node(node, &mut out);
    }
    out
}

fn render_inline_node(node: &Value, out: &mut String) {
    let node_type = node.get("type").and_then(Value::as_str).unwrap_or("");
    match node_type {
        "text" => {
            let text = node.get("text").and_then(Value::as_str).unwrap_or("");
            out.push_str(&apply_marks(text, node));
        }
        "hardBreak" => out.push('\n'),
        "mention" => {
            let name = node
                .get("attrs")
                .and_then(|a| a.get("text"))
                .and_then(Value::as_str)
                .unwrap_or("");
            // ADF mention text usually already carries a leading "@".
            if name.starts_with('@') {
                out.push_str(name);
            } else {
                out.push('@');
                out.push_str(name);
            }
        }
        "emoji" => {
            let attrs = node.get("attrs");
            let text = attrs
                .and_then(|a| a.get("text"))
                .and_then(Value::as_str)
                .or_else(|| {
                    attrs
                        .and_then(|a| a.get("shortName"))
                        .and_then(Value::as_str)
                })
                .unwrap_or("");
            out.push_str(text);
        }
        "date" => {
            let ts = node
                .get("attrs")
                .and_then(|a| a.get("timestamp"))
                .and_then(Value::as_str)
                .unwrap_or("");
            out.push_str(ts);
        }
        "status" => {
            let text = node
                .get("attrs")
                .and_then(|a| a.get("text"))
                .and_then(Value::as_str)
                .unwrap_or("");
            out.push_str(text);
        }
        "inlineCard" | "blockCard" => {
            let url = node
                .get("attrs")
                .and_then(|a| a.get("url"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if !url.is_empty() {
                out.push_str(&format!("[{url}]({url})"));
            }
        }
        "media" => {
            let attrs = node.get("attrs");
            let id = attrs
                .and_then(|a| a.get("id"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let alt = attrs
                .and_then(|a| a.get("alt"))
                .and_then(Value::as_str)
                .unwrap_or("");
            // Sentinel URL the Jira converter resolves against attachments.
            out.push_str(&format!("![{alt}](jira-media:{id})"));
        }
        _ => {
            // Unknown inline node: recurse into children (e.g. mediaInline) and
            // fall back to any text it carries.
            if let Some(children) = node.get("content").and_then(Value::as_array) {
                out.push_str(&render_inline_nodes(children));
            } else if let Some(text) = node.get("text").and_then(Value::as_str) {
                out.push_str(text);
            }
        }
    }
}

/// Apply a text node's `marks` to its text, innermost-first, producing Markdown
/// (`**bold**`, `*em*`, `~~strike~~`, `` `code` ``, `[text](href)`).
fn apply_marks(text: &str, node: &Value) -> String {
    let Some(marks) = node.get("marks").and_then(Value::as_array) else {
        return text.to_string();
    };
    let mut wrapped = text.to_string();
    // `code` should be the tightest wrapper (no nested markdown inside code),
    // links the outermost; apply in a fixed order for determinism.
    let mark_names: Vec<&str> = marks
        .iter()
        .filter_map(|m| m.get("type").and_then(Value::as_str))
        .collect();
    let has = |name: &str| mark_names.contains(&name);

    if has("code") {
        wrapped = format!("`{wrapped}`");
    }
    if has("strong") {
        wrapped = format!("**{wrapped}**");
    }
    if has("em") {
        wrapped = format!("*{wrapped}*");
    }
    if has("strike") {
        wrapped = format!("~~{wrapped}~~");
    }
    if has("link") {
        let href = marks
            .iter()
            .find(|m| m.get("type").and_then(Value::as_str) == Some("link"))
            .and_then(|m| m.get("attrs"))
            .and_then(|a| a.get("href"))
            .and_then(Value::as_str)
            .unwrap_or("");
        wrapped = format!("[{wrapped}]({href})");
    }
    wrapped
}

/// The `content` array of a node as a slice (empty when absent).
fn child_nodes(node: &Value) -> &[Value] {
    node.get("content")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// Concatenate all descendant `text` values (for code blocks, which carry no
/// marks and may have multiple text fragments).
fn collect_plain_text(node: &Value) -> String {
    let mut out = String::new();
    collect_plain_text_into(node, &mut out);
    out
}

fn collect_plain_text_into(node: &Value, out: &mut String) {
    if let Some(text) = node.get("text").and_then(Value::as_str) {
        out.push_str(text);
    }
    for child in child_nodes(node) {
        collect_plain_text_into(child, out);
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text(s: &str) -> Value {
        json!({"type": "text", "text": s})
    }

    fn doc(content: Value) -> Value {
        json!({"type": "doc", "version": 1, "content": content})
    }

    #[test]
    fn test_heading() {
        let adf = doc(json!([
            {"type": "heading", "attrs": {"level": 2}, "content": [text("Title")]}
        ]));
        assert_eq!(adf_to_markdown(&adf), "## Title");
    }

    #[test]
    fn test_paragraph_plain() {
        let adf = doc(json!([
            {"type": "paragraph", "content": [text("Hello world")]}
        ]));
        assert_eq!(adf_to_markdown(&adf), "Hello world");
    }

    #[test]
    fn test_marks_strong_em_strike_code() {
        let adf = doc(json!([{"type": "paragraph", "content": [
            {"type": "text", "text": "b", "marks": [{"type": "strong"}]},
            text(" "),
            {"type": "text", "text": "i", "marks": [{"type": "em"}]},
            text(" "),
            {"type": "text", "text": "s", "marks": [{"type": "strike"}]},
            text(" "),
            {"type": "text", "text": "c", "marks": [{"type": "code"}]},
        ]}]));
        assert_eq!(adf_to_markdown(&adf), "**b** *i* ~~s~~ `c`");
    }

    #[test]
    fn test_link_mark() {
        let adf = doc(json!([{"type": "paragraph", "content": [
            text("See "),
            {"type": "text", "text": "wodo.dev", "marks": [
                {"type": "link", "attrs": {"href": "https://wodo.dev"}}
            ]},
            text(" now"),
        ]}]));
        assert_eq!(
            adf_to_markdown(&adf),
            "See [wodo.dev](https://wodo.dev) now"
        );
    }

    #[test]
    fn test_mention() {
        let adf = doc(json!([{"type": "paragraph", "content": [
            text("Owner: "),
            {"type": "mention", "attrs": {"id": "acc-1", "text": "@Timo"}},
        ]}]));
        assert_eq!(adf_to_markdown(&adf), "Owner: @Timo");
    }

    #[test]
    fn test_mention_without_leading_at() {
        let adf = doc(json!([{"type": "paragraph", "content": [
            {"type": "mention", "attrs": {"id": "acc-1", "text": "Timo"}},
        ]}]));
        assert_eq!(adf_to_markdown(&adf), "@Timo");
    }

    #[test]
    fn test_bullet_list() {
        let adf = doc(json!([{"type": "bulletList", "content": [
            {"type": "listItem", "content": [{"type": "paragraph", "content": [text("one")]}]},
            {"type": "listItem", "content": [{"type": "paragraph", "content": [text("two")]}]},
        ]}]));
        assert_eq!(adf_to_markdown(&adf), "- one\n- two");
    }

    #[test]
    fn test_ordered_list() {
        let adf = doc(json!([{"type": "orderedList", "content": [
            {"type": "listItem", "content": [{"type": "paragraph", "content": [text("a")]}]},
            {"type": "listItem", "content": [{"type": "paragraph", "content": [text("b")]}]},
        ]}]));
        assert_eq!(adf_to_markdown(&adf), "1. a\n2. b");
    }

    #[test]
    fn test_code_block() {
        let adf = doc(json!([{"type": "codeBlock", "attrs": {"language": "rust"},
            "content": [text("fn main() {}")]}]));
        assert_eq!(adf_to_markdown(&adf), "```rust\nfn main() {}\n```");
    }

    #[test]
    fn test_code_block_no_language() {
        let adf = doc(json!([{"type": "codeBlock", "content": [text("plain")]}]));
        assert_eq!(adf_to_markdown(&adf), "```\nplain\n```");
    }

    #[test]
    fn test_panel_to_blockquote() {
        let adf = doc(json!([{"type": "panel", "attrs": {"panelType": "info"},
            "content": [{"type": "paragraph", "content": [text("Heads up")]}]}]));
        assert_eq!(adf_to_markdown(&adf), "> Heads up");
    }

    #[test]
    fn test_blockquote() {
        let adf = doc(json!([{"type": "blockquote",
            "content": [{"type": "paragraph", "content": [text("quoted")]}]}]));
        assert_eq!(adf_to_markdown(&adf), "> quoted");
    }

    #[test]
    fn test_table_to_gfm() {
        let adf = doc(json!([{"type": "table", "content": [
            {"type": "tableRow", "content": [
                {"type": "tableHeader", "content": [{"type": "paragraph", "content": [text("Stage")]}]},
                {"type": "tableHeader", "content": [{"type": "paragraph", "content": [text("Duration")]}]},
            ]},
            {"type": "tableRow", "content": [
                {"type": "tableCell", "content": [{"type": "paragraph", "content": [text("Build")]}]},
                {"type": "tableCell", "content": [{"type": "paragraph", "content": [text("3m")]}]},
            ]},
        ]}]));
        let md = adf_to_markdown(&adf);
        assert_eq!(md, "| Stage | Duration |\n| --- | --- |\n| Build | 3m |");
    }

    #[test]
    fn test_rule() {
        let adf = doc(json!([
            {"type": "paragraph", "content": [text("a")]},
            {"type": "rule"},
            {"type": "paragraph", "content": [text("b")]},
        ]));
        assert_eq!(adf_to_markdown(&adf), "a\n\n---\n\nb");
    }

    #[test]
    fn test_hard_break() {
        let adf = doc(json!([{"type": "paragraph", "content": [
            text("line1"),
            {"type": "hardBreak"},
            text("line2"),
        ]}]));
        assert_eq!(adf_to_markdown(&adf), "line1\nline2");
    }

    #[test]
    fn test_inline_card() {
        let adf = doc(json!([{"type": "paragraph", "content": [
            {"type": "inlineCard", "attrs": {"url": "https://example.com"}},
        ]}]));
        assert_eq!(
            adf_to_markdown(&adf),
            "[https://example.com](https://example.com)"
        );
    }

    #[test]
    fn test_block_card() {
        let adf = doc(json!([{"type": "blockCard", "attrs": {"url": "https://x.test"}}]));
        assert_eq!(adf_to_markdown(&adf), "[https://x.test](https://x.test)");
    }

    #[test]
    fn test_emoji_and_status_and_date() {
        let adf = doc(json!([{"type": "paragraph", "content": [
            {"type": "emoji", "attrs": {"shortName": ":smile:", "text": "🙂"}},
            text(" "),
            {"type": "status", "attrs": {"text": "DONE"}},
            text(" "),
            {"type": "date", "attrs": {"timestamp": "2026-06-20"}},
        ]}]));
        assert_eq!(adf_to_markdown(&adf), "🙂 DONE 2026-06-20");
    }

    #[test]
    fn test_media_sentinel() {
        let adf = doc(json!([{"type": "mediaSingle", "content": [
            {"type": "media", "attrs": {"id": "media-123", "type": "file",
                "collection": "c", "alt": "diagram"}},
        ]}]));
        assert_eq!(adf_to_markdown(&adf), "![diagram](jira-media:media-123)");
    }

    #[test]
    fn test_media_no_alt() {
        let adf = doc(json!([{"type": "mediaGroup", "content": [
            {"type": "media", "attrs": {"id": "m-1", "type": "file"}},
        ]}]));
        assert_eq!(adf_to_markdown(&adf), "![](jira-media:m-1)");
    }

    #[test]
    fn test_unknown_node_recurses() {
        // An unknown wrapper still yields its text.
        let adf = doc(json!([
            {"type": "expand", "attrs": {"title": "x"}, "content": [
                {"type": "paragraph", "content": [text("inside expand")]}
            ]}
        ]));
        assert_eq!(adf_to_markdown(&adf), "inside expand");
    }

    #[test]
    fn test_empty_doc() {
        let adf = doc(json!([]));
        assert_eq!(adf_to_markdown(&adf), "");
    }

    #[test]
    fn test_multiple_blocks_blank_separated() {
        let adf = doc(json!([
            {"type": "heading", "attrs": {"level": 1}, "content": [text("H")]},
            {"type": "paragraph", "content": [text("p1")]},
            {"type": "paragraph", "content": [text("p2")]},
        ]));
        assert_eq!(adf_to_markdown(&adf), "# H\n\np1\n\np2");
    }
}
