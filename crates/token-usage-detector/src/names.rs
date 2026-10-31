//! Namespaced-name and authority-field validation.
//!
//! One grammar, owned here so every layer of this crate admits exactly the
//! names a host applying the same bound would admit: two lowercase segments
//! separated by `.` or `-`, optionally followed by `.`/`-`/`/`-separated
//! leaves that may keep uppercase letters. A bare word is not namespaced,
//! which is what keeps a vendor from claiming a name the product itself
//! publishes.

/// The longest namespaced name accepted, so a name this crate admits cannot be
/// rejected later by a host that applies the same bound.
pub const MAX_NAMESPACED_NAME_BYTES: usize = 160;

/// The authority fields free-form attributes may never supply.
///
/// These belong to the host's own records: who the effect is for, which effect
/// it is, whether it is authorized, and which state root it is bound to. A
/// payload that could set them through a plain attribute would be writing the
/// host's authority rather than describing itself.
pub const AUTHORITY_FIELDS: [&str; 4] = ["principal", "effectId", "authorized", "stateRoot"];

/// Whether `name` is a namespaced name, such as `vendor.example/render` or
/// `acme.quota.tokens`.
pub fn is_namespaced(name: &str) -> bool {
    if name.is_empty() || name.len() > MAX_NAMESPACED_NAME_BYTES {
        return false;
    }
    let bytes = name.as_bytes();
    let mut cursor = 0;
    if !take_lowercase_word(bytes, &mut cursor) {
        return false;
    }
    if cursor == bytes.len() || !matches!(bytes[cursor], b'.' | b'-') {
        return false;
    }
    cursor += 1;
    if !take_lowercase_word(bytes, &mut cursor) {
        return false;
    }
    while cursor < bytes.len() && matches!(bytes[cursor], b'.' | b'-' | b'/') {
        cursor += 1;
        if !take_while(bytes, &mut cursor, |byte| {
            byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
        }) {
            return false;
        }
    }
    cursor == bytes.len()
}

/// Whether `name` is one of the authority fields, in any of the spellings a
/// payload might offer it in.
///
/// Comparison ignores case and `_`, so `effectId`, `effect_id` and `EFFECTID`
/// are all refused: a spelling difference is not a different field.
pub fn is_authority_field(name: &str) -> bool {
    let normalized: String = name
        .chars()
        .filter(|character| *character != '_')
        .flat_map(char::to_lowercase)
        .collect();
    AUTHORITY_FIELDS
        .iter()
        .any(|field| field.eq_ignore_ascii_case(&normalized))
}

fn take_while(bytes: &[u8], cursor: &mut usize, predicate: impl Fn(u8) -> bool) -> bool {
    let start = *cursor;
    while *cursor < bytes.len() && predicate(bytes[*cursor]) {
        *cursor += 1;
    }
    *cursor > start
}

fn take_lowercase_word(bytes: &[u8], cursor: &mut usize) -> bool {
    take_while(bytes, cursor, |byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit()
    })
}
