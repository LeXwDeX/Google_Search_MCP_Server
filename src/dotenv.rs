//! A `.env` loader matching `dotenv@16.5.0` (the package the TypeScript build
//! used), including its quirks:
//!
//!  - `KEY=VALUE` and `KEY: VALUE` separators, optional `export` prefix
//!  - single/double/backtick quotes, including multiline quoted values
//!  - unquoted values stop at the first `#` (`1000#comment` ⇒ `1000`)
//!  - no `${VAR}` expansion (`${VAR}` is stored literally)
//!  - invalid lines are skipped, duplicate keys keep the last value
//!  - variables already present in the process environment are not overridden
//!
//! Only the working-directory `.env` is read.

use crate::util::{is_js_whitespace, js_trim, js_trim_start};

/// Load `.env` (if present) and apply it without overriding existing variables.
pub fn load_and_apply() {
    let Ok(bytes) = std::fs::read(".env") else {
        return;
    };
    let source = String::from_utf8_lossy(&bytes);
    for (key, value) in parse(&source) {
        if std::env::var_os(&key).is_some() {
            continue;
        }
        if key.contains('\0') || value.contains('\0') {
            continue;
        }
        std::env::set_var(key, value);
    }
}

/// Parse dotenv-style text into ordered key/value pairs (last duplicate wins).
pub fn parse(source: &str) -> Vec<(String, String)> {
    let normalized = source.replace("\r\n", "\n").replace('\r', "\n");
    let lines: Vec<&str> = normalized.split('\n').collect();
    let mut out: Vec<(String, String)> = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        match parse_entry(&lines, index) {
            Some((key, value, consumed)) => {
                if let Some(existing) = out.iter_mut().find(|(existing, _)| *existing == key) {
                    existing.1 = value;
                } else {
                    out.push((key, value));
                }
                index += consumed;
            }
            None => index += 1,
        }
    }
    out
}

fn parse_entry(lines: &[&str], start: usize) -> Option<(String, String, usize)> {
    let line = lines[start];
    let mut rest = js_trim_start(line);

    if let Some(after_export) = rest.strip_prefix("export") {
        if after_export
            .chars()
            .next()
            .map(is_js_whitespace)
            .unwrap_or(false)
        {
            rest = js_trim_start(after_export);
        }
    }

    let mut key_len = 0;
    for (offset, ch) in rest.char_indices() {
        let valid = ch.is_ascii_alphanumeric() || ch == '_' || ch == '.' || ch == '-';
        if valid {
            key_len = offset + ch.len_utf8();
        } else {
            break;
        }
    }
    if key_len == 0 {
        return None;
    }
    let key = rest[..key_len].to_string();
    let mut rest = js_trim_start(&rest[key_len..]);

    if let Some(after_eq) = rest.strip_prefix('=') {
        rest = js_trim_start(after_eq);
    } else {
        let after_colon = rest.strip_prefix(':')?;
        // dotenv requires whitespace after the colon.
        if !after_colon
            .chars()
            .next()
            .map(is_js_whitespace)
            .unwrap_or(false)
        {
            return None;
        }
        rest = js_trim_start(after_colon);
    }

    let Some(first) = rest.chars().next() else {
        return Some((key, String::new(), 1));
    };
    if matches!(first, '\'' | '"' | '`') {
        // Quoted values take precedence, but the regex engine falls back to
        // the unquoted alternative when trailing junk makes the quoted form
        // fail its end-of-line anchor.
        if let Some(parsed) = parse_quoted(lines, start, rest, first, key.clone()) {
            return Some(parsed);
        }
    }

    let value = rest.split('#').next().unwrap_or("");
    Some((key, js_trim(value).to_string(), 1))
}

fn parse_quoted(
    lines: &[&str],
    start: usize,
    rest: &str,
    quote: char,
    key: String,
) -> Option<(String, String, usize)> {
    let content = &rest[quote.len_utf8()..];
    let mut combined = String::from(content);
    for line in &lines[start + 1..] {
        combined.push('\n');
        combined.push_str(line);
    }

    let mut value = String::new();
    let mut newlines = 0usize;
    let mut previous_backslash = false;
    let mut closing = None;

    for (index, ch) in combined.char_indices() {
        if ch == quote && !previous_backslash {
            closing = Some(index + ch.len_utf8());
            break;
        }
        if ch == '\n' {
            newlines += 1;
        }
        value.push(ch);
        previous_backslash = ch == '\\';
    }

    let after = closing?;
    let remainder = &combined[after..];
    let rest_of_line = remainder.split('\n').next().unwrap_or("");
    let trimmed = js_trim_start(rest_of_line);
    if !trimmed.is_empty() && !trimmed.starts_with('#') {
        return None;
    }

    let value = if quote == '"' {
        value.replace("\\n", "\n").replace("\\r", "\r")
    } else {
        value
    };
    Some((key, value, newlines + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(source: &str) -> Vec<(String, String)> {
        parse(source)
    }

    #[test]
    fn basic_forms() {
        let vars =
            parsed("GOOGLE_API_KEY=k\nGOOGLE_CSE_ID=c\nGOOGLE_SEARCH_TIMEOUT_MS=1000#comment\n");
        assert_eq!(
            vars,
            vec![
                ("GOOGLE_API_KEY".into(), "k".into()),
                ("GOOGLE_CSE_ID".into(), "c".into()),
                ("GOOGLE_SEARCH_TIMEOUT_MS".into(), "1000".into()),
            ]
        );
        let vars = parsed("GOOGLE_API_KEY: k\nGOOGLE_CSE_ID: c\n");
        assert_eq!(vars[0].1, "k");
        let vars = parsed("export GOOGLE_API_KEY=k\n");
        assert_eq!(vars[0], ("GOOGLE_API_KEY".into(), "k".into()));
        let vars = parsed("\u{FEFF}GOOGLE_API_KEY=k\n");
        assert_eq!(vars[0].1, "k");
    }

    #[test]
    fn duplicate_last_wins() {
        let vars = parsed("A=1\nA=2\n");
        assert_eq!(vars, vec![("A".into(), "2".into())]);
    }

    #[test]
    fn invalid_lines_are_skipped() {
        let vars = parsed("this is not valid\nGOOGLE_API_KEY=k\n");
        assert_eq!(vars, vec![("GOOGLE_API_KEY".into(), "k".into())]);
    }

    #[test]
    fn no_expansion() {
        let vars = parsed("A=${UNSET}\nB=ab$cd\n");
        assert_eq!(vars[0].1, "${UNSET}");
        assert_eq!(vars[1].1, "ab$cd");
    }

    #[test]
    fn quotes_and_multiline() {
        let vars = parsed("A='a b'\nB=`c d`\nC=\"line1\nline2\"\nD=\"a\\\"b\"\n");
        assert_eq!(vars[0].1, "a b");
        assert_eq!(vars[1].1, "c d");
        assert_eq!(vars[2].1, "line1\nline2");
        assert_eq!(vars[3].1, "a\\\"b");
    }

    #[test]
    fn comments_and_line_endings() {
        let vars = parsed("# hi\r\n\r\nA=1 # comment\r\nB=2\r\n");
        assert_eq!(
            vars,
            vec![("A".into(), "1".into()), ("B".into(), "2".into())]
        );
    }
}
