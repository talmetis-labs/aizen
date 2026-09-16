//! Tiny `key=value` parsing.

/// Parse one `key=value` pair. Whitespace around the key and the value is trimmed. Returns
/// `None` when there is no `=` or the key is empty.
pub fn parse_kv(s: &str) -> Option<(&str, &str)> {
    let (k, v) = s.split_once('=')?;
    let k = k.trim();
    if k.is_empty() {
        return None;
    }
    Some((k, v.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_pair() {
        assert_eq!(parse_kv(" a = 1 "), Some(("a", "1")));
    }

    #[test]
    fn rejects_missing_equals_and_empty_key() {
        assert_eq!(parse_kv("a"), None);
        assert_eq!(parse_kv("=1"), None);
    }
}
