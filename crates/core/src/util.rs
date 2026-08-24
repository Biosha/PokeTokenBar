//! Defensive JSON number handling. Local logs come from outside the app (hand-edits,
//! transport corruption, upstream bugs) and stay on disk, so a bad value must degrade,
//! never crash — a crash would recur on every refresh until the user deletes the file.

use serde_json::Value;

/// Parsing cap (~1e15): 100k× real usage, yet small enough that summing many entries
/// stays well inside `i64` headroom (avoids the overflow traps the Swift version documented).
pub const MAX_PARSED_TOKEN: i64 = 1_000_000_000_000_000;

/// Finite number or `None`. `null` / string / non-number / non-finite ⇒ `None`.
pub fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
    .filter(|d| d.is_finite())
}

/// Number as `Option<i64>`, or `None` when not a finite number. `<= 0` collapses to
/// `Some(0)` (there are no negative tokens); values ≥ cap clamp to the cap.
pub fn int_or_nil(v: &Value) -> Option<i64> {
    let d = as_f64(v)?;
    if d <= 0.0 {
        return Some(0);
    }
    if d >= MAX_PARSED_TOKEN as f64 {
        return Some(MAX_PARSED_TOKEN);
    }
    Some(d as i64)
}

/// Number as `i64`, with absence/non-number collapsed to 0.
pub fn int_value(v: &Value) -> i64 {
    int_or_nil(v).unwrap_or(0)
}

/// Truthiness matching Foundation `NSNumber.boolValue` (`!= 0` for numbers).
pub fn bool_value(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|d| d != 0.0).unwrap_or(false),
        _ => false,
    }
}

/// Trimmed non-empty string, or `None`. Whitespace-only and `null` ⇒ `None`.
pub fn non_empty(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) => {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        }
        _ => None,
    }
}

/// `obj[key]` as `i64`, absent/non-number ⇒ 0.
pub fn get_int(obj: &Value, key: &str) -> i64 {
    obj.get(key).map(int_value).unwrap_or(0)
}

/// `obj[key]` as `Option<i64>` (clamped), `None` when absent/non-number.
pub fn get_int_opt(obj: &Value, key: &str) -> Option<i64> {
    obj.get(key).and_then(int_or_nil)
}

/// `obj[key]` as a borrowed string when present.
pub fn get_str<'a>(obj: &'a Value, key: &str) -> Option<&'a str> {
    obj.get(key).and_then(Value::as_str)
}

/// Trimmed non-empty string from an `Option<&str>`; whitespace-only / None ⇒ `None`.
pub fn non_empty_str(v: Option<&str>) -> Option<String> {
    v.map(str::trim).filter(|s| !s.is_empty()).map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn int_value_from_number() {
        assert_eq!(int_value(&json!(42)), 42);
        assert_eq!(int_value(&json!(3.7)), 3);
    }

    #[test]
    fn missing_or_null_is_zero() {
        assert_eq!(int_value(&Value::Null), 0);
        assert_eq!(int_value(&json!("nope")), 0);
    }

    #[test]
    fn negative_clamps_to_zero() {
        assert_eq!(int_or_nil(&json!(-5)), Some(0));
        assert_eq!(int_value(&json!(-5)), 0);
    }

    #[test]
    fn huge_clamps_to_cap() {
        assert_eq!(int_value(&json!(1e30)), MAX_PARSED_TOKEN);
    }

    #[test]
    fn bool_from_number() {
        assert!(bool_value(&json!(1)));
        assert!(!bool_value(&json!(0)));
        assert!(bool_value(&json!(true)));
        assert!(!bool_value(&json!(false)));
    }

    #[test]
    fn non_empty_trims() {
        assert_eq!(non_empty(Some(&json!("  x "))), Some("x".into()));
        assert_eq!(non_empty(Some(&json!("   "))), None);
        assert_eq!(non_empty(None), None);
    }
}
