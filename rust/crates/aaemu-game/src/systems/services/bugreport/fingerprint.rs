//! Fingerprint normalisation and FNV-1a hashing.
//!
//! Two occurrences of the same bug produce the same fingerprint even when the object ids,
//! coordinates or counts differ: every run of digits in the message becomes `#`.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use regex::Regex;

use super::{Severity, Trigger};

/// FNV-1a/64 (stable across runs and platforms; not a cryptographic hash).
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x100000001b3)
    })
}

/// Replaces runs of ASCII digits with `#`, collapses whitespace and trims.
pub fn normalize_message(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    let mut in_digits = false;
    for ch in message.chars() {
        if ch.is_ascii_digit() {
            if !in_digits {
                out.push('#');
                in_digits = true;
            }
        } else {
            in_digits = false;
            out.push(ch);
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `hash(kind, subject, normalised message, opcode/location)` as 16 lowercase hex digits.
///
/// `/bug` reports are grouped per character (`actor`) and ignore the chat text, so one player
/// cannot create a new fingerprint per message.
pub fn fingerprint(trigger: &Trigger, actor: Option<u32>) -> String {
    let command = matches!(trigger, Trigger::Command { .. });
    let mut basis = String::new();
    basis.push_str(trigger.kind());
    basis.push('|');
    basis.push_str(trigger.subject());
    if command {
        if let Some(actor) = actor {
            basis.push_str("|actor:");
            basis.push_str(&actor.to_string());
        }
    }
    basis.push('|');
    let message = if command {
        "chat command".to_string()
    } else {
        normalize_message(trigger.message())
    };
    basis.push_str(&message);
    if let Some(opcode) = trigger.opcode() {
        basis.push_str("|opcode:");
        basis.push_str(opcode);
    }
    format!("{:016x}", fnv1a64(basis.as_bytes()))
}

/// Classifies a severity into the summary's maximum ("error" > "warn" > "info").
pub fn max_severity(a: Severity, b: Severity) -> Severity {
    match (a, b) {
        (Severity::Error, _) | (_, Severity::Error) => Severity::Error,
        (Severity::Warn, _) | (_, Severity::Warn) => Severity::Warn,
        _ => Severity::Info,
    }
}

fn message_ids_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)\b(?:obj(?:ect)?(?:[ _-]?id)?|character(?:[ _-]?id)?|char(?:[ _-]?id)?|unit(?:[ _-]?id)?|slave(?:[ _-]?id)?|npc(?:[ _-]?id)?|doodad(?:[ _-]?id)?)\s*(?:id)?\s*(?:=|:|#)?\s*(0x[0-9a-f]{1,8}|[0-9]{1,10})\b",
        )
        .expect("id regex")
    })
}

/// Object ids mentioned as `unit 1234`, `obj_id=1234`, `character: 0x1f`, ... .
pub fn ids_in_message(message: &str) -> Vec<u32> {
    let mut ids = Vec::new();
    for caps in message_ids_re().captures_iter(message) {
        if let Some(id) = parse_id(&caps[1]) {
            if id != 0 && !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    ids
}

/// An id carried by a structured field (`obj_id`, `character_id`, `unit_id`, ...).
pub fn id_from_field(name: &str, value: &str) -> Option<u32> {
    let key = name.to_ascii_lowercase();
    let prefixes = [
        "obj",
        "object",
        "character",
        "char",
        "unit",
        "slave",
        "npc",
        "doodad",
    ];
    let relevant = prefixes
        .iter()
        .any(|prefix| key.starts_with(prefix) && (key.ends_with("id") || key.ends_with("_id")));
    if !relevant {
        return None;
    }
    parse_id(value.trim())
}

fn parse_id(value: &str) -> Option<u32> {
    if let Some(hex) = value.strip_prefix("0x") {
        u32::from_str_radix(hex, 16).ok()
    } else {
        value.parse().ok()
    }
}

/// `BTreeMap` lookup helper for trigger fields.
pub fn field<'a>(fields: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    fields.get(key).map(String::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(message: &str) -> Trigger {
        Trigger::Log {
            level: "WARN".into(),
            target: "aaemu_game::systems::skills".into(),
            message: message.into(),
            fields: BTreeMap::new(),
        }
    }

    #[test]
    fn numbers_do_not_change_the_fingerprint() {
        let a = fingerprint(&log("unit 1234 has hp -5 (max 100)"), None);
        let b = fingerprint(&log("unit 9876 has hp -42 (max 200)"), None);
        assert_eq!(a, b);
        let c = fingerprint(&log("unit 9876 has mp -42 (max 200)"), None);
        assert_ne!(a, c);
    }

    #[test]
    fn kind_target_and_opcode_are_part_of_the_fingerprint() {
        let base = fingerprint(&log("unit 1 failed"), None);
        let mut other_target = log("unit 1 failed");
        if let Trigger::Log { target, .. } = &mut other_target {
            *target = "aaemu_game::systems::economy".into();
        }
        assert_ne!(base, fingerprint(&other_target, None));

        let mut with_opcode = log("unit 1 failed");
        if let Trigger::Log { fields, .. } = &mut with_opcode {
            fields.insert("opcode".into(), "0x123".into());
        }
        assert_ne!(base, fingerprint(&with_opcode, None));
        assert_ne!(
            base,
            fingerprint(
                &Trigger::Command {
                    text: "unit 1 failed".into()
                },
                None
            )
        );
    }

    #[test]
    fn command_fingerprints_are_per_character_and_ignore_the_text() {
        let command = |text: &str| Trigger::Command { text: text.into() };
        assert_eq!(
            fingerprint(&command("first text"), Some(7)),
            fingerprint(&command("another text"), Some(7))
        );
        assert_ne!(
            fingerprint(&command("same text"), Some(7)),
            fingerprint(&command("same text"), Some(8))
        );
        assert_ne!(
            fingerprint(&command("text"), Some(7)),
            fingerprint(&command("text"), None)
        );
    }

    #[test]
    fn extracts_ids_from_messages_and_fields() {
        assert_eq!(ids_in_message("unit 1234 exploded"), [1234]);
        assert_eq!(ids_in_message("obj_id=77, char 0x1f"), [77, 31]);
        assert_eq!(ids_in_message("nothing here"), Vec::<u32>::new());
        assert_eq!(id_from_field("obj_id", "42"), Some(42));
        assert_eq!(id_from_field("character_id", "0x10"), Some(16));
        assert_eq!(id_from_field("template_id", "42"), None);
        assert_eq!(id_from_field("opcode", "42"), None);
        assert_eq!(normalize_message("a  1\tb 22"), "a # b #");
        assert_eq!(max_severity(Severity::Info, Severity::Warn), Severity::Warn);
        assert_eq!(
            max_severity(Severity::Warn, Severity::Error),
            Severity::Error
        );
    }
}
