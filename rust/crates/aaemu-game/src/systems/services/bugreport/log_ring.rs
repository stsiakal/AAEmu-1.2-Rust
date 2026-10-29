//! Bounded ring of recent log lines, written by the tracing layer without a channel.
//!
//! Logging is far more frequent than reports, so INFO lines never enter the report queue: the
//! layer appends them here (try-lock, dropped on contention) and only WARN / ERROR events also
//! queue a report.

use std::collections::VecDeque;
use std::sync::Mutex;

use super::snapshot::LogLine;

#[derive(Debug)]
pub struct LogRing {
    capacity: usize,
    lines: Mutex<VecDeque<LogLine>>,
}

impl LogRing {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            lines: Mutex::new(VecDeque::with_capacity(capacity.min(256))),
        }
    }

    /// Appends a line, dropping the oldest when full. A contended ring drops the line instead
    /// of blocking the logging thread.
    pub fn push(&self, line: LogLine) {
        if self.capacity == 0 {
            return;
        }
        let Ok(mut lines) = self.lines.try_lock() else {
            return;
        };
        if lines.len() >= self.capacity {
            lines.pop_front();
        }
        lines.push_back(line);
    }

    pub fn snapshot(&self) -> Vec<LogLine> {
        self.lines
            .lock()
            .map(|l| l.iter().cloned().collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn line(message: &str) -> LogLine {
        LogLine {
            timestamp: String::new(),
            timestamp_ms: 0,
            level: "INFO".into(),
            target: "test".into(),
            message: message.into(),
            fields: BTreeMap::new(),
        }
    }

    #[test]
    fn keeps_only_the_last_lines() {
        let ring = LogRing::new(2);
        ring.push(line("one"));
        ring.push(line("two"));
        ring.push(line("three"));
        let lines = ring.snapshot();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1].message, "three");
        let none = LogRing::new(0);
        none.push(line("ignored"));
        assert!(none.snapshot().is_empty());
    }
}
