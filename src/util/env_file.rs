/// Parses a `.env`-style file's contents into "KEY=value" pairs — blank
/// lines and `#`-comments are skipped, and matching surrounding quotes on
/// the value are stripped (the common `.env` convention). No multi-line
/// values or `export` prefixes: real `.env` files in the wild are simple
/// enough that this covers the practical cases, same scope Compose's own
/// `env_file` support covers.
pub fn parse_env_file(contents: &str) -> Vec<String> {
    contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            let key = key.trim();
            let mut value = value.trim();
            if value.len() >= 2
                && ((value.starts_with('"') && value.ends_with('"'))
                    || (value.starts_with('\'') && value.ends_with('\'')))
            {
                value = &value[1..value.len() - 1];
            }
            Some(format!("{key}={value}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_env_file_skips_blanks_and_comments_and_strips_matching_quotes() {
        let contents =
            "# a comment\n\nFOO=bar\nBAZ=\"quoted value\"\nSINGLE='hi'\nMISMATCHED=\"oops'\n";
        let pairs = parse_env_file(contents);
        assert_eq!(
            pairs,
            vec![
                "FOO=bar",
                "BAZ=quoted value",
                "SINGLE=hi",
                "MISMATCHED=\"oops'",
            ]
        );
    }
}
