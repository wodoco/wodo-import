//! Saved-view filter token codec.
//!
//! View `filters` strings are comma-separated tokens; UUID-bearing tokens
//! (`u:`, `t:`, `m:`, `cy:`, and `{label}.{value}` pairs) encode the raw 16
//! UUID bytes as base64url without padding. This mirrors the client's
//! `view_url.encode_uuid` (client/src/view_url.gleam) — the two
//! implementations are pinned to each other by fixed-vector tests on both
//! sides.

use base64::Engine as _;
use std::collections::HashMap;
use uuid::Uuid;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// UUID string → compact filter token payload.
pub fn encode_filter_uuid(uuid: &str) -> Option<String> {
    let parsed = Uuid::parse_str(uuid).ok()?;
    Some(B64.encode(parsed.as_bytes()))
}

/// Compact filter token payload → UUID string.
pub fn decode_filter_uuid(token: &str) -> Option<String> {
    let bytes = B64.decode(token).ok()?;
    let arr: [u8; 16] = bytes.try_into().ok()?;
    Some(Uuid::from_bytes(arr).to_string())
}

/// Rewrite the UUID inside a `u:`/`t:` token through `map` (old UUID → new
/// UUID). Returns the rewritten token, or None when the payload doesn't
/// decode or the UUID has no mapping.
fn remap_principal_token(
    prefix: &str,
    payload: &str,
    map: &HashMap<String, String>,
) -> Option<String> {
    let old = decode_filter_uuid(payload)?;
    let new = map.get(&old)?;
    Some(format!("{}{}", prefix, encode_filter_uuid(new)?))
}

/// Remap a view filter string for import: `u:`/`t:` tokens go through the
/// user/team maps (unmappable ones are dropped and counted); every other
/// token — label/milestone/cycle UUIDs carry over verbatim on import, and
/// keyword or unknown tokens are never stripped — is kept as-is.
pub fn remap_filters_for_import(
    filter_str: &str,
    user_map: &HashMap<String, String>,
    team_map: &HashMap<String, String>,
    dropped: &mut usize,
) -> Option<String> {
    map_principal_tokens(filter_str, |prefix, payload| {
        let map = if prefix == "u:" { user_map } else { team_map };
        match remap_principal_token(prefix, payload, map) {
            Some(tok) => Some(tok),
            None => {
                *dropped += 1;
                None
            }
        }
    })
}

/// Remap `u:`/`t:` tokens through a combined old→new UUID map, keeping
/// unmappable tokens verbatim (anonymization rewrites identities it knows
/// about; it never drops filters).
pub fn remap_filters_through_uuid_map(
    filter_str: &str,
    uuid_map: &HashMap<String, String>,
) -> Option<String> {
    map_principal_tokens(filter_str, |prefix, payload| {
        Some(
            remap_principal_token(prefix, payload, uuid_map)
                .unwrap_or_else(|| format!("{}{}", prefix, payload)),
        )
    })
}

/// Apply `f` to each `u:`/`t:` token (passing prefix and payload); `f`
/// returning None drops the token. All other tokens pass through verbatim.
fn map_principal_tokens(
    filter_str: &str,
    mut f: impl FnMut(&str, &str) -> Option<String>,
) -> Option<String> {
    let kept: Vec<String> = filter_str
        .split(',')
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .filter_map(|token| {
            for prefix in ["u:", "t:"] {
                if let Some(payload) = token.strip_prefix(prefix) {
                    return f(prefix, payload);
                }
            }
            Some(token.to_string())
        })
        .collect();

    if kept.is_empty() {
        None
    } else {
        Some(kept.join(","))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixed vector pinned against the client implementation
    /// (client/test/view_url_test.gleam carries the mirror assertion):
    /// base64url-no-pad of the raw 16 bytes.
    #[test]
    fn test_codec_fixed_vector() {
        let uuid = "dddddddd-0000-0000-0000-000000000004";
        let token = encode_filter_uuid(uuid).unwrap();
        assert_eq!(token, "3d3d3QAAAAAAAAAAAAAABA");
        assert_eq!(decode_filter_uuid(&token).unwrap(), uuid);
    }

    #[test]
    fn test_codec_rejects_garbage() {
        assert!(decode_filter_uuid("not-base64url!").is_none());
        assert!(decode_filter_uuid("AAAA").is_none()); // wrong byte length
        assert!(encode_filter_uuid("not-a-uuid").is_none());
    }

    #[test]
    fn test_import_remap_keeps_unknown_and_keyword_tokens() {
        let old_user = "dddddddd-0000-0000-0000-000000000004";
        let new_user = "eeeeeeee-0000-0000-0000-000000000005";
        let user_map = HashMap::from([(old_user.to_string(), new_user.to_string())]);
        let team_map = HashMap::new();

        let u_tok = encode_filter_uuid(old_user).unwrap();
        let t_tok = encode_filter_uuid("ffffffff-0000-0000-0000-000000000006").unwrap();
        let filters = format!("lbl.val,u:{},t:{},cy:abc,d:overdue,zz:future", u_tok, t_tok);

        let mut dropped = 0usize;
        let out = remap_filters_for_import(&filters, &user_map, &team_map, &mut dropped).unwrap();

        let expected_u = format!("u:{}", encode_filter_uuid(new_user).unwrap());
        assert_eq!(
            out,
            format!("lbl.val,{},cy:abc,d:overdue,zz:future", expected_u)
        );
        assert_eq!(dropped, 1); // the unmappable team token
    }

    #[test]
    fn test_import_remap_all_dropped_yields_none() {
        let mut dropped = 0usize;
        let out = remap_filters_for_import(
            &format!(
                "u:{}",
                encode_filter_uuid("dddddddd-0000-0000-0000-000000000004").unwrap()
            ),
            &HashMap::new(),
            &HashMap::new(),
            &mut dropped,
        );
        assert!(out.is_none());
        assert_eq!(dropped, 1);
    }

    #[test]
    fn test_uuid_map_remap_keeps_unmapped_verbatim() {
        let old = "dddddddd-0000-0000-0000-000000000004";
        let new = "eeeeeeee-0000-0000-0000-000000000005";
        let map = HashMap::from([(old.to_string(), new.to_string())]);
        let known = format!("u:{}", encode_filter_uuid(old).unwrap());
        let unknown = format!("t:{}", encode_filter_uuid(new).unwrap());
        let out = remap_filters_through_uuid_map(&format!("{},{}", known, unknown), &map).unwrap();
        assert_eq!(
            out,
            format!("u:{},{}", encode_filter_uuid(new).unwrap(), unknown)
        );
    }
}
