//! End-to-end flow tests: tracing layer, packet ring, HP invariant, `/bug` and the summary.
//!
//! One test on purpose: the reporter is process-global (like the tracing subscriber) and the
//! scenarios share its dedup counters.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use tracing_subscriber::layer::SubscriberExt;

use super::config::BugReportsConfig;
use super::{invariants, outcome, Outcome, Severity, Trigger, FORMAT};
use crate::systems::services::commands::scripts::add_xp::tests::spawn_npc;
use crate::systems::services::commands::scripts::harness;

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("aaemu-bugreport-flow-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn run_dir(root: &Path) -> PathBuf {
    let mut dirs: Vec<PathBuf> = fs::read_dir(root)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    assert_eq!(dirs.len(), 1, "one run directory");
    dirs.remove(0)
}

fn reports(dir: &Path) -> Vec<Value> {
    let mut out: Vec<Value> = fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".json") && n != "summary.json")
        })
        .map(|p| serde_json::from_slice(&fs::read(p).unwrap()).unwrap())
        .collect();
    out.sort_by_key(|r| r["id"].as_str().unwrap_or_default().to_string());
    out
}

fn summary(dir: &Path) -> Value {
    serde_json::from_slice(&fs::read(dir.join("summary.json")).unwrap()).unwrap()
}

fn flush() {
    assert!(super::flush(Duration::from_secs(10)), "writer flush");
}

struct DisableOnDrop;

impl Drop for DisableOnDrop {
    fn drop(&mut self) {
        super::disable_for_tests();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bugreport_flow() {
    let _guard = DisableOnDrop;
    let root = temp_dir();
    let server = harness::server();
    let config = BugReportsConfig {
        enabled: true,
        directory: Some(root.to_string_lossy().into_owned()),
        max_full_reports_per_fingerprint: 1,
        nearby_radius: 40.0,
        packet_ring: 8,
        log_ring: 50,
        rate_per_second: 100,
        max_fingerprints: 2_000,
        max_full_reports: 2_000,
        max_total_bytes: 64 * 1024 * 1024,
    };
    super::install(&server, &config).expect("install bug reports");

    let bob = harness::pc(&server, 1, "Bob", 100.0, 100.0);
    let npc = spawn_npc(&server, 0x500, 10, 105.0, 100.0, 1_000);
    bob.p
        .send(crate::packets::world::SCErrorMsgPacket::new(1, 2, false));

    // A WARN mentioning the NPC id -> report with its snapshot and the nearby player.
    let subscriber = tracing_subscriber::registry().with(super::layer());
    tracing::subscriber::with_default(subscriber, || {
        tracing::warn!("unit {} exploded", npc.obj_id);
    });
    flush();
    let dir = run_dir(&root);
    let all = reports(&dir);
    let warn = all
        .iter()
        .find(|r| r["trigger"]["kind"] == "log")
        .expect("log report");
    assert_eq!(warn["format"], FORMAT);
    assert_eq!(warn["target"]["obj_id"], npc.obj_id);
    assert_eq!(warn["target"]["template_id"], 10);
    assert!(
        warn["nearby"]["units"]
            .as_array()
            .unwrap()
            .iter()
            .any(|u| u["obj_id"] == bob.p.obj_id),
        "nearby player: {warn:#}"
    );
    assert!(
        warn["logs"].as_array().unwrap().iter().any(|l| l["message"]
            .as_str()
            .unwrap_or_default()
            .contains("exploded")),
        "log ring line: {warn:#}"
    );

    // The packet ring of the actor's connection (explicit report with actor = player).
    assert!(super::report(
        &server,
        Trigger::external("test", "packet ring", Severity::Warn),
        Some(bob.p.obj_id),
        Some(npc.obj_id),
        serde_json::json!({}),
    ));
    flush();
    let all = reports(&dir);
    let ring = all
        .iter()
        .find(|r| r["trigger"]["kind"] == "external")
        .expect("external report");
    let outgoing = ring["packets"]["out"].as_array().unwrap();
    assert!(
        outgoing
            .iter()
            .any(|p| p["name"].as_str().unwrap_or_default().contains("ErrorMsg")),
        "packet ring: {ring:#}"
    );

    // HP invariant: first check writes the full report, the second only increments counters.
    bob.p.data().unit.hp = -1;
    invariants::check_now(&server);
    flush();
    invariants::check_now(&server);
    flush();
    let summary_json = summary(&dir);
    let hp = summary_json["fingerprints"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["subject"] == "unit_hp")
        .expect("unit_hp fingerprint");
    assert_eq!(hp["count"], 2);
    assert_eq!(hp["full_reports"], 1);
    assert!(hp["example"].is_string());

    // Dashboard rows.
    assert!(outcome("combat", "attack", Outcome::Ok));
    assert!(outcome("combat", "attack", Outcome::Fail));
    flush();
    let summary_json = summary(&dir);
    let row = summary_json["systems"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["system"] == "combat" && r["action"] == "attack")
        .expect("combat row");
    assert_eq!(row["ok"], 1);
    assert_eq!(row["fail"], 1);
    assert_eq!(row["total"], 2);

    // `/bug text` writes a command report with the player as actor.
    let (handled, out) = harness::run(&server, &bob.p, "bug test text").await;
    assert!(handled);
    assert!(
        out.iter().any(|m| m.contains("Report submitted")),
        "{out:?}"
    );
    flush();
    let all = reports(&dir);
    let command = all
        .iter()
        .find(|r| r["trigger"]["kind"] == "command")
        .expect("command report");
    assert_eq!(command["actor"]["obj_id"], bob.p.obj_id);
    assert_eq!(command["trigger"]["text"], "test text");
    assert!(!command["packets"]["out"].as_array().unwrap().is_empty());

    // Panic hook: the default output is kept; one `panic` report is queued.
    let _ = std::panic::catch_unwind(|| panic!("bugreport panic hook test"));
    flush();
    let all = reports(&dir);
    assert!(
        all.iter().any(|r| {
            r["trigger"]["kind"] == "panic"
                && r["trigger"]["message"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("bugreport panic hook test")
        }),
        "panic report: {all:?}"
    );

    assert!(root.exists());
}
