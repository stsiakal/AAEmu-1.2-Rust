//! The report writer: dedup, rate limit, snapshots, JSON files and the run summary.
//!
//! One dedicated thread owns all state; callers only `try_send` a small message. A full
//! channel drops the message and bumps the `dropped_events` counter instead of blocking.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use chrono::{SecondsFormat, TimeZone, Utc};
use serde::Serialize;
use serde_json::{json, Value};

use super::fingerprint::{self, max_severity};
use super::log_ring::LogRing;
use super::redact;
use super::snapshot;
use super::{Outcome, Severity, Trigger, BUILD, FORMAT};
use crate::server::GameServer;

/// Queue depth for the writer (events beyond it are dropped and counted).
const CHANNEL: usize = 4096;
/// `summary.json` refresh interval.
const SUMMARY_INTERVAL: Duration = Duration::from_secs(5);
/// `/bug` reports from one character are ignored for this long after the previous one.
const COMMAND_COOLDOWN_MS: i64 = 60_000;

#[derive(Debug, Clone)]
pub struct ReporterConfig {
    pub directory: PathBuf,
    pub max_full_reports_per_fingerprint: u32,
    pub nearby_radius: f32,
    pub log_ring: usize,
    pub rate_per_second: u32,
    pub max_fingerprints: usize,
    pub max_full_reports: u64,
    pub max_total_bytes: u64,
}

enum Cmd {
    Report(ReportRequest),
    Outcome {
        system: String,
        action: String,
        outcome: Outcome,
    },
    Flush(SyncSender<()>),
    Shutdown(SyncSender<()>),
}

struct ReportRequest {
    trigger: Trigger,
    actor: Option<u32>,
    target: Option<u32>,
    details: Value,
    observed: Vec<u32>,
    server: Option<Weak<GameServer>>,
    timestamp_ms: i64,
}

/// Handle to the writer thread.
#[derive(Debug)]
pub struct Reporter {
    tx: SyncSender<Cmd>,
    dropped: Arc<AtomicU64>,
    run_dir: Arc<PathBuf>,
    log_ring: Arc<LogRing>,
}

impl Reporter {
    pub fn start(config: ReporterConfig, server: Option<Weak<GameServer>>) -> io::Result<Self> {
        let (run, run_dir) = create_run_dir(&config.directory)?;
        let (tx, rx) = mpsc::sync_channel(CHANNEL);
        let dropped = Arc::new(AtomicU64::new(0));
        let writer_dropped = dropped.clone();
        let writer_run_dir = run_dir.clone();
        let log_ring = Arc::new(LogRing::new(config.log_ring));
        let writer_log_ring = log_ring.clone();
        std::thread::Builder::new()
            .name("bugreport-writer".into())
            .spawn(move || {
                super::mark_writer_thread();
                Writer::new(
                    config,
                    server,
                    run,
                    writer_run_dir,
                    writer_dropped,
                    writer_log_ring,
                )
                .run(rx);
            })?;
        Ok(Self {
            tx,
            dropped,
            run_dir: Arc::new(run_dir),
            log_ring,
        })
    }

    /// The `<Directory>/<run>/` folder of this run.
    pub fn run_directory(&self) -> &Path {
        &self.run_dir
    }

    /// The log ring shared with the tracing layer.
    pub(crate) fn log_ring(&self) -> Arc<LogRing> {
        self.log_ring.clone()
    }

    pub fn submit(
        &self,
        trigger: Trigger,
        actor: Option<u32>,
        target: Option<u32>,
        details: Value,
        observed: Vec<u32>,
        server: Option<Weak<GameServer>>,
    ) -> bool {
        self.send(Cmd::Report(ReportRequest {
            trigger,
            actor,
            target,
            details,
            observed,
            server,
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
        }))
    }

    pub fn outcome(&self, system: &str, action: &str, outcome: Outcome) -> bool {
        self.send(Cmd::Outcome {
            system: system.to_string(),
            action: action.to_string(),
            outcome,
        })
    }

    pub fn flush(&self, timeout: Duration) -> bool {
        let (ack, rx) = mpsc::sync_channel(1);
        if self.tx.send(Cmd::Flush(ack)).is_err() {
            return false;
        }
        rx.recv_timeout(timeout).is_ok()
    }

    pub fn shutdown(&self, timeout: Duration) {
        let (ack, rx) = mpsc::sync_channel(1);
        if self.tx.send(Cmd::Shutdown(ack)).is_ok() {
            let _ = rx.recv_timeout(timeout);
        }
    }

    fn send(&self, cmd: Cmd) -> bool {
        match self.tx.try_send(cmd) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                false
            }
            Err(TrySendError::Disconnected(_)) => false,
        }
    }
}

fn create_run_dir(root: &Path) -> io::Result<(String, PathBuf)> {
    fs::create_dir_all(root)?;
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let base = format!("{stamp}-{}", std::process::id());
    let mut run = base.clone();
    let mut n = 1;
    while root.join(&run).exists() {
        run = format!("{base}-{n}");
        n += 1;
    }
    let dir = root.join(&run);
    fs::create_dir_all(&dir)?;
    Ok((run, dir))
}

#[derive(Debug, Default, Serialize)]
struct Outcomes {
    ok: u64,
    fail: u64,
    refused: u64,
}

impl Outcomes {
    fn add(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::Ok => self.ok += 1,
            Outcome::Fail => self.fail += 1,
            Outcome::Refused => self.refused += 1,
        }
    }

    fn total(&self) -> u64 {
        self.ok + self.fail + self.refused
    }
}

struct Entry {
    kind: &'static str,
    subject: String,
    severity: Severity,
    count: u64,
    full_reports: u32,
    first_ms: i64,
    last_ms: i64,
    example: Option<String>,
}

struct Writer {
    config: ReporterConfig,
    server: Option<Weak<GameServer>>,
    run: String,
    run_dir: PathBuf,
    dropped: Arc<AtomicU64>,
    next_id: u64,
    entries: BTreeMap<String, Entry>,
    log_ring: Arc<LogRing>,
    systems: BTreeMap<(String, String), Outcomes>,
    window_second: i64,
    window_count: u32,
    full_reports_written: u64,
    total_bytes: u64,
    capped_reports: u64,
    cooldown_rejections: u64,
    last_command_ms: HashMap<u32, i64>,
    dirty: bool,
    last_summary: Instant,
    started_ms: i64,
}

impl Writer {
    fn new(
        config: ReporterConfig,
        server: Option<Weak<GameServer>>,
        run: String,
        run_dir: PathBuf,
        dropped: Arc<AtomicU64>,
        log_ring: Arc<LogRing>,
    ) -> Self {
        Self {
            config,
            server,
            run,
            run_dir,
            dropped,
            next_id: 1,
            entries: BTreeMap::new(),
            log_ring,
            systems: BTreeMap::new(),
            window_second: 0,
            window_count: 0,
            full_reports_written: 0,
            total_bytes: 0,
            capped_reports: 0,
            cooldown_rejections: 0,
            last_command_ms: HashMap::new(),
            dirty: false,
            last_summary: Instant::now(),
            started_ms: chrono::Utc::now().timestamp_millis(),
        }
    }

    fn run(mut self, rx: Receiver<Cmd>) {
        loop {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(cmd) => {
                    if self.handle(cmd) {
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            if self.dirty && self.last_summary.elapsed() >= SUMMARY_INTERVAL {
                self.write_summary();
            }
        }
    }

    /// `true` = stop.
    fn handle(&mut self, cmd: Cmd) -> bool {
        match cmd {
            Cmd::Report(req) => self.handle_report(req),
            Cmd::Outcome {
                system,
                action,
                outcome,
            } => {
                self.systems
                    .entry((system, action))
                    .or_default()
                    .add(outcome);
                self.dirty = true;
            }
            Cmd::Flush(ack) => {
                self.write_summary();
                let _ = ack.send(());
            }
            Cmd::Shutdown(ack) => {
                self.write_summary();
                let _ = ack.send(());
                return true;
            }
        }
        false
    }

    fn handle_report(&mut self, req: ReportRequest) {
        let trigger = redact_trigger(req.trigger.clone());
        let timestamp_ms = req.timestamp_ms;

        // `/bug` anti-abuse: one command report per character per minute.
        if matches!(trigger, Trigger::Command { .. }) {
            if let Some(actor) = req.actor {
                if let Some(last) = self.last_command_ms.get(&actor) {
                    if timestamp_ms.saturating_sub(*last) < COMMAND_COOLDOWN_MS {
                        self.cooldown_rejections += 1;
                        self.dirty = true;
                        return;
                    }
                }
                self.last_command_ms.insert(actor, timestamp_ms);
            }
        }

        let fingerprint = fingerprint::fingerprint(&trigger, req.actor);
        if !self.entries.contains_key(&fingerprint)
            && self.entries.len() >= self.config.max_fingerprints
        {
            self.capped_reports += 1;
            self.dirty = true;
            return;
        }
        let severity = trigger.severity();

        let (_, full_reports) = {
            let entry = self.entries.entry(fingerprint.clone()).or_insert(Entry {
                kind: trigger.kind(),
                subject: trigger.subject().to_string(),
                severity,
                count: 0,
                full_reports: 0,
                first_ms: timestamp_ms,
                last_ms: timestamp_ms,
                example: None,
            });
            entry.count += 1;
            entry.last_ms = timestamp_ms;
            entry.severity = max_severity(entry.severity, severity);
            (entry.count, entry.full_reports)
        };
        self.dirty = true;

        if full_reports >= self.config.max_full_reports_per_fingerprint {
            return;
        }
        if self.full_reports_written >= self.config.max_full_reports
            || self.total_bytes >= self.config.max_total_bytes
        {
            self.capped_reports += 1;
            return;
        }
        if !self.take_rate() {
            return;
        }

        let number = full_reports + 1;
        let file_name = format!("{fingerprint}-{number}.json");
        let report = self.build_report(&req, &trigger, &fingerprint, number, timestamp_ms);
        let report = redact::redact_value(report);
        let bytes = match serde_json::to_vec_pretty(&report) {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::warn!("bugreport: cannot encode report: {e}");
                return;
            }
        };
        if self.total_bytes + bytes.len() as u64 > self.config.max_total_bytes {
            self.capped_reports += 1;
            return;
        }
        let path = self.run_dir.join(&file_name);
        match write_bytes(&path, &bytes) {
            Ok(()) => {
                self.full_reports_written += 1;
                self.total_bytes += bytes.len() as u64;
                if let Some(entry) = self.entries.get_mut(&fingerprint) {
                    entry.full_reports = number;
                    if entry.example.is_none() {
                        entry.example = Some(file_name);
                    }
                }
            }
            Err(e) => {
                tracing::warn!("bugreport: cannot write {}: {e}", path.display());
            }
        }
    }

    /// Per-second full-report budget.
    fn take_rate(&mut self) -> bool {
        let second = chrono::Utc::now().timestamp();
        if second != self.window_second {
            self.window_second = second;
            self.window_count = 0;
        }
        if self.window_count >= self.config.rate_per_second {
            return false;
        }
        self.window_count += 1;
        true
    }

    fn build_report(
        &mut self,
        req: &ReportRequest,
        trigger: &Trigger,
        fingerprint: &str,
        number: u32,
        timestamp_ms: i64,
    ) -> Value {
        let id = format!("{}-{:06}", self.run, self.next_id);
        self.next_id += 1;
        let server = req
            .server
            .as_ref()
            .and_then(Weak::upgrade)
            .or_else(|| self.server.as_ref().and_then(Weak::upgrade));

        let mut report = json!({
            "format": FORMAT,
            "id": id,
            "fingerprint": fingerprint,
            "fingerprint_number": number,
            "trigger": trigger,
            "severity": trigger.severity().as_str(),
            "timestamp": rfc3339(timestamp_ms),
            "timestamp_ms": timestamp_ms,
            "build": BUILD,
            "run": self.run,
        });
        let details = redact::redact_value(req.details.clone());
        report["details"] = details;
        report["actor"] = Value::Null;
        report["target"] = Value::Null;
        report["observed_ids"] = json!(req.observed);
        report["nearby"] = json!({"radius": self.config.nearby_radius, "units": [], "doodads": []});
        report["ship_audit"] = Value::Null;
        report["packets"] = json!({"in": [], "out": []});
        report["logs"] = json!([]);

        if let Some(server) = server {
            let logs = self.log_ring.snapshot();
            let snapshot = snapshot::collect(
                &server,
                req.actor,
                req.target,
                &req.observed,
                self.config.nearby_radius,
                &logs,
            );
            report["actor"] = snapshot.actor;
            report["target"] = snapshot.target;
            report["nearby"] = snapshot.nearby;
            report["ship_audit"] = snapshot.ship_audit;
            report["packets"] = snapshot.packets;
            report["logs"] = Value::Array(snapshot.logs);
        }
        report
    }

    fn write_summary(&mut self) {
        if !self.dirty && self.last_summary.elapsed() < SUMMARY_INTERVAL {
            return;
        }
        let fingerprints: Vec<Value> = self
            .entries
            .iter()
            .map(|(fingerprint, entry)| {
                json!({
                    "fingerprint": fingerprint,
                    "kind": entry.kind,
                    "subject": entry.subject,
                    "severity": entry.severity.as_str(),
                    "count": entry.count,
                    "full_reports": entry.full_reports,
                    "first": rfc3339(entry.first_ms),
                    "last": rfc3339(entry.last_ms),
                    "example": entry.example,
                })
            })
            .collect();
        let systems: Vec<Value> = self
            .systems
            .iter()
            .map(|((system, action), counts)| {
                let total = counts.total();
                json!({
                    "system": system,
                    "action": action,
                    "ok": counts.ok,
                    "fail": counts.fail,
                    "refused": counts.refused,
                    "total": total,
                    "success_rate": if total == 0 { 0.0 } else { counts.ok as f64 / total as f64 },
                })
            })
            .collect();
        let reports: u64 = self.entries.values().map(|e| e.count).sum();
        let summary = json!({
            "format": FORMAT,
            "run": self.run,
            "started": rfc3339(self.started_ms),
            "updated": rfc3339(chrono::Utc::now().timestamp_millis()),
            "build": BUILD,
            "reports": reports,
            "full_reports": self.full_reports_written,
            "bytes": self.total_bytes,
            "capped_reports": self.capped_reports,
            "cooldown_rejections": self.cooldown_rejections,
            "fingerprints": fingerprints,
            "systems": systems,
            "dropped_events": self.dropped.load(Ordering::Relaxed),
        });
        let path = self.run_dir.join("summary.json");
        match serde_json::to_vec_pretty(&summary) {
            Ok(bytes) => {
                if let Err(e) = write_bytes(&path, &bytes) {
                    tracing::warn!("bugreport: cannot write {}: {e}", path.display());
                }
            }
            Err(e) => tracing::warn!("bugreport: cannot encode summary: {e}"),
        }
        self.dirty = false;
        self.last_summary = Instant::now();
    }
}

fn redact_trigger(trigger: Trigger) -> Trigger {
    match trigger {
        Trigger::Log {
            level,
            target,
            message,
            mut fields,
        } => {
            for value in fields.values_mut() {
                *value = redact::redact_text(value);
            }
            Trigger::Log {
                level,
                target,
                message: redact::redact_text(&message),
                fields,
            }
        }
        Trigger::Panic {
            location,
            message,
            thread,
        } => Trigger::Panic {
            location,
            message: redact::redact_text(&message),
            thread,
        },
        Trigger::Command { text } => Trigger::Command {
            text: redact::redact_text(&text),
        },
        Trigger::Invariant { check, message } => Trigger::Invariant {
            check,
            message: redact::redact_text(&message),
        },
        Trigger::External {
            subsystem,
            message,
            severity,
        } => Trigger::External {
            subsystem,
            message: redact::redact_text(&message),
            severity,
        },
    }
}

pub(crate) fn rfc3339(ms: i64) -> String {
    Utc.timestamp_millis_opt(ms)
        .single()
        .map(|d| d.to_rfc3339_opts(SecondsFormat::Millis, true))
        .unwrap_or_default()
}

fn write_bytes(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(dir: &Path) -> ReporterConfig {
        ReporterConfig {
            directory: dir.to_path_buf(),
            max_full_reports_per_fingerprint: 1,
            nearby_radius: 40.0,
            log_ring: 200,
            rate_per_second: 100,
            max_fingerprints: 2_000,
            max_full_reports: 2_000,
            max_total_bytes: 64 * 1024 * 1024,
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("aaemu-bugreport-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn trigger(message: &str) -> Trigger {
        Trigger::External {
            subsystem: "test".into(),
            message: message.into(),
            severity: Severity::Warn,
        }
    }

    fn command(text: &str) -> Trigger {
        Trigger::Command { text: text.into() }
    }

    fn reports(reporter: &Reporter) -> Vec<Value> {
        let mut out = Vec::new();
        for e in fs::read_dir(reporter.run_directory()).unwrap().flatten() {
            let path = e.path();
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".json") && n != "summary.json")
            {
                out.push(serde_json::from_slice(&fs::read(path).unwrap()).unwrap());
            }
        }
        out
    }

    fn summary(reporter: &Reporter) -> Value {
        serde_json::from_slice(&fs::read(reporter.run_directory().join("summary.json")).unwrap())
            .unwrap()
    }

    #[test]
    fn dedup_counts_after_the_full_report_budget() {
        let dir = temp_dir("dedup");
        let reporter = Reporter::start(config(&dir), None).unwrap();
        for _ in 0..4 {
            assert!(reporter.submit(
                trigger("unit 1234 exploded"),
                None,
                None,
                Value::Null,
                Vec::new(),
                None
            ));
        }
        assert!(reporter.flush(Duration::from_secs(5)));
        assert_eq!(reports(&reporter).len(), 1);
        let summary = summary(&reporter);
        let f = &summary["fingerprints"][0];
        assert_eq!(f["count"], 4);
        assert_eq!(f["full_reports"], 1);
        assert!(f["example"].is_string());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rate_limit_keeps_counters_but_writes_fewer_files() {
        let dir = temp_dir("rate");
        let mut cfg = config(&dir);
        cfg.max_full_reports_per_fingerprint = 10;
        cfg.rate_per_second = 1;
        let reporter = Reporter::start(cfg, None).unwrap();
        for i in 0..6 {
            reporter.submit(
                trigger(&format!("object {i} is broken")),
                None,
                None,
                Value::Null,
                Vec::new(),
                None,
            );
        }
        assert!(reporter.flush(Duration::from_secs(5)));
        let summary = summary(&reporter);
        assert_eq!(summary["reports"], 6);
        assert!(reports(&reporter).len() <= 2, "one full report per second");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn command_reports_are_per_character_and_cool_down() {
        let dir = temp_dir("command");
        let reporter = Reporter::start(config(&dir), None).unwrap();
        reporter.submit(
            command("first"),
            Some(7),
            None,
            Value::Null,
            Vec::new(),
            None,
        );
        reporter.submit(
            command("second"),
            Some(7),
            None,
            Value::Null,
            Vec::new(),
            None,
        );
        reporter.submit(
            command("other player"),
            Some(8),
            None,
            Value::Null,
            Vec::new(),
            None,
        );
        assert!(reporter.flush(Duration::from_secs(5)));
        assert_eq!(reports(&reporter).len(), 2);
        let summary = summary(&reporter);
        assert_eq!(summary["cooldown_rejections"], 1);
        let command_rows: Vec<&Value> = summary["fingerprints"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|f| f["kind"] == "command")
            .collect();
        assert_eq!(command_rows.len(), 2, "one fingerprint per character");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn global_file_and_fingerprint_caps_keep_counters() {
        let dir = temp_dir("caps");
        let mut cfg = config(&dir);
        cfg.max_full_reports = 1;
        cfg.max_fingerprints = 1;
        let reporter = Reporter::start(cfg, None).unwrap();
        reporter.submit(
            trigger("first failure"),
            None,
            None,
            Value::Null,
            Vec::new(),
            None,
        );
        reporter.submit(
            trigger("second failure"),
            None,
            None,
            Value::Null,
            Vec::new(),
            None,
        );
        reporter.submit(
            trigger("first failure"),
            None,
            None,
            Value::Null,
            Vec::new(),
            None,
        );
        assert!(reporter.flush(Duration::from_secs(5)));
        assert_eq!(reports(&reporter).len(), 1);
        let summary = summary(&reporter);
        assert_eq!(summary["fingerprints"].as_array().unwrap().len(), 1);
        assert_eq!(summary["fingerprints"][0]["count"], 2);
        assert_eq!(summary["capped_reports"], 1);
        assert_eq!(summary["full_reports"], 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn details_are_redacted_and_systems_rows_are_written() {
        let dir = temp_dir("redact");
        let reporter = Reporter::start(config(&dir), None).unwrap();
        reporter.submit(
            trigger("login failed"),
            None,
            None,
            json!({"password": "hunter2", "note": "token=abc"}),
            Vec::new(),
            None,
        );
        assert!(reporter.outcome("combat", "attack", Outcome::Ok));
        assert!(reporter.outcome("combat", "attack", Outcome::Fail));
        assert!(reporter.outcome("trade", "sell", Outcome::Refused));
        assert!(reporter.flush(Duration::from_secs(5)));
        let text = serde_json::to_string(&reports(&reporter)).unwrap();
        assert!(!text.contains("hunter2"));
        assert!(!text.contains("token=abc"));
        let summary = summary(&reporter);
        let rows = summary["systems"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["system"], "combat");
        assert_eq!(rows[0]["ok"], 1);
        assert_eq!(rows[0]["fail"], 1);
        assert_eq!(rows[0]["total"], 2);
        assert_eq!(rows[1]["refused"], 1);
        let _ = fs::remove_dir_all(&dir);
    }
}
