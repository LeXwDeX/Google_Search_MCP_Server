//! Small JS-compatibility helpers shared across the crate.

use serde_json::Value;
use time::macros::format_description;
use time::OffsetDateTime;

/// `new Date().toISOString()` shape: UTC, exactly three fractional digits.
pub const ISO_FORMAT: &[time::format_description::FormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z");

/// Current epoch milliseconds (matches `Date.now()`).
pub fn now_ms() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp_nanos() as i64 / 1_000_000
}

/// Format epoch milliseconds as an ISO-8601 UTC timestamp.
pub fn format_iso_ms(ms: i64) -> String {
    match OffsetDateTime::from_unix_timestamp_nanos(ms as i128 * 1_000_000) {
        Ok(dt) => dt.format(ISO_FORMAT).unwrap_or_else(|_| String::new()),
        Err(_) => String::new(),
    }
}

/// Format the current time as an ISO-8601 UTC timestamp.
pub fn format_now_iso() -> String {
    OffsetDateTime::now_utc()
        .format(ISO_FORMAT)
        .unwrap_or_else(|_| String::new())
}

/// JS `typeof`-style JSON type name, used in zod-compatible error messages.
pub fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Name used when the value is absent (JS `undefined`).
pub const UNDEFINED_TYPE: &str = "undefined";

/// Largest integer JavaScript can represent exactly (`Number.MAX_SAFE_INTEGER`).
pub const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

/// Whether `n` is a JS safe integer: finite, integral, |n| <= 2^53-1.
pub fn is_safe_integer(n: f64) -> bool {
    n.is_finite() && n.fract() == 0.0 && n.abs() <= MAX_SAFE_INTEGER
}

/// ECMAScript `WhiteSpace` + `LineTerminator`, the set used by
/// `String.prototype.trim()` and `Number()`.
///
/// Notably this includes U+FEFF (BOM) and excludes U+0085 (NEL), which is the
/// opposite of Rust's `char::is_whitespace`.
pub fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'..='\u{000D}'
            | ' '
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
            | '\u{FEFF}'
    )
}

/// JS `String.prototype.trimStart()`.
pub fn js_trim_start(s: &str) -> &str {
    s.trim_start_matches(is_js_whitespace)
}

/// JS `String.prototype.trimEnd()`.
pub fn js_trim_end(s: &str) -> &str {
    s.trim_end_matches(is_js_whitespace)
}

/// JS `String.prototype.trim()`.
pub fn js_trim(s: &str) -> &str {
    js_trim_end(js_trim_start(s))
}

/// UTF-16 code-unit length, matching JS `String.prototype.length`.
pub fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// Take the first `n` UTF-16 code units, matching JS `String.prototype.slice`.
pub fn utf16_slice(s: &str, n: usize) -> String {
    let mut out = String::new();
    let mut units = 0;
    for ch in s.chars() {
        let len = ch.len_utf16();
        if units + len > n {
            break;
        }
        out.push(ch);
        units += len;
    }
    out
}

/// JavaScript `Number(string)` coercion for configuration values.
///
/// Handles the shapes that differ from Rust's `f64::from_str`: hex/octal/binary
/// literals, `Infinity`, and leading/trailing whitespace.
pub fn js_number(input: &str) -> f64 {
    let s = js_trim(input);
    if s.is_empty() {
        return 0.0;
    }
    // JS only accepts the exact spellings `Infinity`, `+Infinity`, `-Infinity`.
    match s {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    let (body, has_sign) = match s.as_bytes().first() {
        Some(b'+') => (&s[1..], true),
        Some(b'-') => (&s[1..], true),
        _ => (s, false),
    };
    let lower = body.to_ascii_lowercase();
    let radix = if let Some(rest) = lower.strip_prefix("0x") {
        Some((rest, 16))
    } else if let Some(rest) = lower.strip_prefix("0o") {
        Some((rest, 8))
    } else {
        lower.strip_prefix("0b").map(|rest| (rest, 2))
    };
    if let Some((digits, radix)) = radix {
        // Hex/octal/binary literals never take a sign in JS `Number()`.
        if has_sign || digits.is_empty() {
            return f64::NAN;
        }
        return match u128::from_str_radix(digits, radix) {
            Ok(v) => v as f64,
            Err(_) => f64::NAN,
        };
    }
    if !is_js_decimal(body) {
        return f64::NAN;
    }
    s.parse::<f64>().unwrap_or(f64::NAN)
}

/// Whether `s` is a JS decimal-number literal (no sign handling).
fn is_js_decimal(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut index = 0;
    let mut digits = 0;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        index += 1;
        digits += 1;
    }
    if index < bytes.len() && bytes[index] == b'.' {
        index += 1;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return false;
    }
    if index < bytes.len() && (bytes[index] == b'e' || bytes[index] == b'E') {
        index += 1;
        if index < bytes.len() && (bytes[index] == b'+' || bytes[index] == b'-') {
            index += 1;
        }
        let mut exponent_digits = 0;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
            exponent_digits += 1;
        }
        if exponent_digits == 0 {
            return false;
        }
    }
    index == bytes.len()
}

/// Timestamp at the start of the UTC day containing `ms`.
pub fn start_of_utc_day(ms: i64) -> i64 {
    ms - ms.rem_euclid(86_400_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn js_number_matches_common_shapes() {
        assert_eq!(js_number("1000"), 1000.0);
        assert_eq!(js_number(" 1000 "), 1000.0);
        assert_eq!(js_number("1e3"), 1000.0);
        assert_eq!(js_number("0x10"), 16.0);
        assert!(js_number("-0x10").is_nan());
        assert!(js_number("+0x10").is_nan());
        assert_eq!(js_number("0b101"), 5.0);
        assert_eq!(js_number("0o17"), 15.0);
        assert!(js_number("abc").is_nan());
        assert_eq!(js_number("Infinity"), f64::INFINITY);
        assert_eq!(js_number("+Infinity"), f64::INFINITY);
        assert_eq!(js_number("-Infinity"), f64::NEG_INFINITY);
        assert!(js_number("infinity").is_nan());
        assert!(js_number("inf").is_nan());
        assert!(js_number("NaN").is_nan());
        assert_eq!(js_number(".5"), 0.5);
        assert_eq!(js_number("1."), 1.0);
        assert_eq!(js_number(""), 0.0);
    }

    #[test]
    fn iso_format_has_millis_and_z() {
        let s = format_iso_ms(1_794_268_800_000);
        assert_eq!(s, "2026-11-10T00:00:00.000Z");
    }

    #[test]
    fn utf16_lengths_count_astral_pairs() {
        assert_eq!(utf16_len("abc"), 3);
        assert_eq!(utf16_len("😀"), 2);
        assert_eq!(utf16_slice("a😀b", 3), "a😀");
    }

    #[test]
    fn js_trim_matches_ecmascript_whitespace() {
        assert_eq!(js_trim("\u{FEFF}debug"), "debug");
        assert_eq!(js_trim("\u{0085}debug"), "\u{0085}debug");
        assert_eq!(js_trim(" \t\r\nx\u{00A0}\u{3000}"), "x");
        assert_eq!(js_number("\u{FEFF}1000"), 1000.0);
        assert!(js_number("\u{0085}1000").is_nan());
    }

    #[test]
    fn safe_integer_range() {
        assert!(is_safe_integer(9_007_199_254_740_991.0));
        assert!(!is_safe_integer(9_007_199_254_740_992.0));
        assert!(!is_safe_integer(f64::INFINITY));
        assert!(!is_safe_integer(1.5));
        assert!(!is_safe_integer(f64::NAN));
    }
}
