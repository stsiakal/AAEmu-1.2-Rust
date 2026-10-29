//! Redaction of credentials in report payloads (details, messages, packet bodies, log lines).

use std::sync::OnceLock;

use regex::Regex;
use serde_json::{Map, Value};

/// Keys whose values are never written to a report (case-insensitive substring match).
const SECRET_PARTS: &[&str] = &[
    "password",
    "passwd",
    "secret",
    "token",
    "ticket",
    "apikey",
    "api_key",
    "authorization",
];

/// Exact key names that are redacted (`key` alone is too common).
const SECRET_KEYS: &[&str] = &[
    "key",
    "sessionkey",
    "session_key",
    "secretkey",
    "secret_key",
];

fn key_is_secret(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    SECRET_KEYS.contains(&lower.as_str()) || SECRET_PARTS.iter().any(|part| lower.contains(part))
}

fn assignment_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)\b(password|passwd|secret|token|ticket|api[_-]?key|secret[_-]?key|authorization)\b(\s*[=:]\s*|\s+)([^\s,;]+)",
        )
        .expect("redaction regex")
    })
}

/// `password=hunter2` -> `password=[redacted]` (also `token: abc`, `api_key abc`, ...).
pub fn redact_text(text: &str) -> String {
    assignment_re()
        .replace_all(text, "$1$2[redacted]")
        .into_owned()
}

/// Recursively redacts secret-looking object keys and assignment-like strings.
pub fn redact_value(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| {
                    if key_is_secret(&k) {
                        (k, Value::String("[redacted]".into()))
                    } else {
                        (k, redact_value(v))
                    }
                })
                .collect::<Map<String, Value>>(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(redact_value).collect()),
        Value::String(s) => Value::String(redact_text(&s)),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn redacts_assignments_and_secret_keys() {
        let value = json!({
            "account": "bob",
            "Password": "hunter2",
            "nested": {"api_key": "abc", "note": "token=deadbeef keep"},
            "list": ["ticket: T123", "plain"],
            "secretkey": 42
        });
        let redacted = redact_value(value);
        let text = serde_json::to_string(&redacted).unwrap();
        assert!(!text.contains("hunter2"));
        assert!(!text.contains("deadbeef"));
        assert!(!text.contains("T123"));
        assert!(!text.contains("\"abc\""));
        assert!(!text.contains("42"));
        assert!(text.contains("bob"));
        assert!(text.contains("plain"));
        assert_eq!(redact_text("password=abc"), "password=[redacted]");
    }
}
