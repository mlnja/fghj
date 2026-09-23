/// Looks up `key` in a `k=v&k=v` query string. No percent-decoding — the
/// only values passed through this today (workspace ids) never need it.
pub fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then_some(v)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_param_finds_a_key_anywhere_in_the_string() {
        assert_eq!(query_param("workspace=abc", "workspace"), Some("abc"));
        assert_eq!(
            query_param("a=1&workspace=abc&b=2", "workspace"),
            Some("abc")
        );
        assert_eq!(query_param("a=1&b=2", "workspace"), None);
        assert_eq!(query_param("", "workspace"), None);
        assert_eq!(query_param("workspace", "workspace"), None);
    }
}
