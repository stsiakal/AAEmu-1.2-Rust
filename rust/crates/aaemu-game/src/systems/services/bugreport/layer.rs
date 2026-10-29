//! The WARN/ERROR (and log-ring INFO) tracing layer.
//!
//! Installed through `aaemu_commons::logging::init_with` so it sees exactly the events of the
//! console / file targets. There is deliberately no `enabled` override: in tracing-subscriber
//! 0.3 `Layer::enabled` is a *global* filter (the composed layer ANDs them), so returning false
//! here would silence every other target too. The layer gates itself in `on_event` instead.
//!
//! INFO / WARN / ERROR lines are appended to the bounded [`LogRing`] directly; only WARN /
//! ERROR events also queue a report on the bounded reporter channel, so an INFO flood cannot
//! drop error reports.

use std::collections::BTreeMap;
use std::fmt;

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::Layer;

use super::fingerprint;
use super::log_ring::LogRing;
use super::reporter::rfc3339;
use super::snapshot::LogLine;

/// The capture layer (stateless in production; the reporter is looked up at event time).
#[derive(Debug, Default, Clone)]
pub struct BugReportLayer {
    /// `None` = consult the installed reporter; `Some` = tests.
    forced: Option<bool>,
    /// Tests inject the ring; production uses the installed reporter's ring.
    #[cfg(test)]
    forced_log: Option<std::sync::Arc<LogRing>>,
}

impl BugReportLayer {
    pub fn new() -> Self {
        Self {
            forced: None,
            #[cfg(test)]
            forced_log: None,
        }
    }

    fn active(&self) -> bool {
        self.forced.unwrap_or_else(super::enabled)
    }

    fn log_ring(&self) -> Option<std::sync::Arc<LogRing>> {
        #[cfg(test)]
        if self.forced.is_some() {
            return self.forced_log.clone();
        }
        super::shared().map(|s| s.log_ring.clone())
    }

    /// A layer with a fixed active state, for tests that must not depend on process globals.
    #[cfg(test)]
    pub(crate) fn for_test(active: bool) -> Self {
        Self {
            forced: Some(active),
            forced_log: None,
        }
    }

    /// A layer writing to an explicit ring (tests).
    #[cfg(test)]
    pub(crate) fn for_test_with_ring(active: bool, ring: std::sync::Arc<LogRing>) -> Self {
        Self {
            forced: Some(active),
            forced_log: Some(ring),
        }
    }
}

impl<S: Subscriber> Layer<S> for BugReportLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        if !self.active() || super::in_writer_thread() || super::in_panic_hook() {
            return;
        }
        let meta = event.metadata();
        // Per-layer INFO-and-above filter. This is deliberately here and not in `Layer::enabled`
        // (a global filter); the cheap level check runs before any formatting, so packet TRACE
        // lines (`aaemu_game::net=trace`) cost one comparison.
        if *meta.level() > Level::INFO {
            return;
        }
        let Some(log_ring) = self.log_ring() else {
            return;
        };
        let mut visitor = EventVisitor::default();
        event.record(&mut visitor);
        let level = meta.level().as_str().to_string();
        let message = visitor.message;
        let timestamp_ms = chrono::Utc::now().timestamp_millis();
        let line = LogLine {
            timestamp: rfc3339(timestamp_ms),
            timestamp_ms,
            level,
            target: meta.target().to_string(),
            message,
            fields: visitor.fields,
        };
        let report = *meta.level() <= Level::WARN;
        let ids = if report {
            extract_ids(&line.message, &line.fields)
        } else {
            Vec::new()
        };
        log_ring.push(line.clone());
        // Forced layers are test-only and must not touch the process-global reporter.
        if report && self.forced.is_none() {
            let _ = super::report_observed(
                super::Trigger::Log {
                    level: line.level,
                    target: line.target,
                    message: line.message,
                    fields: line.fields,
                },
                ids,
                serde_json::Value::Null,
            );
        }
    }
}

fn extract_ids(message: &str, fields: &BTreeMap<String, String>) -> Vec<u32> {
    let mut ids = fingerprint::ids_in_message(message);
    for (name, value) in fields {
        if let Some(id) = fingerprint::id_from_field(name, value) {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    ids
}

const MAX_FIELDS: usize = 20;
const MAX_VALUE_CHARS: usize = 300;

#[derive(Default)]
struct EventVisitor {
    message: String,
    fields: BTreeMap<String, String>,
}

impl EventVisitor {
    fn record(&mut self, field: &Field, value: String) {
        if field.name() == "message" {
            self.message = value;
        } else if self.fields.len() < MAX_FIELDS {
            self.fields
                .insert(field.name().to_string(), truncate(value, MAX_VALUE_CHARS));
        }
    }
}

fn truncate(text: String, max: usize) -> String {
    if text.chars().count() <= max {
        return text;
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

fn unquote(text: &str) -> String {
    text.strip_prefix('"')
        .and_then(|t| t.strip_suffix('"'))
        .unwrap_or(text)
        .to_string()
}

impl Visit for EventVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.record(field, value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        let text = format!("{value:?}");
        let text = if field.name() == "message" {
            unquote(&text)
        } else {
            text
        };
        self.record(field, text);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use tracing_subscriber::layer::SubscriberExt;

    use super::*;

    #[derive(Clone)]
    struct Counting(Arc<AtomicUsize>);

    impl<S: Subscriber> Layer<S> for Counting {
        fn on_event(&self, _event: &Event<'_>, _ctx: Context<'_, S>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn count_events(layer: BugReportLayer) -> usize {
        let seen = Arc::new(AtomicUsize::new(0));
        let subscriber = tracing_subscriber::registry()
            .with(layer)
            .with(Counting(seen.clone()));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("info");
            tracing::warn!("warn");
            tracing::error!("error");
            tracing::debug!("debug");
        });
        seen.load(Ordering::Relaxed)
    }

    #[test]
    fn disabled_layer_does_not_filter_other_layers() {
        assert_eq!(count_events(BugReportLayer::for_test(false)), 4);
    }

    #[test]
    fn enabled_layer_does_not_filter_other_layers() {
        assert_eq!(count_events(BugReportLayer::for_test(true)), 4);
    }

    #[test]
    fn enabled_layer_captures_info_and_above_only() {
        let seen = Arc::new(AtomicUsize::new(0));
        let ring = Arc::new(LogRing::new(16));
        let subscriber = tracing_subscriber::registry()
            .with(BugReportLayer::for_test_with_ring(true, ring.clone()))
            .with(Counting(seen.clone()));
        tracing::subscriber::with_default(subscriber, || {
            tracing::trace!("trace line");
            tracing::debug!("debug line");
            tracing::info!("info line");
            tracing::warn!("warn line");
        });
        assert_eq!(
            seen.load(Ordering::Relaxed),
            4,
            "other layer sees TRACE too"
        );
        let messages: Vec<String> = ring
            .snapshot()
            .into_iter()
            .map(|line| line.message)
            .collect();
        assert!(messages.iter().any(|m| m == "info line"), "{messages:?}");
        assert!(messages.iter().any(|m| m == "warn line"), "{messages:?}");
        assert!(
            !messages
                .iter()
                .any(|m| m == "trace line" || m == "debug line"),
            "{messages:?}"
        );
    }

    #[test]
    fn extracts_ids_from_fields_before_the_message() {
        let mut fields = BTreeMap::new();
        fields.insert("obj_id".into(), "77".into());
        assert_eq!(extract_ids("unit 42 died", &fields), [42, 77]);
        assert_eq!(extract_ids("no ids", &BTreeMap::new()), Vec::<u32>::new());
        assert_eq!(unquote("\"hello\""), "hello");
        assert_eq!(unquote("hello"), "hello");
    }
}
