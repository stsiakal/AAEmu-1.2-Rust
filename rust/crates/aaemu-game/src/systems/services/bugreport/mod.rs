//! Automated bug reports (`aaemu-bugreport/1`).
//!
//! Triggers: the WARN/ERROR tracing layer, the panic hook, the `/bug` player command, the
//! slow invariant timer (HP, under the world, missing parents, ship position jumps) and the
//! public [`report`] / [`outcome`] APIs used by bots and scenarios.
//!
//! Reports are deduplicated by [`fingerprint`] and written under
//! `<Directory>/<run>/<fingerprint>-<n>.json`; every run also gets a `summary.json` with the
//! per-fingerprint counters and the `systems` dashboard rows (see `README.md`).
//!
//! Nothing heavy runs on the packet / tick hot paths: the tracing layer only formats the event
//! and `try_send`s it, the packet rings are fixed-size and allocated only when enabled, and the
//! report writer (a dedicated thread) performs every world lookup, snapshot and file write.

pub mod command;
pub mod config;
pub mod fingerprint;
pub mod invariants;
pub mod layer;
pub(crate) mod log_ring;
pub(crate) mod packet_ring;
pub(crate) mod panic_hook;
pub mod redact;
pub mod reporter;
pub mod snapshot;

#[cfg(test)]
mod flow_tests;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

use crate::server::GameServer;

pub use config::BugReportsConfig;
pub use layer::BugReportLayer;
pub use reporter::ReporterConfig;

/// Report format written to every `*.json` file and to `summary.json`.
pub const FORMAT: &str = "aaemu-bugreport/1";

/// Build identity: a release build may set `AAEMU_GIT_DESCRIBE`, otherwise the crate version.
pub const BUILD: &str = match option_env!("AAEMU_GIT_DESCRIBE") {
    Some(describe) => describe,
    None => env!("CARGO_PKG_VERSION"),
};

/// How bad a trigger is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warn,
    Error,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warn => "warn",
            Severity::Error => "error",
        }
    }
}

/// Outcome of one `{system, action}` attempt (the dashboard row).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    Fail,
    Refused,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Fail => "fail",
            Outcome::Refused => "refused",
        }
    }
}

/// What produced a report.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Trigger {
    /// A `tracing` WARN or ERROR event.
    Log {
        level: String,
        target: String,
        message: String,
        #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
        fields: std::collections::BTreeMap<String, String>,
    },
    /// A panic (the default hook output is kept).
    Panic {
        location: String,
        message: String,
        thread: String,
    },
    /// The `/bug <text>` player command.
    Command { text: String },
    /// A failed invariant check.
    Invariant { check: String, message: String },
    /// Another subsystem, a scenario or a bot.
    External {
        subsystem: String,
        message: String,
        severity: Severity,
    },
}

impl Trigger {
    /// `Trigger::External` for scenarios / bots.
    pub fn external(
        sub: impl Into<String>,
        message: impl Into<String>,
        severity: Severity,
    ) -> Self {
        Self::External {
            subsystem: sub.into(),
            message: message.into(),
            severity,
        }
    }

    /// `Trigger::Invariant` for the slow checker.
    pub fn invariant(check: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Invariant {
            check: check.into(),
            message: message.into(),
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Trigger::Log { .. } => "log",
            Trigger::Panic { .. } => "panic",
            Trigger::Command { .. } => "command",
            Trigger::Invariant { .. } => "invariant",
            Trigger::External { .. } => "external",
        }
    }

    /// Subsystem / target / check / panic location (part of the fingerprint).
    pub fn subject(&self) -> &str {
        match self {
            Trigger::Log { target, .. } => target,
            Trigger::Panic { location, .. } => location,
            Trigger::Command { .. } => "command",
            Trigger::Invariant { check, .. } => check,
            Trigger::External { subsystem, .. } => subsystem,
        }
    }

    pub fn message(&self) -> &str {
        match self {
            Trigger::Log { message, .. } => message,
            Trigger::Panic { message, .. } => message,
            Trigger::Command { text } => text,
            Trigger::Invariant { message, .. } => message,
            Trigger::External { message, .. } => message,
        }
    }

    pub fn severity(&self) -> Severity {
        match self {
            Trigger::Log { level, .. } => {
                if level.eq_ignore_ascii_case("ERROR") {
                    Severity::Error
                } else {
                    Severity::Warn
                }
            }
            Trigger::Panic { .. } => Severity::Error,
            Trigger::Command { .. } => Severity::Info,
            Trigger::Invariant { .. } => Severity::Error,
            Trigger::External { severity, .. } => *severity,
        }
    }

    /// `opcode` / `opcode_id` field of a log event, when present.
    pub fn opcode(&self) -> Option<&str> {
        let Trigger::Log { fields, .. } = self else {
            return None;
        };
        fields
            .get("opcode")
            .or_else(|| fields.get("opcodeid"))
            .or_else(|| fields.get("opcode_id"))
            .map(String::as_str)
    }
}

/// The installed reporter (only when `BugReports.Enabled`).
pub(crate) struct Shared {
    reporter: reporter::Reporter,
    log_ring: Arc<log_ring::LogRing>,
    server: Option<Weak<GameServer>>,
    enabled: AtomicBool,
}

static SHARED: OnceLock<Shared> = OnceLock::new();

thread_local! {
    /// Set on the report writer thread (its own log lines must not trigger reports).
    static WRITER_THREAD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Set while the previous panic hook runs (its `error!` must not duplicate the panic report).
    static PANIC_HOOK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(crate) fn in_writer_thread() -> bool {
    WRITER_THREAD.with(std::cell::Cell::get)
}

pub(crate) fn in_panic_hook() -> bool {
    PANIC_HOOK.with(std::cell::Cell::get)
}

pub(crate) fn mark_writer_thread() {
    WRITER_THREAD.with(|v| v.set(true));
}

pub(crate) fn shared() -> Option<&'static Shared> {
    SHARED.get()
}

pub(crate) fn enabled() -> bool {
    SHARED
        .get()
        .is_some_and(|s| s.enabled.load(Ordering::Relaxed))
}

/// Installs the reporter, panic hook, invariant timer and the `/bug` command. Idempotent.
pub fn install(server: &Arc<GameServer>, config: &BugReportsConfig) -> anyhow::Result<()> {
    if SHARED.get().is_some() {
        return Ok(());
    }
    if !config.enabled {
        return Ok(());
    }
    packet_ring::set_capacity(config.packet_ring);
    let reporter_config = ReporterConfig {
        directory: config.resolve_directory(&server.config.app_path),
        max_full_reports_per_fingerprint: config.max_full_reports_per_fingerprint,
        nearby_radius: config.nearby_radius,
        log_ring: config.log_ring,
        rate_per_second: config.rate_per_second,
        max_fingerprints: config.max_fingerprints,
        max_full_reports: config.max_full_reports,
        max_total_bytes: config.max_total_bytes,
    };
    let reporter = reporter::Reporter::start(reporter_config, Some(Arc::downgrade(server)))?;
    let log_ring = reporter.log_ring();
    let shared = Shared {
        reporter,
        log_ring,
        server: Some(Arc::downgrade(server)),
        enabled: AtomicBool::new(true),
    };
    if SHARED.set(shared).is_err() {
        return Ok(());
    }
    panic_hook::install();
    // The flow tests drive `check_now` directly; the timer would race their counters.
    #[cfg(not(test))]
    invariants::start_task(server);
    command::register(&server.systems.services.commands);
    tracing::info!(
        "Bug reports enabled (run directory {})",
        SHARED
            .get()
            .map(|s| s.reporter.run_directory().display().to_string())
            .unwrap_or_default()
    );
    Ok(())
}

/// [`install`] from the subsystems' startup (`services::System::start`); reads the config.
pub fn ensure_started(server: &Arc<GameServer>) -> anyhow::Result<()> {
    let config = server.config.bug_reports.clone();
    install(server, &config)
}

/// Queues a report. Returns `false` when bug reports are disabled.
///
/// `actor` / `target` are object ids. The heavy part (world snapshots, packet/log rings, file
/// write) runs on the report writer, never on the caller.
pub fn report(
    server: &GameServer,
    trigger: Trigger,
    actor: Option<u32>,
    target: Option<u32>,
    details: Value,
) -> bool {
    let Some(shared) = SHARED.get() else {
        return false;
    };
    if !shared.enabled.load(Ordering::Relaxed) {
        return false;
    }
    let server = server
        .self_arc()
        .map(|s| Arc::downgrade(&s))
        .or_else(|| shared.server.clone());
    shared
        .reporter
        .submit(trigger, actor, target, details, Vec::new(), server)
}

/// Queues a report with ids observed in a log message (the tracing layer path).
pub(crate) fn report_observed(trigger: Trigger, observed: Vec<u32>, details: Value) -> bool {
    let Some(shared) = SHARED.get() else {
        return false;
    };
    if !shared.enabled.load(Ordering::Relaxed) {
        return false;
    }
    shared.reporter.submit(
        trigger,
        None,
        None,
        details,
        observed,
        shared.server.clone(),
    )
}

/// The dashboard row: one attempt of `{system, action}` with `ok|fail|refused`.
pub fn outcome(system: &str, action: &str, outcome: Outcome) -> bool {
    let Some(shared) = SHARED.get() else {
        return false;
    };
    if !shared.enabled.load(Ordering::Relaxed) {
        return false;
    }
    shared.reporter.outcome(system, action, outcome)
}

/// Waits until every queued report / outcome is processed and `summary.json` is written.
pub fn flush(timeout: Duration) -> bool {
    SHARED.get().is_some_and(|s| s.reporter.flush(timeout))
}

/// Stops accepting reports, writes the final summary and stops the writer.
pub fn shutdown(timeout: Duration) {
    if let Some(shared) = SHARED.get() {
        shared.enabled.store(false, Ordering::Relaxed);
        shared.reporter.shutdown(timeout);
    }
    packet_ring::set_capacity(0);
}

/// The tracing layer passed to `aaemu_commons::logging::init_with`.
pub fn layer() -> BugReportLayer {
    BugReportLayer::new()
}

/// Test teardown: the installed reporter must not keep affecting other tests in the process.
#[cfg(test)]
pub(crate) fn disable_for_tests() {
    if let Some(shared) = SHARED.get() {
        shared.enabled.store(false, Ordering::Relaxed);
    }
    packet_ring::set_capacity(0);
}
