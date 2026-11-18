use std::borrow::Cow;

use encoding::all::IBM866;
use encoding::{EncoderTrap, Encoding};

pub const CAPABILITY: &[u8] = b"VANGERS-UTF8/1";
const CHAT_LIMIT: usize = 139;

fn terminated(mut bytes: Vec<u8>) -> Vec<u8> {
    bytes.push(0);
    bytes
}

pub fn single_line(value: &str) -> String {
    value
        .chars()
        .map(|c| match c {
            '\u{0000}'..='\u{001f}' | '\u{007f}' | '\u{0085}' | '\u{2028}' | '\u{2029}' => ' ',
            c => c,
        })
        .collect()
}

fn limit_utf8(value: &str, limit: usize) -> &str {
    let mut end = value.len().min(limit);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

pub fn project_legacy(text: &str, limit: usize, fallback: &str) -> Vec<u8> {
    if let Ok(bytes) = IBM866.encode(text, EncoderTrap::Strict) {
        return terminated(bytes[..bytes.len().min(limit)].to_vec());
    }
    terminated(fallback.as_bytes().to_vec())
}

pub fn name_for<'a>(legacy: &'a [u8], native: Option<&'a [u8]>, unicode: bool) -> Cow<'a, [u8]> {
    if !unicode {
        return Cow::Borrowed(legacy);
    }
    if let Some(native) = native {
        return Cow::Borrowed(native);
    }
    Cow::Owned(cp866_to_utf8(legacy))
}

fn cp866_to_utf8(bytes: &[u8]) -> Vec<u8> {
    let text = crate::utils::convert_cp866_to_utf8(&bytes[..bytes.len() - 1]).unwrap_or_default();
    terminated(single_line(&text).into_bytes())
}

// Input includes exactly one terminating NUL; the framed C-string parser owns that boundary.
pub fn chat_for_modes(bytes: &[u8], unicode_sender: bool) -> Option<(Vec<u8>, Vec<u8>)> {
    let text = &bytes[..bytes.len() - 1];
    if unicode_sender {
        let text = single_line(std::str::from_utf8(text).ok()?);
        let utf8 = if text.len() > CHAT_LIMIT {
            terminated(format!("{}...", limit_utf8(&text, CHAT_LIMIT - 3)).into_bytes())
        } else {
            terminated(text.as_bytes().to_vec())
        };
        let legacy = project_legacy(&text, CHAT_LIMIT, "[UTF-8 message]");
        Some((legacy, utf8))
    } else {
        let legacy = if text.len() > CHAT_LIMIT {
            let mut legacy = text[..CHAT_LIMIT - 3].to_vec();
            legacy.extend_from_slice(b"...");
            terminated(legacy)
        } else {
            bytes.to_vec()
        };
        let utf8 = cp866_to_utf8(&legacy);
        Some((legacy, utf8))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_preserves_legacy_and_translates_per_recipient() {
        let old = b"\x8f\xe0\xa8\xa2\xa5\xe2\0"; // Привет
        let (legacy, utf8) = chat_for_modes(old, false).unwrap();
        assert_eq!(&legacy, old);
        assert_eq!(utf8, "Привет\0".as_bytes());
        assert_eq!(chat_for_modes(&utf8, true).unwrap().0, old);
        let new = "日本語 é\0".as_bytes();
        let (legacy, utf8) = chat_for_modes(new, true).unwrap();
        assert_eq!(legacy, b"[UTF-8 message]\0");
        assert_eq!(&utf8, new);
    }

    #[test]
    fn clamps_at_scalar_boundary_and_rejects_malformed_utf8() {
        let text = format!("{}日bcdef\0", "a".repeat(CHAT_LIMIT - 5));
        let (_, utf8) = chat_for_modes(text.as_bytes(), true).unwrap();
        assert!(std::str::from_utf8(&utf8).unwrap().ends_with("...\0"));
        assert!(utf8.len() <= CHAT_LIMIT + 1);
        assert!(chat_for_modes(b"\xff\0", true).is_none());
    }
}
