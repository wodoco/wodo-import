//! Shared XML deep-copy helpers for Yjs XmlFragment types.
//!
//! Used by both archive_migration (moving items between docs) and
//! template serialization (extracting fragments into temp docs for binary encoding).

use yrs::types::xml::XmlFragment as XmlFragmentTrait;
use yrs::{
    Any, GetString, Out, ReadTxn, Text, TransactionMut, Xml, XmlElementPrelim, XmlFragmentRef,
    XmlOut, XmlTextPrelim,
};

/// Maximum recursion depth for deep copy operations.
/// Protects against stack overflow from maliciously crafted deeply nested documents.
pub const MAX_COPY_DEPTH: u32 = 100;

/// Deep-copy all children from one XmlFragment to another.
///
/// This is the main entry point for fragment-level copying. It preserves
/// elements (with attributes), text (with formatting via `diff()`), and
/// nested fragments.
pub fn deep_copy_xml_fragment<R: ReadTxn>(
    src_xml: &XmlFragmentRef,
    src_txn: &R,
    dst_xml: &XmlFragmentRef,
    dst_txn: &mut TransactionMut,
    depth: u32,
) -> Result<(), String> {
    if depth > MAX_COPY_DEPTH {
        return Err(format!(
            "Maximum nesting depth ({}) exceeded during deep copy",
            MAX_COPY_DEPTH
        ));
    }
    for child in src_xml.children(src_txn) {
        deep_copy_xml_node_to_fragment(src_txn, dst_xml, dst_txn, child, depth + 1)?;
    }
    Ok(())
}

/// Deep-copy an XML node into a fragment parent.
pub fn deep_copy_xml_node_to_fragment<R: ReadTxn>(
    src_txn: &R,
    dst_parent: &XmlFragmentRef,
    dst_txn: &mut TransactionMut,
    node: XmlOut,
    depth: u32,
) -> Result<(), String> {
    if depth > MAX_COPY_DEPTH {
        return Err(format!(
            "Maximum nesting depth ({}) exceeded during deep copy",
            MAX_COPY_DEPTH
        ));
    }
    match node {
        XmlOut::Element(elem) => {
            let tag = elem.tag();
            let new_elem: yrs::XmlElementRef =
                dst_parent.push_back(dst_txn, XmlElementPrelim::empty(tag.as_ref()));

            for (attr_key, attr_value) in elem.attributes(src_txn) {
                new_elem.insert_attribute(dst_txn, attr_key, attr_value);
            }

            for child in elem.children(src_txn) {
                deep_copy_xml_node_to_element(src_txn, &new_elem, dst_txn, child, depth + 1)?;
            }
        }
        XmlOut::Text(text) => {
            let content = text.get_string(src_txn);
            let new_text: yrs::XmlTextRef = dst_parent.push_back(dst_txn, XmlTextPrelim::new(""));

            for diff in text.diff(src_txn, yrs::types::text::YChange::identity) {
                if let Out::Any(Any::String(s)) = diff.insert {
                    let attrs = diff.attributes.map(|a| (*a).clone()).unwrap_or_default();
                    new_text.insert_with_attributes(dst_txn, new_text.len(dst_txn), &s, attrs);
                }
            }

            if new_text.len(dst_txn) == 0 && !content.is_empty() {
                new_text.insert(dst_txn, 0, &content);
            }
        }
        XmlOut::Fragment(frag) => {
            for child in frag.children(src_txn) {
                deep_copy_xml_node_to_fragment(src_txn, dst_parent, dst_txn, child, depth + 1)?;
            }
        }
    }
    Ok(())
}

/// Deep-copy an XML node into an element parent.
pub fn deep_copy_xml_node_to_element<R: ReadTxn>(
    src_txn: &R,
    dst_parent: &yrs::XmlElementRef,
    dst_txn: &mut TransactionMut,
    node: XmlOut,
    depth: u32,
) -> Result<(), String> {
    if depth > MAX_COPY_DEPTH {
        return Err(format!(
            "Maximum nesting depth ({}) exceeded during deep copy",
            MAX_COPY_DEPTH
        ));
    }
    match node {
        XmlOut::Element(elem) => {
            let tag = elem.tag();
            let new_elem: yrs::XmlElementRef =
                dst_parent.push_back(dst_txn, XmlElementPrelim::empty(tag.as_ref()));

            for (attr_key, attr_value) in elem.attributes(src_txn) {
                new_elem.insert_attribute(dst_txn, attr_key, attr_value);
            }

            for child in elem.children(src_txn) {
                deep_copy_xml_node_to_element(src_txn, &new_elem, dst_txn, child, depth + 1)?;
            }
        }
        XmlOut::Text(text) => {
            let content = text.get_string(src_txn);
            let new_text: yrs::XmlTextRef = dst_parent.push_back(dst_txn, XmlTextPrelim::new(""));

            for diff in text.diff(src_txn, yrs::types::text::YChange::identity) {
                if let Out::Any(Any::String(s)) = diff.insert {
                    let attrs = diff.attributes.map(|a| (*a).clone()).unwrap_or_default();
                    new_text.insert_with_attributes(dst_txn, new_text.len(dst_txn), &s, attrs);
                }
            }

            if new_text.len(dst_txn) == 0 && !content.is_empty() {
                new_text.insert(dst_txn, 0, &content);
            }
        }
        XmlOut::Fragment(frag) => {
            for child in frag.children(src_txn) {
                deep_copy_xml_node_to_element(src_txn, dst_parent, dst_txn, child, depth + 1)?;
            }
        }
    }
    Ok(())
}
