//! Live Markdown → Yjs (TipTap) conversion.
//!
//! Converts Markdown source text into a TipTap-compatible `XmlFragment`, using
//! `XmlText.format()` for inline marks (bold, italic, strike, code) the way
//! y-prosemirror stores them. This is the production path used when writing item
//! descriptions, comments, and document content from Markdown (e.g. via the MCP
//! tools and import pipeline).
//!
//! The entry point is [`write_markdown_to_fragment`]. [`write_description`] is a
//! thin wrapper that clears and repopulates an item's `description` fragment in
//! place (preserving TipTap's binding to it). [`build_short_id_lookup`] builds
//! the `short_id → UUID` map used to resolve `[[WDO-42]]` item references via an
//! [`ItemResolver`].

/// Write description as TipTap XmlFragment, parsing markdown syntax.
///
/// Supports:
/// - `# Heading` (levels 1-6)
/// - Paragraphs (blank-line separated)
/// - `- bullet` or `* bullet` lists
/// - `- [ ]` / `- [x]` task lists
/// - `1. numbered` lists
/// - `> blockquote`
/// - ``` `​``language ... `​`` ``` code blocks
/// - `[[WDO-42]]` item references (requires resolver)
///
/// IMPORTANT: This function clears and repopulates the existing XmlFragment
/// rather than replacing it. This preserves TipTap's binding to the fragment,
/// allowing real-time sync to work correctly.
pub fn write_description(
    txn: &mut yrs::TransactionMut,
    item_map: &yrs::MapRef,
    content: &str,
    resolver: Option<ItemResolver<'_>>,
) -> Result<(), String> {
    use yrs::{Map, Out, XmlFragment, XmlFragmentPrelim};

    // Get or create the fragment, preserving TipTap's binding to it.
    let frag = match item_map.get(txn, "description") {
        Some(Out::YXmlFragment(existing)) => existing,
        _ => item_map.insert(txn, "description", XmlFragmentPrelim::default()),
    };

    // Clear existing content before writing.
    let len = frag.len(txn);
    if len > 0 {
        frag.remove_range(txn, 0, len);
    }

    // Delegate to write_markdown_to_fragment which uses XmlText.format() for marks
    // (bold, italic, etc.) rather than wrapper XML elements.
    write_markdown_to_fragment(txn, &frag, content, resolver);

    Ok(())
}

// =============================================================================
// Inline Formatting Parser
// =============================================================================

/// Represents a segment of text with optional formatting
#[derive(Debug, Clone)]
enum TextSegment<'a> {
    Plain(&'a str),
    Bold(&'a str),
    Italic(&'a str),
    Strike(&'a str),
    Code(&'a str),
    /// Hyperlink `[text](href)` — rendered as a `link` mark over `text`.
    Link {
        text: &'a str,
        href: &'a str,
    },
    /// Image `![alt](src)` — rendered as an inline `image` element node.
    Image {
        alt: &'a str,
        src: &'a str,
    },
    /// Item reference by short_id (e.g., [[WDO-42]] or [[42]])
    ItemRef(i64),
}

/// Mark type for formatting ranges
#[derive(Debug, Clone)]
enum MarkType {
    Bold,
    Italic,
    Strike,
    Code,
    /// Hyperlink mark carrying its `href` attribute.
    Link(String),
}

/// A range of text with a specific mark applied
#[derive(Debug, Clone)]
struct FormattingRange {
    start: u32,
    length: u32,
    mark_type: MarkType,
}

/// An inline image embed to insert (position, src, optional alt).
/// Handled separately from text since it's an actual element, not a mark.
#[derive(Debug, Clone)]
struct ImageEmbed {
    src: String,
    alt: Option<String>,
}

/// Text content with formatting ranges (for use with XmlText.format())
#[derive(Debug, Clone)]
struct FormattedTextContent {
    /// The full text content (without any markup)
    text: String,
    /// Formatting ranges to apply
    ranges: Vec<FormattingRange>,
    /// Item embeds to insert (position, short_id)
    /// These are handled separately since they're actual elements, not marks
    item_embeds: Vec<(u32, i64)>,
    /// Image embeds to insert (position, src/alt). Like item embeds, these are
    /// actual element nodes rather than marks.
    image_embeds: Vec<(u32, ImageEmbed)>,
}

/// Convert TextSegments to FormattedTextContent
/// This extracts the plain text and tracks formatting ranges for later application.
fn segments_to_formatted_content(segments: &[TextSegment<'_>]) -> FormattedTextContent {
    let mut text = String::new();
    let mut ranges = Vec::new();
    let mut item_embeds = Vec::new();
    let mut image_embeds = Vec::new();

    for segment in segments {
        let start = text.len() as u32;
        match segment {
            TextSegment::Plain(s) => {
                text.push_str(s);
            }
            TextSegment::Bold(s) => {
                text.push_str(s);
                ranges.push(FormattingRange {
                    start,
                    length: s.len() as u32,
                    mark_type: MarkType::Bold,
                });
            }
            TextSegment::Italic(s) => {
                text.push_str(s);
                ranges.push(FormattingRange {
                    start,
                    length: s.len() as u32,
                    mark_type: MarkType::Italic,
                });
            }
            TextSegment::Strike(s) => {
                text.push_str(s);
                ranges.push(FormattingRange {
                    start,
                    length: s.len() as u32,
                    mark_type: MarkType::Strike,
                });
            }
            TextSegment::Code(s) => {
                text.push_str(s);
                ranges.push(FormattingRange {
                    start,
                    length: s.len() as u32,
                    mark_type: MarkType::Code,
                });
            }
            TextSegment::Link {
                text: link_text,
                href,
            } => {
                text.push_str(link_text);
                ranges.push(FormattingRange {
                    start,
                    length: link_text.len() as u32,
                    mark_type: MarkType::Link(href.to_string()),
                });
            }
            TextSegment::Image { alt, src } => {
                // Image embeds are handled separately - they're actual elements.
                image_embeds.push((
                    start,
                    ImageEmbed {
                        src: src.to_string(),
                        alt: if alt.is_empty() {
                            None
                        } else {
                            Some(alt.to_string())
                        },
                    },
                ));
                // Insert a placeholder character that we'll replace with the embed
                text.push('\u{FFFC}'); // Object replacement character
            }
            TextSegment::ItemRef(short_id) => {
                // Item embeds are handled separately - they need to be actual elements
                item_embeds.push((start, *short_id));
                // Insert a placeholder character that we'll replace with the embed
                text.push('\u{FFFC}'); // Object replacement character
            }
        }
    }

    FormattedTextContent {
        text,
        ranges,
        item_embeds,
        image_embeds,
    }
}

/// Insert formatted text into a parent XmlElement and apply formatting marks.
/// This uses XmlText.format() which is how y-prosemirror stores marks.
fn insert_formatted_text_into_element(
    txn: &mut yrs::TransactionMut,
    parent: &yrs::XmlElementRef,
    content: &FormattedTextContent,
    resolver: Option<ItemResolver<'_>>,
) {
    use yrs::{XmlFragment, XmlTextPrelim};

    let has_embeds = !content.item_embeds.is_empty() || !content.image_embeds.is_empty();

    // Placeholder chars (\u{FFFC}) stand in for embeds; with embeds present we
    // strip them from the text node and append the embed elements afterwards.
    let text = if has_embeds {
        content.text.replace('\u{FFFC}', "")
    } else {
        content.text.clone()
    };

    let text_ref: yrs::XmlTextRef = parent.insert(txn, 0, XmlTextPrelim::new(&text));

    // Apply formatting marks, adjusting positions for any stripped placeholders.
    let placeholders_before = |start: u32| -> u32 {
        if !has_embeds {
            return 0;
        }
        let items = content.item_embeds.iter().filter(|(pos, _)| *pos < start);
        let images = content.image_embeds.iter().filter(|(pos, _)| *pos < start);
        (items.count() + images.count()) as u32
    };
    for range in &content.ranges {
        let adjusted_start = range.start - placeholders_before(range.start);
        apply_mark(
            txn,
            &text_ref,
            adjusted_start,
            range.length,
            &range.mark_type,
        );
    }

    // Append embeds after the text (simplified: not interleaved at exact offset).
    for (_pos, short_id) in &content.item_embeds {
        if let Some(uuid) = resolver.and_then(|r| r(*short_id)) {
            let mut elem =
                yrs::XmlElementPrelim::new("itemEmbed", Vec::<yrs::types::xml::XmlIn>::new());
            elem.attributes.insert("itemId".into(), uuid.to_string());
            parent.insert(txn, parent.len(txn), elem);
        }
    }
    for (_pos, image) in &content.image_embeds {
        let mut elem = yrs::XmlElementPrelim::new("image", Vec::<yrs::types::xml::XmlIn>::new());
        elem.attributes.insert("src".into(), image.src.clone());
        if let Some(alt) = &image.alt {
            elem.attributes.insert("alt".into(), alt.clone());
        }
        parent.insert(txn, parent.len(txn), elem);
    }
}

/// Apply a single inline mark over a text range using `XmlText.format()` (the
/// y-prosemirror storage form). Marks with attributes (e.g. `link`) store their
/// value as an `Any::Map` of those attributes; attribute-less marks store
/// `Any::Bool(true)`.
fn apply_mark(
    txn: &mut yrs::TransactionMut,
    text_ref: &yrs::XmlTextRef,
    start: u32,
    length: u32,
    mark_type: &MarkType,
) {
    use std::collections::HashMap;
    use std::sync::Arc;
    use yrs::{Any, Text};

    let mut attrs = HashMap::new();
    match mark_type {
        MarkType::Bold => {
            attrs.insert("bold".into(), Any::Bool(true));
        }
        MarkType::Italic => {
            attrs.insert("italic".into(), Any::Bool(true));
        }
        MarkType::Strike => {
            attrs.insert("strike".into(), Any::Bool(true));
        }
        MarkType::Code => {
            attrs.insert("code".into(), Any::Bool(true));
        }
        MarkType::Link(href) => {
            // y-prosemirror stores a mark's attributes as a nested map keyed by
            // the mark name, so the editor's `link` mark gets its `href` attr.
            let mut link_attrs: HashMap<String, Any> = HashMap::new();
            link_attrs.insert("href".to_string(), Any::String(href.clone().into()));
            attrs.insert("link".into(), Any::Map(Arc::new(link_attrs)));
        }
    }
    text_ref.format(txn, start, length, attrs);
}

/// Write markdown content to an XmlFragment using proper y-prosemirror formatting.
/// This uses XmlText.format() for marks instead of wrapper elements.
///
/// Supports:
/// - `# Heading` (levels 1-6)
/// - Paragraphs (blank-line separated)
/// - `- bullet` or `* bullet` lists
/// - `- [ ]` / `- [x]` task lists
/// - `1. numbered` lists
/// - `> blockquote`
/// - ``` `​``language ... `​`` ``` code blocks
/// - GFM tables: `| header | header |` with `| --- | --- |` separator
/// - Inline formatting: **bold**, *italic*, ~~strike~~, `code`
/// - `[[WDO-42]]` item references (requires resolver)
pub fn write_markdown_to_fragment(
    txn: &mut yrs::TransactionMut,
    frag: &yrs::XmlFragmentRef,
    content: &str,
    resolver: Option<ItemResolver<'_>>,
) {
    let mut lines = content.lines().peekable();
    let mut element_index = 0u32;

    while let Some(line) = lines.next() {
        // Skip empty lines between blocks
        if line.trim().is_empty() {
            continue;
        }

        // 1. Try to parse as heading: # ## ### etc.
        if let Some((level, text)) = try_parse_heading_line(line) {
            write_heading_element(txn, frag, element_index, level, text, resolver);
            element_index += 1;
            continue;
        }

        // 2. Code block: ```language
        if line.trim_start().starts_with("```") {
            write_code_block_element(txn, frag, element_index, line, &mut lines);
            element_index += 1;
            continue;
        }

        // 3. Task list: - [ ] or - [x] (must check BEFORE bullet list)
        if is_task_list_start(line) {
            write_task_list_element(txn, frag, element_index, line, &mut lines, resolver);
            element_index += 1;
            continue;
        }

        // 4. Bullet list: - or *
        if line.trim_start().starts_with("- ") || line.trim_start().starts_with("* ") {
            write_bullet_list_element(txn, frag, element_index, line, &mut lines, resolver);
            element_index += 1;
            continue;
        }

        // 5. Ordered list: 1. 2. etc.
        if is_ordered_list_start(line) {
            write_ordered_list_element(txn, frag, element_index, line, &mut lines, resolver);
            element_index += 1;
            continue;
        }

        // 6. Blockquote: >
        if line.trim_start().starts_with("> ") || line.trim() == ">" {
            write_blockquote_element(txn, frag, element_index, line, &mut lines, resolver);
            element_index += 1;
            continue;
        }

        // 7. Table: line with pipe(s), next line is a valid separator
        if line.contains('|') {
            if let Some(next_line) = lines.peek() {
                if let Some(sep_cols) = scan_table_separator(next_line) {
                    let header_cells = split_table_row(line);
                    if header_cells.len() == sep_cols {
                        lines.next(); // consume separator
                        write_table_element(
                            txn,
                            frag,
                            element_index,
                            &header_cells,
                            sep_cols,
                            &mut lines,
                            resolver,
                        );
                        element_index += 1;
                        continue;
                    }
                }
            }
        }

        // 7b. Standalone image line `![alt](src)` → a BLOCK-level `image`
        // element (fragment sibling). The editor's image node is block-level
        // (`inline: false`); nested in a paragraph it is schema-invalid and
        // ProseMirror silently drops it on load.
        if let Some((alt, src)) = try_parse_standalone_image(line) {
            write_image_element(txn, frag, element_index, src, alt);
            element_index += 1;
            continue;
        }

        // 8. Default: paragraph
        write_paragraph_element(txn, frag, element_index, line, resolver);
        element_index += 1;
    }
}

/// Try to parse a line as a heading.
/// Returns (level, text) if it's a valid heading, None otherwise.
/// A line that is exactly one markdown image `![alt](src)` (nothing else), for
/// block-level emission. Returns `(alt, src)`.
fn try_parse_standalone_image(line: &str) -> Option<(&str, &str)> {
    let t = line.trim();
    let rest = t.strip_prefix("![")?;
    let close = rest.find("](")?;
    let alt = &rest[..close];
    let src = rest[close + 2..].strip_suffix(')')?;
    // Reject mixed/nested content so only a lone image matches.
    if alt.contains(['[', ']']) || src.is_empty() || src.contains([')', '(']) {
        return None;
    }
    Some((alt, src))
}

/// Insert a block-level `image` element (a direct fragment child) — matches the
/// editor's block-level image node, unlike the inline image embeds.
fn write_image_element(
    txn: &mut yrs::TransactionMut,
    frag: &yrs::XmlFragmentRef,
    index: u32,
    src: &str,
    alt: &str,
) {
    use yrs::XmlFragment;
    let mut elem = yrs::XmlElementPrelim::new("image", Vec::<yrs::types::xml::XmlIn>::new());
    elem.attributes.insert("src".into(), src.to_string());
    if !alt.is_empty() {
        elem.attributes.insert("alt".into(), alt.to_string());
    }
    frag.insert(txn, index, elem);
}

fn try_parse_heading_line(line: &str) -> Option<(u8, &str)> {
    let trimmed = line.trim();
    if !trimmed.starts_with('#') {
        return None;
    }

    let level = trimmed.chars().take_while(|c| *c == '#').count();
    if level == 0 || level > 6 {
        return None;
    }

    // Must have space after #s (e.g., "# Heading" not "#Heading")
    let rest = &trimmed[level..];
    if !rest.starts_with(' ') && !rest.is_empty() {
        return None;
    }

    let text = rest.trim_start();
    Some((level as u8, text))
}

/// Write a heading element with proper y-prosemirror formatting.
fn write_heading_element(
    txn: &mut yrs::TransactionMut,
    frag: &yrs::XmlFragmentRef,
    index: u32,
    level: u8,
    text: &str,
    resolver: Option<ItemResolver<'_>>,
) {
    use yrs::{XmlElementPrelim, XmlFragment};

    let mut heading = XmlElementPrelim::new("heading", Vec::<yrs::types::xml::XmlIn>::new());
    heading.attributes.insert("level".into(), level.to_string());
    let heading_ref: yrs::XmlElementRef = frag.insert(txn, index, heading);

    // Parse inline formatting and insert with marks
    let segments = parse_inline_formatting(text);
    let formatted = segments_to_formatted_content(&segments);
    insert_formatted_text_into_element(txn, &heading_ref, &formatted, resolver);
}

/// Write a code block element (no inline formatting in code blocks).
fn write_code_block_element<'a>(
    txn: &mut yrs::TransactionMut,
    frag: &yrs::XmlFragmentRef,
    index: u32,
    first_line: &str,
    lines: &mut std::iter::Peekable<std::str::Lines<'a>>,
) {
    use yrs::{Text, XmlElementPrelim, XmlFragment, XmlTextPrelim};

    // Extract language from ```lang
    let lang = first_line.trim().strip_prefix("```").unwrap_or("").trim();

    // Collect lines until closing ```
    let mut code_lines = Vec::new();
    for line in lines.by_ref() {
        if line.trim() == "```" {
            break;
        }
        code_lines.push(line);
    }
    let code = code_lines.join("\n");

    // Create code block element
    let mut block = XmlElementPrelim::new("codeBlock", Vec::<yrs::types::xml::XmlIn>::new());
    if !lang.is_empty() {
        block.attributes.insert("language".into(), lang.to_string());
    }
    let block_ref: yrs::XmlElementRef = frag.insert(txn, index, block);

    // Insert plain text (no formatting in code blocks)
    if !code.is_empty() {
        let text_ref: yrs::XmlTextRef = block_ref.insert(txn, 0, XmlTextPrelim::new(""));
        text_ref.insert(txn, 0, &code);
    }
}

/// Write a bullet list element with proper y-prosemirror formatting.
fn write_bullet_list_element<'a>(
    txn: &mut yrs::TransactionMut,
    frag: &yrs::XmlFragmentRef,
    index: u32,
    first_line: &str,
    lines: &mut std::iter::Peekable<std::str::Lines<'a>>,
    resolver: Option<ItemResolver<'_>>,
) {
    use yrs::{XmlElementPrelim, XmlFragment};

    // Collect all bullet items
    let mut items = Vec::new();

    // Extract text from first line
    let first_text = first_line
        .trim()
        .strip_prefix("- ")
        .or_else(|| first_line.trim().strip_prefix("* "))
        .unwrap_or(first_line.trim());
    items.push(first_text.to_string());

    // Collect consecutive bullet items
    while let Some(line) = lines.peek() {
        let trimmed = line.trim();
        // Stop if we hit a task list item (they look similar but are different)
        if is_task_list_start(line) {
            break;
        }
        if let Some(text) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            items.push(text.to_string());
            lines.next();
        } else {
            break;
        }
    }

    // Create bullet list element
    let list = XmlElementPrelim::new("bulletList", Vec::<yrs::types::xml::XmlIn>::new());
    let list_ref: yrs::XmlElementRef = frag.insert(txn, index, list);

    // Add each list item
    for (i, item_text) in items.iter().enumerate() {
        write_list_item_into(txn, &list_ref, i as u32, item_text, resolver);
    }
}

/// Write an ordered list element with proper y-prosemirror formatting.
fn write_ordered_list_element<'a>(
    txn: &mut yrs::TransactionMut,
    frag: &yrs::XmlFragmentRef,
    index: u32,
    first_line: &str,
    lines: &mut std::iter::Peekable<std::str::Lines<'a>>,
    resolver: Option<ItemResolver<'_>>,
) {
    use yrs::{XmlElementPrelim, XmlFragment};

    // Collect all ordered items
    let mut items = Vec::new();

    // Extract text from first line
    items.push(extract_ordered_list_text(first_line).to_string());

    // Collect consecutive numbered items
    while let Some(line) = lines.peek() {
        if is_ordered_list_start(line) {
            items.push(extract_ordered_list_text(line).to_string());
            lines.next();
        } else {
            break;
        }
    }

    // Create ordered list element with start attribute
    let mut list = XmlElementPrelim::new("orderedList", Vec::<yrs::types::xml::XmlIn>::new());
    list.attributes.insert("start".into(), "1".to_string());
    let list_ref: yrs::XmlElementRef = frag.insert(txn, index, list);

    // Add each list item
    for (i, item_text) in items.iter().enumerate() {
        write_list_item_into(txn, &list_ref, i as u32, item_text, resolver);
    }
}

/// Write a task list element with proper y-prosemirror formatting.
fn write_task_list_element<'a>(
    txn: &mut yrs::TransactionMut,
    frag: &yrs::XmlFragmentRef,
    index: u32,
    first_line: &str,
    lines: &mut std::iter::Peekable<std::str::Lines<'a>>,
    resolver: Option<ItemResolver<'_>>,
) {
    use yrs::{XmlElementPrelim, XmlFragment};

    // Collect all task items: (text, checked)
    let mut items: Vec<(String, bool)> = Vec::new();

    // Extract from first line
    let (text, checked) = extract_task_item(first_line);
    items.push((text.to_string(), checked));

    // Collect consecutive task list items
    while let Some(line) = lines.peek() {
        if is_task_list_start(line) {
            let (text, checked) = extract_task_item(line);
            items.push((text.to_string(), checked));
            lines.next();
        } else {
            break;
        }
    }

    // Create task list element
    let list = XmlElementPrelim::new("taskList", Vec::<yrs::types::xml::XmlIn>::new());
    let list_ref: yrs::XmlElementRef = frag.insert(txn, index, list);

    // Add each task item
    for (i, (item_text, checked)) in items.iter().enumerate() {
        write_task_item_into(txn, &list_ref, i as u32, item_text, *checked, resolver);
    }
}

/// Write a blockquote element with proper y-prosemirror formatting.
fn write_blockquote_element<'a>(
    txn: &mut yrs::TransactionMut,
    frag: &yrs::XmlFragmentRef,
    index: u32,
    first_line: &str,
    lines: &mut std::iter::Peekable<std::str::Lines<'a>>,
    resolver: Option<ItemResolver<'_>>,
) {
    use yrs::{XmlElementPrelim, XmlFragment};

    // Collect all blockquote lines
    let mut quote_lines = Vec::new();

    // Extract text from first line
    let first_text = first_line
        .trim()
        .strip_prefix("> ")
        .or_else(|| first_line.trim().strip_prefix(">"))
        .unwrap_or(first_line.trim());
    quote_lines.push(first_text.to_string());

    // Collect consecutive blockquote lines
    while let Some(line) = lines.peek() {
        let trimmed = line.trim();
        if let Some(text) = trimmed.strip_prefix("> ") {
            quote_lines.push(text.to_string());
            lines.next();
        } else if trimmed.starts_with('>') {
            // Handle ">" with no text after
            quote_lines.push(String::new());
            lines.next();
        } else {
            break;
        }
    }

    // Create blockquote element
    let blockquote = XmlElementPrelim::new("blockquote", Vec::<yrs::types::xml::XmlIn>::new());
    let blockquote_ref: yrs::XmlElementRef = frag.insert(txn, index, blockquote);

    // Each line becomes a paragraph inside the blockquote
    for (i, line_text) in quote_lines.iter().enumerate() {
        let para = XmlElementPrelim::new("paragraph", Vec::<yrs::types::xml::XmlIn>::new());
        let para_ref: yrs::XmlElementRef = blockquote_ref.insert(txn, i as u32, para);

        if !line_text.is_empty() {
            let segments = parse_inline_formatting(line_text);
            let formatted = segments_to_formatted_content(&segments);
            insert_formatted_text_into_element(txn, &para_ref, &formatted, resolver);
        }
    }
}

/// Write a table element from parsed header cells and subsequent data rows.
fn write_table_element<'a>(
    txn: &mut yrs::TransactionMut,
    frag: &yrs::XmlFragmentRef,
    index: u32,
    header_cells: &[String],
    num_cols: usize,
    lines: &mut std::iter::Peekable<std::str::Lines<'a>>,
    resolver: Option<ItemResolver<'_>>,
) {
    use yrs::{XmlElementPrelim, XmlFragment};

    let table = XmlElementPrelim::new("table", Vec::<yrs::types::xml::XmlIn>::new());
    let table_ref: yrs::XmlElementRef = frag.insert(txn, index, table);

    let mut row_index = 0u32;

    // Header row
    let header_row = XmlElementPrelim::new("tableRow", Vec::<yrs::types::xml::XmlIn>::new());
    let header_row_ref: yrs::XmlElementRef = table_ref.insert(txn, row_index, header_row);

    for (i, cell_text) in header_cells.iter().take(num_cols).enumerate() {
        write_table_cell(txn, &header_row_ref, i as u32, cell_text, true, resolver);
    }
    for i in header_cells.len()..num_cols {
        write_table_cell(txn, &header_row_ref, i as u32, "", true, resolver);
    }
    row_index += 1;

    // Data rows — consume until blank line or line without pipes
    while let Some(line) = lines.peek() {
        let trimmed = line.trim();
        if trimmed.is_empty() || !trimmed.contains('|') {
            break;
        }

        let cells = split_table_row(line);
        if cells.is_empty() {
            break;
        }
        lines.next();

        let data_row = XmlElementPrelim::new("tableRow", Vec::<yrs::types::xml::XmlIn>::new());
        let data_row_ref: yrs::XmlElementRef = table_ref.insert(txn, row_index, data_row);

        for (i, cell_text) in cells.iter().take(num_cols).enumerate() {
            write_table_cell(txn, &data_row_ref, i as u32, cell_text, false, resolver);
        }
        for i in cells.len()..num_cols {
            write_table_cell(txn, &data_row_ref, i as u32, "", false, resolver);
        }
        row_index += 1;
    }
}

/// Write a single table cell (header or data) into a row element.
fn write_table_cell(
    txn: &mut yrs::TransactionMut,
    row_ref: &yrs::XmlElementRef,
    index: u32,
    text: &str,
    is_header: bool,
    resolver: Option<ItemResolver<'_>>,
) {
    use yrs::{XmlElementPrelim, XmlFragment};

    let tag = if is_header {
        "tableHeader"
    } else {
        "tableCell"
    };
    // Don't set colspan/rowspan — yrs stores them as strings but tiptap expects
    // numbers. Omitting them lets tiptap use its schema defaults (colspan=1, rowspan=1).
    let cell = XmlElementPrelim::new(tag, Vec::<yrs::types::xml::XmlIn>::new());
    let cell_ref: yrs::XmlElementRef = row_ref.insert(txn, index, cell);

    let para = XmlElementPrelim::new("paragraph", Vec::<yrs::types::xml::XmlIn>::new());
    let para_ref: yrs::XmlElementRef = cell_ref.insert(txn, 0, para);

    if !text.is_empty() {
        let segments = parse_inline_formatting(text);
        let formatted = segments_to_formatted_content(&segments);
        insert_formatted_text_into_element(txn, &para_ref, &formatted, resolver);
    }
}

/// Write a paragraph element with proper y-prosemirror formatting.
fn write_paragraph_element(
    txn: &mut yrs::TransactionMut,
    frag: &yrs::XmlFragmentRef,
    index: u32,
    text: &str,
    resolver: Option<ItemResolver<'_>>,
) {
    use yrs::{XmlElementPrelim, XmlFragment};

    let para = XmlElementPrelim::new("paragraph", Vec::<yrs::types::xml::XmlIn>::new());
    let para_ref: yrs::XmlElementRef = frag.insert(txn, index, para);

    let segments = parse_inline_formatting(text);
    let formatted = segments_to_formatted_content(&segments);
    insert_formatted_text_into_element(txn, &para_ref, &formatted, resolver);
}

/// Write a listItem into a list element (bullet or ordered).
fn write_list_item_into(
    txn: &mut yrs::TransactionMut,
    list_ref: &yrs::XmlElementRef,
    index: u32,
    text: &str,
    resolver: Option<ItemResolver<'_>>,
) {
    use yrs::{XmlElementPrelim, XmlFragment};

    // <listItem><paragraph>text with formatting</paragraph></listItem>
    let list_item = XmlElementPrelim::new("listItem", Vec::<yrs::types::xml::XmlIn>::new());
    let list_item_ref: yrs::XmlElementRef = list_ref.insert(txn, index, list_item);

    let para = XmlElementPrelim::new("paragraph", Vec::<yrs::types::xml::XmlIn>::new());
    let para_ref: yrs::XmlElementRef = list_item_ref.insert(txn, 0, para);

    if !text.is_empty() {
        let segments = parse_inline_formatting(text);
        let formatted = segments_to_formatted_content(&segments);
        insert_formatted_text_into_element(txn, &para_ref, &formatted, resolver);
    }
}

/// Write a taskItem into a task list element.
fn write_task_item_into(
    txn: &mut yrs::TransactionMut,
    list_ref: &yrs::XmlElementRef,
    index: u32,
    text: &str,
    checked: bool,
    resolver: Option<ItemResolver<'_>>,
) {
    use yrs::{XmlElementPrelim, XmlFragment};

    // <taskItem checked="true/false"><paragraph>text with formatting</paragraph></taskItem>
    let mut task_item = XmlElementPrelim::new("taskItem", Vec::<yrs::types::xml::XmlIn>::new());
    task_item
        .attributes
        .insert("checked".into(), checked.to_string());
    let task_item_ref: yrs::XmlElementRef = list_ref.insert(txn, index, task_item);

    let para = XmlElementPrelim::new("paragraph", Vec::<yrs::types::xml::XmlIn>::new());
    let para_ref: yrs::XmlElementRef = task_item_ref.insert(txn, 0, para);

    if !text.is_empty() {
        let segments = parse_inline_formatting(text);
        let formatted = segments_to_formatted_content(&segments);
        insert_formatted_text_into_element(txn, &para_ref, &formatted, resolver);
    }
}

/// Parse item reference content from [[...]] syntax.
/// Accepts formats: "WDO-42", "42", "PREFIX-123"
/// Returns the short_id number if valid.
fn parse_item_reference(content: &str) -> Option<i64> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return None;
    }

    // Try parsing as plain number first: [[42]]
    if let Ok(num) = trimmed.parse::<i64>() {
        if num > 0 {
            return Some(num);
        }
    }

    // Try parsing as PREFIX-NUMBER format: [[WDO-42]]
    // The dash must not be at the start (to avoid parsing "-1" as prefix "")
    if let Some(dash_pos) = trimmed.rfind('-') {
        if dash_pos > 0 {
            let number_part = &trimmed[dash_pos + 1..];
            if let Ok(num) = number_part.parse::<i64>() {
                if num > 0 {
                    return Some(num);
                }
            }
        }
    }

    None
}

/// Parse a markdown `[label](url)` starting at the beginning of `s`.
///
/// Returns `(label, url, bytes_consumed)` where `bytes_consumed` covers the
/// whole `[label](url)` span. Basic only: no titles, no nested/escaped brackets
/// in the label (the first `]` ends it), and the url runs to the first `)`.
/// Returns `None` if the shape doesn't match.
fn parse_link_like(s: &str) -> Option<(&str, &str, usize)> {
    let rest = s.strip_prefix('[')?;
    let close = rest.find(']')?;
    let label = &rest[..close];
    // The `]` must be immediately followed by `(`.
    let after_label = &rest[close + 1..];
    let paren_rest = after_label.strip_prefix('(')?;
    let paren_close = paren_rest.find(')')?;
    let url = &paren_rest[..paren_close];
    if url.is_empty() {
        return None;
    }
    // consumed = '[' + label + ']' + '(' + url + ')'
    let consumed = 1 + close + 1 + 1 + paren_close + 1;
    Some((label, url, consumed))
}

/// Parse text for inline markdown formatting: **bold**, *italic*, ~~strike~~,
/// `code`, [text](href) links, ![alt](src) images, [[item-ref]].
/// Returns segments that can be converted to XmlIn elements.
fn parse_inline_formatting(text: &str) -> Vec<TextSegment<'_>> {
    let mut segments = Vec::new();
    let mut remaining = text;

    while !remaining.is_empty() {
        // Try to match inline code first (highest priority, doesn't nest)
        if let Some(rest) = remaining.strip_prefix('`') {
            if let Some(end) = rest.find('`') {
                let code_content = &rest[..end];
                if !code_content.is_empty() {
                    segments.push(TextSegment::Code(code_content));
                    remaining = &rest[end + 1..];
                    continue;
                }
            }
        }

        // Try to match bold (**text**)
        if let Some(rest) = remaining.strip_prefix("**") {
            if let Some(end) = rest.find("**") {
                let bold_content = &rest[..end];
                if !bold_content.is_empty() {
                    segments.push(TextSegment::Bold(bold_content));
                    remaining = &rest[end + 2..];
                    continue;
                }
            }
        }

        // Try to match strikethrough (~~text~~)
        if let Some(rest) = remaining.strip_prefix("~~") {
            if let Some(end) = rest.find("~~") {
                let strike_content = &rest[..end];
                if !strike_content.is_empty() {
                    segments.push(TextSegment::Strike(strike_content));
                    remaining = &rest[end + 2..];
                    continue;
                }
            }
        }

        // Try to match italic (*text*) - must check after ** to avoid conflicts
        if let Some(rest) = remaining.strip_prefix('*') {
            // Make sure it's not ** (already handled above)
            if !rest.starts_with('*') {
                if let Some(end) = rest.find('*') {
                    // Make sure the closing * is not part of **
                    if end > 0 && !rest[end..].starts_with("**") {
                        let italic_content = &rest[..end];
                        if !italic_content.is_empty() {
                            segments.push(TextSegment::Italic(italic_content));
                            remaining = &rest[end + 1..];
                            continue;
                        }
                    }
                }
            }
        }

        // Try to match an image (![alt](src)) BEFORE links and item refs, so
        // the leading `!` isn't mistaken for plain text + a link.
        if remaining.starts_with("![") {
            if let Some((alt, src, consumed)) = parse_link_like(&remaining[1..]) {
                segments.push(TextSegment::Image { alt, src });
                remaining = &remaining[1 + consumed..];
                continue;
            }
        }

        // Try to match item reference ([[WDO-42]] or [[42]]) BEFORE plain links,
        // so `[[...]]` isn't mis-parsed as a `[text](url)` link.
        if let Some(rest) = remaining.strip_prefix("[[") {
            if let Some(end) = rest.find("]]") {
                let ref_content = &rest[..end];
                if let Some(short_id) = parse_item_reference(ref_content) {
                    segments.push(TextSegment::ItemRef(short_id));
                    remaining = &rest[end + 2..];
                    continue;
                }
            }
        }

        // Try to match a link ([text](href)). Must come after image + item ref.
        if remaining.starts_with('[') {
            if let Some((text, href, consumed)) = parse_link_like(remaining) {
                if !text.is_empty() {
                    segments.push(TextSegment::Link { text, href });
                    remaining = &remaining[consumed..];
                    continue;
                }
            }
        }

        // No marker found at current position, consume one character as plain text
        // Find the next potential marker or end of string
        let next_marker = remaining
            .find(['*', '~', '`', '[', '!'])
            .unwrap_or(remaining.len());

        if next_marker > 0 {
            segments.push(TextSegment::Plain(&remaining[..next_marker]));
            remaining = &remaining[next_marker..];
        } else {
            // Edge case: marker character that didn't match a pattern
            segments.push(TextSegment::Plain(&remaining[..1]));
            remaining = &remaining[1..];
        }
    }

    segments
}

/// Type alias for item reference resolver function.
/// Takes a short_id and returns the item's UUID if found.
pub type ItemResolver<'a> = &'a dyn Fn(i64) -> Option<uuid::Uuid>;

/// Build a short_id to UUID lookup map from a Y.js items map.
/// This extracts short_id and id from each item in the map.
pub fn build_short_id_lookup<T: yrs::ReadTxn>(
    txn: &T,
    items_map: &yrs::MapRef,
) -> std::collections::HashMap<i64, uuid::Uuid> {
    use yrs::{Any, Map, Out};

    let mut lookup = std::collections::HashMap::new();

    for (_key, value) in items_map.iter(txn) {
        if let Out::YMap(item_map) = value {
            // Get short_id
            let short_id = match item_map.get(txn, "short_id") {
                Some(Out::Any(Any::BigInt(id))) => Some(id),
                Some(Out::Any(Any::Number(n))) => Some(n as i64),
                _ => None,
            };

            // Get id (UUID string)
            let uuid = match item_map.get(txn, "id") {
                Some(Out::Any(Any::String(s))) => uuid::Uuid::parse_str(s.as_ref()).ok(),
                _ => None,
            };

            if let (Some(sid), Some(uid)) = (short_id, uuid) {
                lookup.insert(sid, uid);
            }
        }
    }

    lookup
}

fn is_ordered_list_start(line: &str) -> bool {
    let trimmed = line.trim();
    // Match patterns like "1. ", "2. ", "10. " etc.
    if let Some(dot_pos) = trimmed.find(". ") {
        let prefix = &trimmed[..dot_pos];
        return prefix.chars().all(|c| c.is_ascii_digit()) && !prefix.is_empty();
    }
    false
}

fn extract_ordered_list_text(line: &str) -> &str {
    let trimmed = line.trim();
    if let Some(dot_pos) = trimmed.find(". ") {
        &trimmed[dot_pos + 2..]
    } else {
        trimmed
    }
}

/// Check if a line is a valid GFM table separator row.
/// Valid separators contain only `|`, `-`, `:`, and space.
/// Each column must have at least one `-`, and at least one `|` is required.
/// Returns the number of columns if valid, None otherwise.
fn scan_table_separator(line: &str) -> Option<usize> {
    let trimmed = line.trim();

    if !trimmed.contains('|') || !trimmed.contains('-') {
        return None;
    }

    // Only allowed characters
    if trimmed.chars().any(|c| !matches!(c, '|' | '-' | ':' | ' ')) {
        return None;
    }

    let cells = split_table_row(trimmed);
    if cells.is_empty() {
        return None;
    }

    // Each column must have at least one dash
    for cell in &cells {
        if !cell.contains('-') {
            return None;
        }
    }

    Some(cells.len())
}

/// Split a table row by unescaped `|`, handling leading/trailing border pipes.
/// `\|` inside a cell produces a literal `|`.
fn split_table_row(line: &str) -> Vec<String> {
    let mut s = line.trim();

    // Strip leading border pipe
    if let Some(rest) = s.strip_prefix('|') {
        s = rest;
    }
    // Strip trailing border pipe (only if not escaped)
    if s.ends_with('|') && !s.ends_with("\\|") {
        s = &s[..s.len() - 1];
        s = s.trim_end();
    }

    // Split remaining by unescaped pipes
    let mut cells = Vec::new();
    let mut current = String::new();
    let mut chars = s.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\\' && chars.peek() == Some(&'|') {
            current.push('|');
            chars.next();
        } else if ch == '|' {
            cells.push(current.trim().to_string());
            current = String::new();
        } else {
            current.push(ch);
        }
    }
    cells.push(current.trim().to_string());

    cells
}

/// Check if line is a task list item: `- [ ]` (unchecked) or `- [x]`/`- [X]` (checked)
fn is_task_list_start(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with("- [ ] ")
        || trimmed.starts_with("- [x] ")
        || trimmed.starts_with("- [X] ")
        || trimmed == "- [ ]"
        || trimmed == "- [x]"
        || trimmed == "- [X]"
}

/// Extract text and checked state from a task list item line
fn extract_task_item(line: &str) -> (&str, bool) {
    let trimmed = line.trim();
    if let Some(text) = trimmed.strip_prefix("- [ ] ") {
        (text, false)
    } else if let Some(text) = trimmed.strip_prefix("- [x] ") {
        (text, true)
    } else if let Some(text) = trimmed.strip_prefix("- [X] ") {
        (text, true)
    } else if trimmed == "- [ ]" {
        ("", false)
    } else if trimmed == "- [x]" || trimmed == "- [X]" {
        ("", true)
    } else {
        (trimmed, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // Inline Formatting Parser Tests
    // =========================================================================

    #[test]
    fn test_parse_inline_formatting_plain_text() {
        let segments = parse_inline_formatting("Hello world");
        assert_eq!(segments.len(), 1);
        assert!(matches!(segments[0], TextSegment::Plain("Hello world")));
    }

    #[test]
    fn test_parse_inline_formatting_bold() {
        let segments = parse_inline_formatting("Hello **bold** world");
        assert_eq!(segments.len(), 3);
        assert!(matches!(segments[0], TextSegment::Plain("Hello ")));
        assert!(matches!(segments[1], TextSegment::Bold("bold")));
        assert!(matches!(segments[2], TextSegment::Plain(" world")));
    }

    #[test]
    fn test_parse_inline_formatting_italic() {
        let segments = parse_inline_formatting("Hello *italic* world");
        assert_eq!(segments.len(), 3);
        assert!(matches!(segments[0], TextSegment::Plain("Hello ")));
        assert!(matches!(segments[1], TextSegment::Italic("italic")));
        assert!(matches!(segments[2], TextSegment::Plain(" world")));
    }

    #[test]
    fn test_parse_inline_formatting_strikethrough() {
        let segments = parse_inline_formatting("Hello ~~strike~~ world");
        assert_eq!(segments.len(), 3);
        assert!(matches!(segments[0], TextSegment::Plain("Hello ")));
        assert!(matches!(segments[1], TextSegment::Strike("strike")));
        assert!(matches!(segments[2], TextSegment::Plain(" world")));
    }

    #[test]
    fn test_parse_inline_formatting_code() {
        let segments = parse_inline_formatting("Hello `code` world");
        assert_eq!(segments.len(), 3);
        assert!(matches!(segments[0], TextSegment::Plain("Hello ")));
        assert!(matches!(segments[1], TextSegment::Code("code")));
        assert!(matches!(segments[2], TextSegment::Plain(" world")));
    }

    #[test]
    fn test_parse_inline_formatting_multiple_formats() {
        let segments = parse_inline_formatting("**bold** and *italic* and `code`");
        assert_eq!(segments.len(), 5);
        assert!(matches!(segments[0], TextSegment::Bold("bold")));
        assert!(matches!(segments[1], TextSegment::Plain(" and ")));
        assert!(matches!(segments[2], TextSegment::Italic("italic")));
        assert!(matches!(segments[3], TextSegment::Plain(" and ")));
        assert!(matches!(segments[4], TextSegment::Code("code")));
    }

    #[test]
    fn test_parse_inline_formatting_unclosed_bold() {
        // Unclosed markers should be treated as plain text
        let segments = parse_inline_formatting("Hello **unclosed");
        // The ** doesn't match, so it gets treated character by character
        assert!(!segments.is_empty());
        // Should contain the original text as plain segments
    }

    #[test]
    fn test_parse_inline_formatting_empty_markers_ignored() {
        // Empty markers like ** ** or `` should not create empty formatted segments
        let segments = parse_inline_formatting("test ** ** test");
        // The ** ** has space inside which IS content, so it creates a bold with space
        // But truly empty like **** would be plain
        assert!(!segments.is_empty());
    }

    #[test]
    fn test_parse_inline_formatting_code_with_special_chars() {
        // Code blocks should preserve content literally
        let segments = parse_inline_formatting("Use `**not bold**` for emphasis");
        assert_eq!(segments.len(), 3);
        assert!(matches!(segments[0], TextSegment::Plain("Use ")));
        assert!(matches!(segments[1], TextSegment::Code("**not bold**")));
        assert!(matches!(segments[2], TextSegment::Plain(" for emphasis")));
    }

    #[test]
    fn test_parse_inline_formatting_adjacent_formats() {
        let segments = parse_inline_formatting("**bold***italic*");
        assert_eq!(segments.len(), 2);
        assert!(matches!(segments[0], TextSegment::Bold("bold")));
        assert!(matches!(segments[1], TextSegment::Italic("italic")));
    }

    #[test]
    fn test_parse_inline_formatting_at_boundaries() {
        // Format at start
        let segments = parse_inline_formatting("**bold** text");
        assert_eq!(segments.len(), 2);
        assert!(matches!(segments[0], TextSegment::Bold("bold")));

        // Format at end
        let segments = parse_inline_formatting("text **bold**");
        assert_eq!(segments.len(), 2);
        assert!(matches!(segments[1], TextSegment::Bold("bold")));

        // Only format
        let segments = parse_inline_formatting("**only bold**");
        assert_eq!(segments.len(), 1);
        assert!(matches!(segments[0], TextSegment::Bold("only bold")));
    }

    // =========================================================================
    // XML serialization helpers (for write_markdown_to_fragment golden tests)
    // =========================================================================

    /// Serialize XmlFragment to XML string for comparison with golden references.
    /// This walks the Y.js document tree and produces XML like Tiptap does.
    fn serialize_xml_fragment_to_string<R: yrs::ReadTxn>(
        txn: &R,
        frag: &yrs::XmlFragmentRef,
    ) -> String {
        use yrs::XmlFragment;

        let mut result = String::new();
        for child in frag.children(txn) {
            serialize_xml_node(txn, &child, &mut result);
        }
        result
    }

    fn serialize_xml_node<R: yrs::ReadTxn>(txn: &R, node: &yrs::XmlOut, result: &mut String) {
        use yrs::{GetString, Xml, XmlFragment, XmlOut};

        match node {
            XmlOut::Element(elem) => {
                let tag = elem.tag();
                result.push('<');
                result.push_str(&tag);

                // Add attributes if any
                for (key, value) in elem.attributes(txn) {
                    result.push(' ');
                    result.push_str(key);
                    result.push_str("=\"");
                    result.push_str(&value);
                    result.push('"');
                }
                result.push('>');

                // Serialize children
                for child in elem.children(txn) {
                    serialize_xml_node(txn, &child, result);
                }

                result.push_str("</");
                result.push_str(&tag);
                result.push('>');
            }
            XmlOut::Text(text) => {
                result.push_str(&text.get_string(txn));
            }
            XmlOut::Fragment(frag) => {
                for child in frag.children(txn) {
                    serialize_xml_node(txn, &child, result);
                }
            }
        }
    }

    /// Create a Y.js document with markdown using write_markdown_to_fragment (production code path)
    /// and return the serialized XML. This tests the actual code used for MCP comments.
    fn write_markdown_to_fragment_xml(markdown: &str) -> String {
        use yrs::{Doc, Map, Transact, WriteTxn, XmlFragmentPrelim};

        let doc = Doc::new();
        let mut txn = doc.transact_mut();

        let root = txn.get_or_insert_map("test");
        let frag: yrs::XmlFragmentRef =
            root.insert(&mut txn, "content", XmlFragmentPrelim::default());

        // Use the production function
        write_markdown_to_fragment(&mut txn, &frag, markdown, None);

        serialize_xml_fragment_to_string(&txn, &frag)
    }

    #[test]
    fn test_write_markdown_to_fragment() {
        // Test the new write_markdown_to_fragment function that uses format() API
        use yrs::{Doc, GetString, Map, Transact, WriteTxn, XmlFragmentPrelim};

        let markdown = "**Completed**: Fixed the issue.\n\nBoth `code1` and `code2` work.";

        let doc = Doc::new();
        let mut txn = doc.transact_mut();

        let root = txn.get_or_insert_map("test");
        let frag: yrs::XmlFragmentRef =
            root.insert(&mut txn, "content", XmlFragmentPrelim::default());

        // Use the new function
        write_markdown_to_fragment(&mut txn, &frag, markdown, None);

        // Check the output
        let result = frag.get_string(&txn);
        println!("write_markdown_to_fragment result: {}", result);

        // Should contain properly formatted marks
        assert!(
            result.contains("<bold>Completed</bold>"),
            "Expected <bold>Completed</bold>, got: {}",
            result
        );
        assert!(
            result.contains("<code>code1</code>"),
            "Expected <code>code1</code>, got: {}",
            result
        );
        assert!(
            result.contains("<code>code2</code>"),
            "Expected <code>code2</code>, got: {}",
            result
        );
    }

    #[test]
    fn test_xmltext_format_api() {
        // Test using XmlText.format() API which is how y-prosemirror stores marks
        use std::collections::HashMap;
        use yrs::types::xml::XmlIn;
        use yrs::{
            Any, Doc, GetString, Map, Text, Transact, WriteTxn, XmlElementPrelim, XmlFragment,
            XmlTextPrelim,
        };

        let doc = Doc::new();
        let mut txn = doc.transact_mut();

        // Create a paragraph element
        let root = txn.get_or_insert_map("test");
        let para_prelim = XmlElementPrelim::new("paragraph", Vec::<XmlIn>::new());
        let para: yrs::XmlElementRef = root.insert(&mut txn, "para", para_prelim);

        // Insert text into paragraph using XmlFragment trait
        let text_ref: yrs::XmlTextRef =
            para.insert(&mut txn, 0, XmlTextPrelim::new("Hello bold world"));

        // Apply bold formatting to "bold" (positions 6-10)
        let mut attrs = HashMap::new();
        attrs.insert("bold".into(), Any::Bool(true));
        text_ref.format(&mut txn, 6, 4, attrs.into());

        // Check what get_string produces
        let result = para.get_string(&txn);
        println!("XmlText with format() result: {}", result);

        // Should contain <bold>bold</bold>
        assert!(
            result.contains("<bold>bold</bold>"),
            "Expected <bold>bold</bold>, got: {}",
            result
        );
    }

    // =========================================================================
    // Item Reference Parsing Tests
    // =========================================================================

    #[test]
    fn test_parse_item_reference_plain_number() {
        assert_eq!(parse_item_reference("42"), Some(42));
        assert_eq!(parse_item_reference("1"), Some(1));
        assert_eq!(parse_item_reference("12345"), Some(12345));
    }

    #[test]
    fn test_parse_item_reference_with_prefix() {
        assert_eq!(parse_item_reference("WDO-42"), Some(42));
        assert_eq!(parse_item_reference("ABC-123"), Some(123));
        assert_eq!(parse_item_reference("PROJECT-1"), Some(1));
    }

    #[test]
    fn test_parse_item_reference_with_whitespace() {
        assert_eq!(parse_item_reference(" 42 "), Some(42));
        assert_eq!(parse_item_reference(" WDO-42 "), Some(42));
    }

    #[test]
    fn test_parse_item_reference_invalid() {
        assert_eq!(parse_item_reference(""), None);
        assert_eq!(parse_item_reference("abc"), None);
        assert_eq!(parse_item_reference("0"), None);
        assert_eq!(parse_item_reference("-1"), None);
        assert_eq!(parse_item_reference("WDO-"), None);
        assert_eq!(parse_item_reference("WDO-abc"), None);
    }

    #[test]
    fn test_parse_inline_formatting_item_ref() {
        let segments = parse_inline_formatting("See [[WDO-42]] for details");
        assert_eq!(segments.len(), 3);
        assert!(matches!(segments[0], TextSegment::Plain("See ")));
        assert!(matches!(segments[1], TextSegment::ItemRef(42)));
        assert!(matches!(segments[2], TextSegment::Plain(" for details")));
    }

    #[test]
    fn test_parse_inline_formatting_item_ref_plain_number() {
        let segments = parse_inline_formatting("Check [[123]]");
        assert_eq!(segments.len(), 2);
        assert!(matches!(segments[0], TextSegment::Plain("Check ")));
        assert!(matches!(segments[1], TextSegment::ItemRef(123)));
    }

    #[test]
    fn test_parse_inline_formatting_multiple_item_refs() {
        let segments = parse_inline_formatting("[[1]] and [[2]]");
        assert_eq!(segments.len(), 3);
        assert!(matches!(segments[0], TextSegment::ItemRef(1)));
        assert!(matches!(segments[1], TextSegment::Plain(" and ")));
        assert!(matches!(segments[2], TextSegment::ItemRef(2)));
    }

    #[test]
    fn test_parse_inline_formatting_item_ref_with_formatting() {
        let segments = parse_inline_formatting("**bold** and [[42]]");
        assert_eq!(segments.len(), 3);
        assert!(matches!(segments[0], TextSegment::Bold("bold")));
        assert!(matches!(segments[1], TextSegment::Plain(" and ")));
        assert!(matches!(segments[2], TextSegment::ItemRef(42)));
    }

    // =========================================================================
    // Link & Image Parsing Tests
    // =========================================================================

    #[test]
    fn test_parse_inline_formatting_link() {
        let segments = parse_inline_formatting("See [Wodo](https://wodo.dev) now");
        assert_eq!(segments.len(), 3);
        assert!(matches!(segments[0], TextSegment::Plain("See ")));
        assert!(matches!(
            segments[1],
            TextSegment::Link {
                text: "Wodo",
                href: "https://wodo.dev"
            }
        ));
        assert!(matches!(segments[2], TextSegment::Plain(" now")));
    }

    #[test]
    fn test_parse_inline_formatting_image() {
        let segments = parse_inline_formatting("![pic](/api/spaces/X/attachments/Y)");
        assert_eq!(segments.len(), 1);
        assert!(matches!(
            segments[0],
            TextSegment::Image {
                alt: "pic",
                src: "/api/spaces/X/attachments/Y"
            }
        ));
    }

    #[test]
    fn test_standalone_image_is_block_not_paragraph() {
        // The editor's `image` node is block-level (inline:false): a lone image
        // line must be a fragment sibling, NOT nested in a paragraph (ProseMirror
        // drops a schema-invalid inline image). Regression for MTR-5's missing
        // inline image.
        let xml = write_markdown_to_fragment_xml("![pic](/api/spaces/X/attachments/Y)");
        assert!(xml.contains("<image"), "block image element present: {xml}");
        assert!(
            xml.contains("/api/spaces/X/attachments/Y"),
            "src carried: {xml}"
        );
        assert!(
            !xml.contains("paragraph"),
            "lone image must not be wrapped in a paragraph: {xml}"
        );
    }

    #[test]
    fn test_parse_inline_formatting_image_inline_in_paragraph() {
        // An image embedded between text.
        let segments = parse_inline_formatting("Before ![pic](/img.png) after");
        assert_eq!(segments.len(), 3);
        assert!(matches!(segments[0], TextSegment::Plain("Before ")));
        assert!(matches!(
            segments[1],
            TextSegment::Image {
                alt: "pic",
                src: "/img.png"
            }
        ));
        assert!(matches!(segments[2], TextSegment::Plain(" after")));
    }

    #[test]
    fn test_parse_inline_formatting_image_not_mistaken_for_link() {
        // `![...]` must parse as an image, never as plain `!` + link.
        let segments = parse_inline_formatting("![alt](u)");
        assert_eq!(segments.len(), 1);
        assert!(matches!(segments[0], TextSegment::Image { .. }));
    }

    #[test]
    fn test_parse_inline_formatting_item_ref_not_mistaken_for_link() {
        // `[[WDO-42]]` must parse as an item ref, never as a `[text](url)` link.
        let segments = parse_inline_formatting("[[WDO-42]]");
        assert_eq!(segments.len(), 1);
        assert!(matches!(segments[0], TextSegment::ItemRef(42)));
    }

    #[test]
    fn test_parse_inline_formatting_lone_exclamation_is_plain() {
        // A bare `!` (not an image opener) stays plain text.
        let segments = parse_inline_formatting("Wow! Nice");
        assert_eq!(
            segments
                .iter()
                .filter(|s| matches!(s, TextSegment::Image { .. }))
                .count(),
            0,
            "no spurious image"
        );
        let joined: String = segments
            .iter()
            .map(|s| match s {
                TextSegment::Plain(p) => *p,
                _ => "",
            })
            .collect();
        assert_eq!(joined, "Wow! Nice");
    }

    #[test]
    fn test_parse_link_like_basic() {
        assert_eq!(
            parse_link_like("[a](b)rest"),
            Some(("a", "b", 6)),
            "consumes only the link span"
        );
        // Empty url → no match.
        assert_eq!(parse_link_like("[a]()"), None);
        // No paren after `]` → no match.
        assert_eq!(parse_link_like("[a] (b)"), None);
        // Not a link at all.
        assert_eq!(parse_link_like("plain"), None);
    }

    // =========================================================================
    // Extract Helper Tests
    // =========================================================================

    #[test]
    fn test_extract_ordered_list_text_basic() {
        assert_eq!(extract_ordered_list_text("1. First item"), "First item");
        assert_eq!(extract_ordered_list_text("2. Second item"), "Second item");
        assert_eq!(extract_ordered_list_text("10. Tenth item"), "Tenth item");
    }

    #[test]
    fn test_extract_ordered_list_text_preserves_content() {
        assert_eq!(
            extract_ordered_list_text("1. Item with **bold** text"),
            "Item with **bold** text"
        );
    }

    #[test]
    fn test_extract_ordered_list_text_no_number() {
        // When there's no number prefix, returns original text
        assert_eq!(
            extract_ordered_list_text("No number here"),
            "No number here"
        );
    }

    #[test]
    fn test_extract_task_item_unchecked() {
        let (text, checked) = extract_task_item("- [ ] Task to do");
        assert_eq!(text, "Task to do");
        assert!(!checked, "Unchecked task should return false");
    }

    #[test]
    fn test_extract_task_item_checked() {
        let (text, checked) = extract_task_item("- [x] Done task");
        assert_eq!(text, "Done task");
        assert!(checked, "Checked task should return true");

        // Also test uppercase X
        let (text2, checked2) = extract_task_item("- [X] Also done");
        assert_eq!(text2, "Also done");
        assert!(checked2, "Uppercase X should also be checked");
    }

    #[test]
    fn test_extract_task_item_no_checkbox() {
        // When there's no checkbox pattern, returns original text
        let (text, checked) = extract_task_item("Regular list item");
        assert_eq!(text, "Regular list item");
        assert!(!checked, "No checkbox should default to unchecked");
    }

    #[test]
    fn test_extract_task_item_preserves_formatting() {
        let (text, checked) = extract_task_item("- [x] Task with `code` and **bold**");
        assert_eq!(text, "Task with `code` and **bold**");
        assert!(checked);
    }

    // =========================================================================
    // write_markdown_to_fragment Tests (Production MCP code path)
    // =========================================================================

    #[test]
    fn test_write_markdown_fragment_heading() {
        let xml = write_markdown_to_fragment_xml("# Heading 1");
        assert_eq!(xml, "<heading level=\"1\">Heading 1</heading>");

        let xml = write_markdown_to_fragment_xml("## Heading 2");
        assert_eq!(xml, "<heading level=\"2\">Heading 2</heading>");

        let xml = write_markdown_to_fragment_xml("### Heading 3");
        assert_eq!(xml, "<heading level=\"3\">Heading 3</heading>");
    }

    #[test]
    fn test_write_markdown_fragment_heading_with_formatting() {
        let xml = write_markdown_to_fragment_xml("## **Important** heading");
        assert!(
            xml.contains("<heading level=\"2\">"),
            "Expected heading, got: {}",
            xml
        );
        assert!(
            xml.contains("<bold>Important</bold>"),
            "Expected bold, got: {}",
            xml
        );
    }

    #[test]
    fn test_write_markdown_fragment_code_block() {
        let markdown = "```rust\nfn main() {\n    println!(\"Hello\");\n}\n```";
        let xml = write_markdown_to_fragment_xml(markdown);

        assert!(
            xml.contains("<codeBlock language=\"rust\">"),
            "Expected codeBlock with language, got: {}",
            xml
        );
        assert!(
            xml.contains("fn main()"),
            "Expected code content, got: {}",
            xml
        );
    }

    #[test]
    fn test_write_markdown_fragment_code_block_no_language() {
        let markdown = "```\nsome code\n```";
        let xml = write_markdown_to_fragment_xml(markdown);

        assert!(
            xml.contains("<codeBlock>"),
            "Expected codeBlock without language attr, got: {}",
            xml
        );
    }

    #[test]
    fn test_write_markdown_fragment_bullet_list() {
        let markdown = "- Item 1\n- Item 2\n- Item 3";
        let xml = write_markdown_to_fragment_xml(markdown);

        assert!(
            xml.contains("<bulletList>"),
            "Expected bulletList, got: {}",
            xml
        );
        assert!(
            xml.contains("<listItem>"),
            "Expected listItem, got: {}",
            xml
        );
        assert!(
            xml.contains("<paragraph>Item 1</paragraph>"),
            "Expected Item 1, got: {}",
            xml
        );
        assert!(
            xml.contains("<paragraph>Item 2</paragraph>"),
            "Expected Item 2, got: {}",
            xml
        );
    }

    #[test]
    fn test_write_markdown_fragment_bullet_list_with_formatting() {
        let xml = write_markdown_to_fragment_xml("- Item with **bold** text");
        assert!(
            xml.contains("<bold>bold</bold>"),
            "Expected bold in list item, got: {}",
            xml
        );
    }

    #[test]
    fn test_write_markdown_fragment_ordered_list() {
        let markdown = "1. First\n2. Second\n3. Third";
        let xml = write_markdown_to_fragment_xml(markdown);

        assert!(
            xml.contains("<orderedList start=\"1\">"),
            "Expected orderedList with start attr, got: {}",
            xml
        );
        assert!(
            xml.contains("<listItem>"),
            "Expected listItem, got: {}",
            xml
        );
        assert!(
            xml.contains("<paragraph>First</paragraph>"),
            "Expected First, got: {}",
            xml
        );
    }

    #[test]
    fn test_write_markdown_fragment_task_list() {
        let markdown = "- [ ] Unchecked task\n- [x] Checked task";
        let xml = write_markdown_to_fragment_xml(markdown);

        assert!(
            xml.contains("<taskList>"),
            "Expected taskList, got: {}",
            xml
        );
        assert!(
            xml.contains("<taskItem checked=\"false\">"),
            "Expected unchecked task, got: {}",
            xml
        );
        assert!(
            xml.contains("<taskItem checked=\"true\">"),
            "Expected checked task, got: {}",
            xml
        );
        assert!(
            xml.contains("Unchecked task"),
            "Expected task text, got: {}",
            xml
        );
    }

    #[test]
    fn test_write_markdown_fragment_blockquote() {
        let markdown = "> This is a quote\n> Second line";
        let xml = write_markdown_to_fragment_xml(markdown);

        assert!(
            xml.contains("<blockquote>"),
            "Expected blockquote, got: {}",
            xml
        );
        assert!(
            xml.contains("<paragraph>This is a quote</paragraph>"),
            "Expected quote text, got: {}",
            xml
        );
    }

    #[test]
    fn test_write_markdown_fragment_blockquote_with_formatting() {
        let xml = write_markdown_to_fragment_xml("> Quote with *italic*");
        assert!(
            xml.contains("<italic>italic</italic>"),
            "Expected italic in blockquote, got: {}",
            xml
        );
    }

    #[test]
    fn test_write_markdown_fragment_paragraph() {
        let xml = write_markdown_to_fragment_xml("Simple paragraph text");
        assert_eq!(xml, "<paragraph>Simple paragraph text</paragraph>");
    }

    #[test]
    fn test_write_markdown_fragment_inline_formatting() {
        let xml = write_markdown_to_fragment_xml("Text with **bold** and *italic* and `code`");
        assert!(
            xml.contains("<bold>bold</bold>"),
            "Expected bold, got: {}",
            xml
        );
        assert!(
            xml.contains("<italic>italic</italic>"),
            "Expected italic, got: {}",
            xml
        );
        assert!(
            xml.contains("<code>code</code>"),
            "Expected code, got: {}",
            xml
        );
    }

    // =========================================================================
    // Link & Image Tests — Production path (write_markdown_to_fragment_xml)
    // =========================================================================

    #[test]
    fn test_write_markdown_fragment_link() {
        // "Wodo" carries a `link` mark with href https://wodo.dev.
        let xml = write_markdown_to_fragment_xml("[Wodo](https://wodo.dev)");
        assert!(
            xml.contains("<link href=\"https://wodo.dev\">Wodo</link>"),
            "Expected link mark with href, got: {}",
            xml
        );
    }

    #[test]
    fn test_write_markdown_fragment_link_inline() {
        // A link surrounded by other text in the same paragraph.
        let xml = write_markdown_to_fragment_xml("Visit [Wodo](https://wodo.dev) today");
        assert!(
            xml.contains("Visit "),
            "Expected leading text, got: {}",
            xml
        );
        assert!(
            xml.contains("<link href=\"https://wodo.dev\">Wodo</link>"),
            "Expected link mark, got: {}",
            xml
        );
        assert!(
            xml.contains(" today"),
            "Expected trailing text, got: {}",
            xml
        );
    }

    #[test]
    fn test_write_markdown_fragment_image() {
        // An `image` element node with the src attribute.
        let xml = write_markdown_to_fragment_xml("![pic](/api/spaces/X/attachments/Y)");
        assert!(
            xml.contains("<image"),
            "Expected image element, got: {}",
            xml
        );
        assert!(
            xml.contains("src=\"/api/spaces/X/attachments/Y\""),
            "Expected image src, got: {}",
            xml
        );
        assert!(
            xml.contains("alt=\"pic\""),
            "Expected image alt, got: {}",
            xml
        );
    }

    #[test]
    fn test_write_markdown_fragment_image_inline_in_paragraph() {
        // An image inline within a paragraph (text before + after).
        let xml = write_markdown_to_fragment_xml("Before ![pic](/img.png) after");
        assert!(
            xml.contains("<paragraph>"),
            "Expected paragraph, got: {}",
            xml
        );
        assert!(
            xml.contains("<image") && xml.contains("src=\"/img.png\""),
            "Expected inline image node, got: {}",
            xml
        );
        // Surrounding text preserved.
        assert!(
            xml.contains("Before ") && xml.contains(" after"),
            "Expected surrounding text, got: {}",
            xml
        );
    }

    #[test]
    fn test_write_markdown_fragment_image_no_alt() {
        // Empty alt → no alt attribute emitted.
        let xml = write_markdown_to_fragment_xml("![](/img.png)");
        assert!(
            xml.contains("src=\"/img.png\""),
            "Expected image src, got: {}",
            xml
        );
        assert!(!xml.contains("alt="), "Expected no alt attr, got: {}", xml);
    }

    // =========================================================================
    // Table Tests — Production path (write_markdown_to_fragment_xml)
    // =========================================================================

    #[test]
    fn test_write_markdown_fragment_table_basic() {
        let markdown = "| Name | Age |\n| --- | --- |\n| Alice | 30 |\n| Bob | 25 |";
        let xml = write_markdown_to_fragment_xml(markdown);

        assert!(xml.contains("<table>"), "Expected table, got: {}", xml);
        assert!(
            xml.contains("<tableHeader"),
            "Expected tableHeader, got: {}",
            xml
        );
        assert!(
            xml.contains("<tableCell"),
            "Expected tableCell, got: {}",
            xml
        );
        assert!(xml.contains("Alice"), "Expected Alice, got: {}", xml);
        assert!(xml.contains("Bob"), "Expected Bob, got: {}", xml);
        // No colspan/rowspan attributes — omitted so tiptap uses numeric defaults
        assert!(
            !xml.contains("colspan"),
            "Should NOT have colspan attr (yrs stores as string, breaks tiptap), got: {}",
            xml
        );
    }

    #[test]
    fn test_write_markdown_fragment_consecutive_tables() {
        let markdown = "| A | B |\n| --- | --- |\n| 1 | 2 |\n\n| X | Y |\n| --- | --- |\n| 3 | 4 |";
        let xml = write_markdown_to_fragment_xml(markdown);

        let table_count = xml.matches("<table>").count();
        assert_eq!(
            table_count, 2,
            "Expected 2 separate tables, got {}: {}",
            table_count, xml
        );
        assert!(
            xml.contains("<paragraph>1</paragraph>"),
            "First table cell, got: {}",
            xml
        );
        assert!(
            xml.contains("<paragraph>3</paragraph>"),
            "Second table cell, got: {}",
            xml
        );
    }

    #[test]
    fn test_write_markdown_fragment_table_with_formatting() {
        let markdown = "| Feature | Status |\n| --- | --- |\n| **Tables** | `done` |";
        let xml = write_markdown_to_fragment_xml(markdown);

        assert!(
            xml.contains("<bold>Tables</bold>"),
            "Expected bold, got: {}",
            xml
        );
        assert!(
            xml.contains("<code>done</code>"),
            "Expected code, got: {}",
            xml
        );
    }

    #[test]
    fn test_write_markdown_fragment_table_header_only() {
        let markdown = "| Col A | Col B |\n| --- | --- |";
        let xml = write_markdown_to_fragment_xml(markdown);

        assert!(xml.contains("<table>"), "Expected table, got: {}", xml);
        assert!(
            xml.contains("<tableHeader"),
            "Expected headers, got: {}",
            xml
        );
        assert!(
            !xml.contains("<tableCell"),
            "No data cells expected, got: {}",
            xml
        );
    }

    #[test]
    fn test_write_markdown_fragment_table_padding() {
        let markdown = "| A | B | C |\n| --- | --- | --- |\n| x |";
        let xml = write_markdown_to_fragment_xml(markdown);

        let header_count = xml.matches("<tableHeader").count();
        let cell_count = xml.matches("<tableCell").count();
        assert_eq!(header_count, 3, "Expected 3 headers, got: {}", xml);
        assert_eq!(cell_count, 3, "Expected 3 cells (2 padded), got: {}", xml);
    }

    #[test]
    fn test_write_markdown_fragment_table_escaped_pipe() {
        let markdown = "| A | B |\n| --- | --- |\n| hello \\| world | ok |";
        let xml = write_markdown_to_fragment_xml(markdown);

        assert!(
            xml.contains("hello | world"),
            "Expected unescaped pipe in cell, got: {}",
            xml
        );
    }

    #[test]
    fn test_write_markdown_fragment_table_in_document() {
        let markdown = "# Report\n\nSome intro text.\n\n| Metric | Value |\n| --- | --- |\n| Speed | Fast |\n\nConclusion paragraph.";
        let xml = write_markdown_to_fragment_xml(markdown);

        assert!(
            xml.contains("<heading level=\"1\">Report</heading>"),
            "Expected heading, got: {}",
            xml
        );
        assert!(
            xml.contains("<paragraph>Some intro text.</paragraph>"),
            "Expected intro, got: {}",
            xml
        );
        assert!(xml.contains("<table>"), "Expected table, got: {}", xml);
        assert!(
            xml.contains("<paragraph>Conclusion paragraph.</paragraph>"),
            "Expected conclusion, got: {}",
            xml
        );
    }

    // =========================================================================
    // Table helper unit tests
    // =========================================================================

    #[test]
    fn test_scan_table_separator_valid() {
        assert_eq!(scan_table_separator("| --- | --- |"), Some(2));
        assert_eq!(scan_table_separator("| :--- | ---: |"), Some(2));
        assert_eq!(scan_table_separator("| :---: | --- |"), Some(2));
        assert_eq!(scan_table_separator("--- | ---"), Some(2));
        assert_eq!(scan_table_separator("| - | - |"), Some(2));
        assert_eq!(scan_table_separator("| --- | --- | --- |"), Some(3));
    }

    #[test]
    fn test_scan_table_separator_invalid() {
        assert_eq!(scan_table_separator("| abc | def |"), None);
        assert_eq!(scan_table_separator("no pipes here"), None);
        assert_eq!(scan_table_separator("| | |"), None); // no dashes
        assert_eq!(scan_table_separator(""), None);
    }

    #[test]
    fn test_split_table_row_basic() {
        assert_eq!(split_table_row("| a | b | c |"), vec!["a", "b", "c"]);
        assert_eq!(split_table_row("a | b | c"), vec!["a", "b", "c"]);
        assert_eq!(split_table_row("| a | b"), vec!["a", "b"]);
    }

    #[test]
    fn test_split_table_row_escaped() {
        assert_eq!(split_table_row("| a \\| b | c |"), vec!["a | b", "c"]);
    }

    #[test]
    fn test_split_table_row_empty_cells() {
        assert_eq!(split_table_row("| | b |"), vec!["", "b"]);
        assert_eq!(split_table_row("| a | |"), vec!["a", ""]);
    }

    #[test]
    fn test_write_markdown_fragment_comprehensive() {
        // Test all block types together - this is the pattern that failed in WDO-22
        let markdown = r#"# Heading 1

## Heading 2 with **bold**

Regular paragraph with `code` and *italic*.

- Bullet item 1
- Bullet item with **bold**

1. Ordered item 1
2. Ordered item 2

- [ ] Unchecked task
- [x] Checked task

```rust
fn main() {
    println!("Hello");
}
```

> Blockquote with *italic*

| Col A | Col B |
| --- | --- |
| val 1 | val 2 |"#;

        let xml = write_markdown_to_fragment_xml(markdown);

        // Verify all block types are present
        assert!(
            xml.contains("<heading level=\"1\">Heading 1</heading>"),
            "Expected heading 1, got: {}",
            xml
        );
        assert!(
            xml.contains("<heading level=\"2\">"),
            "Expected heading 2, got: {}",
            xml
        );
        assert!(
            xml.contains("<bulletList>"),
            "Expected bulletList, got: {}",
            xml
        );
        assert!(
            xml.contains("<orderedList"),
            "Expected orderedList, got: {}",
            xml
        );
        assert!(
            xml.contains("<taskList>"),
            "Expected taskList, got: {}",
            xml
        );
        assert!(
            xml.contains("<taskItem checked=\"false\">"),
            "Expected unchecked task, got: {}",
            xml
        );
        assert!(
            xml.contains("<taskItem checked=\"true\">"),
            "Expected checked task, got: {}",
            xml
        );
        assert!(
            xml.contains("<codeBlock"),
            "Expected codeBlock, got: {}",
            xml
        );
        assert!(
            xml.contains("<blockquote>"),
            "Expected blockquote, got: {}",
            xml
        );
        assert!(
            xml.contains("<code>code</code>"),
            "Expected inline code, got: {}",
            xml
        );
        assert!(
            xml.contains("<italic>italic</italic>"),
            "Expected italic, got: {}",
            xml
        );
        assert!(
            xml.contains("<bold>bold</bold>"),
            "Expected bold, got: {}",
            xml
        );
        assert!(xml.contains("<table>"), "Expected table, got: {}", xml);
        assert!(
            xml.contains("<tableHeader"),
            "Expected tableHeader, got: {}",
            xml
        );
        assert!(
            xml.contains("<paragraph>val 1</paragraph>"),
            "Expected table cell content, got: {}",
            xml
        );
    }

    // =========================================================================
    // write_markdown_to_fragment Tests (real-world documents)
    // =========================================================================

    #[test]
    fn test_write_markdown_fragment_research_document() {
        // Test the exact markdown pattern that exposed the WDO-22 bug
        let markdown = r#"## Research Findings

### Tools by Technology

**Rust (Cargo)**
- **cargo-about** - Generates license reports

```bash
cargo about init
```

> Note: This is recommended"#;

        let xml = write_markdown_to_fragment_xml(markdown);

        // Headings must NOT be rendered as paragraphs with literal markdown
        assert!(
            !xml.contains("<paragraph>## Research"),
            "Heading should not be in paragraph: {}",
            xml
        );
        assert!(
            xml.contains("<heading level=\"2\">Research Findings</heading>"),
            "Expected heading 2, got: {}",
            xml
        );
        assert!(
            xml.contains("<heading level=\"3\">Tools by Technology</heading>"),
            "Expected heading 3, got: {}",
            xml
        );
        assert!(
            xml.contains("<bold>Rust (Cargo)</bold>"),
            "Expected bold paragraph, got: {}",
            xml
        );
        assert!(
            xml.contains("<bulletList>"),
            "Expected bulletList, got: {}",
            xml
        );
        assert!(
            xml.contains("<codeBlock language=\"bash\">"),
            "Expected codeBlock, got: {}",
            xml
        );
        assert!(
            xml.contains("<blockquote>"),
            "Expected blockquote, got: {}",
            xml
        );
    }
}
