//! Merge the per-race JSONL results into `crawl.json` and `crawl.md`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

use super::runner::QuestResult;

#[derive(Debug, Clone, Copy, Default, Serialize)]
struct Counts {
    passed: u64,
    /// Quests whose objective a GM command completed: shown apart from the plain passes.
    passed_gm: u64,
    failed: u64,
    unsupported: u64,
    skipped: u64,
    /// Quests the server reported as completed before this run (error 621).
    completed_before: u64,
}

impl Counts {
    fn add(&mut self, r: &QuestResult) {
        match r.status.as_str() {
            "passed" if r.gm_assisted => self.passed_gm += 1,
            "passed" => self.passed += 1,
            "failed" => self.failed += 1,
            "unsupported" => self.unsupported += 1,
            "skipped" => self.skipped += 1,
            "completed-before" => self.completed_before += 1,
            _ => self.failed += 1,
        }
    }
    fn total(&self) -> u64 {
        self.passed
            + self.passed_gm
            + self.failed
            + self.unsupported
            + self.skipped
            + self.completed_before
    }
    fn values(&self) -> [u64; 6] {
        [
            self.passed,
            self.passed_gm,
            self.failed,
            self.unsupported,
            self.skipped,
            self.completed_before,
        ]
    }
}

#[derive(Debug, Default, Serialize)]
struct RaceTotals {
    counts: Counts,
    by_zone: BTreeMap<String, Counts>,
    /// Keyed by the first level of the band, so the map (and the JSON) sorts numerically.
    by_band: BTreeMap<u16, Counts>,
}

#[derive(Debug, Default, Serialize)]
struct ActTotals {
    counts: Counts,
}

#[derive(Debug, Serialize)]
struct FailureGroup {
    quest: u32,
    name: String,
    level: u8,
    zone: String,
    races: Vec<String>,
    steps: BTreeMap<String, u64>,
    reasons: BTreeMap<String, u64>,
    /// The most common `step: reason` pair, so the rendered row does not mix one race's step
    /// with another race's reason.
    causes: BTreeMap<String, u64>,
    reports: BTreeSet<String>,
}

#[derive(Debug, Default, Serialize)]
struct Fingerprint {
    fingerprint: String,
    reports: u64,
    quests: BTreeSet<u32>,
    races: BTreeSet<String>,
}

#[derive(Debug, Default, Serialize)]
struct Dashboard {
    generated: String,
    results: u64,
    totals: Counts,
    races: BTreeMap<String, RaceTotals>,
    act_kinds: BTreeMap<String, ActTotals>,
    top_failures: Vec<FailureGroup>,
    bug_fingerprints: Vec<Fingerprint>,
}

const COLUMNS: [&str; 6] = [
    "passed",
    "passed (GM-assisted)",
    "failed",
    "unsupported",
    "skipped",
    "completed-before",
];

pub fn write(out: &Path) -> Result<()> {
    std::fs::create_dir_all(out)?;
    let mut latest: HashMap<(String, u32), QuestResult> = HashMap::new();
    for entry in std::fs::read_dir(out).with_context(|| format!("reading {}", out.display()))? {
        let path = entry?.path();
        if path.extension().is_none_or(|e| e != "jsonl") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in text.lines() {
            let Ok(r) = serde_json::from_str::<QuestResult>(line) else {
                continue;
            };
            latest.insert((r.race.clone(), r.quest), r);
        }
    }

    let mut d = Dashboard {
        generated: chrono_like_now(),
        ..Dashboard::default()
    };
    let mut failures: HashMap<u32, FailureGroup> = HashMap::new();
    let mut fingerprints: HashMap<String, Fingerprint> = HashMap::new();

    for r in latest.values() {
        d.results += 1;
        d.totals.add(r);
        let race = d.races.entry(r.race.clone()).or_default();
        race.counts.add(r);
        race.by_zone
            .entry(if r.zone.is_empty() {
                "unknown".into()
            } else {
                r.zone.clone()
            })
            .or_default()
            .add(r);
        race.by_band.entry(band(r.level)).or_default().add(r);

        let mut seen_acts = BTreeSet::new();
        for act in &r.act_kinds {
            if !seen_acts.insert(act.clone()) {
                continue;
            }
            d.act_kinds.entry(act.clone()).or_default().counts.add(r);
        }

        if r.status == "failed" {
            let f = failures.entry(r.quest).or_insert_with(|| FailureGroup {
                quest: r.quest,
                name: r.name.clone(),
                level: r.level,
                zone: r.zone.clone(),
                races: Vec::new(),
                steps: BTreeMap::new(),
                reasons: BTreeMap::new(),
                causes: BTreeMap::new(),
                reports: BTreeSet::new(),
            });
            if !f.races.contains(&r.race) {
                f.races.push(r.race.clone());
            }
            let step = if r.step.is_empty() {
                "unknown".to_string()
            } else {
                r.step.clone()
            };
            let reason = trim(&r.reason);
            *f.steps.entry(step.clone()).or_default() += 1;
            *f.reasons.entry(reason.clone()).or_default() += 1;
            *f.causes.entry(format!("{step}: {reason}")).or_default() += 1;
            for p in &r.reports {
                f.reports.insert(p.clone());
            }
        }
        for p in &r.reports {
            if let Some(fp) = fingerprint_of(p) {
                let e = fingerprints.entry(fp.clone()).or_default();
                e.fingerprint = fp;
                e.reports += 1;
                e.quests.insert(r.quest);
                e.races.insert(r.race.clone());
            }
        }
    }

    let mut top: Vec<FailureGroup> = failures.into_values().collect();
    top.sort_by(|a, b| {
        b.races
            .len()
            .cmp(&a.races.len())
            .then_with(|| {
                b.reasons
                    .values()
                    .sum::<u64>()
                    .cmp(&a.reasons.values().sum::<u64>())
            })
            .then(a.level.cmp(&b.level))
            .then(a.quest.cmp(&b.quest))
    });
    top.truncate(50);
    d.top_failures = top;

    let mut fps: Vec<Fingerprint> = fingerprints.into_values().collect();
    fps.sort_by(|a, b| {
        b.reports
            .cmp(&a.reports)
            .then(a.fingerprint.cmp(&b.fingerprint))
    });
    fps.truncate(30);
    d.bug_fingerprints = fps;

    let json = serde_json::to_string_pretty(&d)?;
    std::fs::write(out.join("crawl.json"), json)?;
    std::fs::write(out.join("crawl.md"), render(&d))?;
    Ok(())
}

fn render(d: &Dashboard) -> String {
    let mut s = String::new();
    s.push_str("# Quest crawler\n\n");
    s.push_str(&format!(
        "Generated {}; {} quest results.\n\n",
        d.generated, d.results
    ));
    s.push_str(&format!(
        "**Totals:** {} passed, {} passed (GM-assisted), {} failed, {} unsupported, {} skipped, \
         {} completed before.\n\n",
        d.totals.passed,
        d.totals.passed_gm,
        d.totals.failed,
        d.totals.unsupported,
        d.totals.skipped,
        d.totals.completed_before
    ));

    s.push_str(&table_header("race"));
    for (race, t) in &d.races {
        s.push_str(&table_row(race, &t.counts));
    }
    s.push('\n');

    s.push_str("## Per level band\n\n");
    s.push_str(&table_header("band"));
    let mut bands: BTreeMap<u16, Counts> = BTreeMap::new();
    for t in d.races.values() {
        for (lo, c) in &t.by_band {
            let e = bands.entry(*lo).or_default();
            *e = add_counts(*e, *c);
        }
    }
    for (lo, c) in &bands {
        s.push_str(&table_row(&format!("{lo}-{}", lo + 4), c));
    }
    s.push('\n');

    s.push_str("## Per zone\n\n");
    s.push_str(&table_header("zone"));
    let mut zones: BTreeMap<String, Counts> = BTreeMap::new();
    for t in d.races.values() {
        for (zone, c) in &t.by_zone {
            let e = zones.entry(zone.clone()).or_default();
            *e = add_counts(*e, *c);
        }
    }
    let mut zones: Vec<(String, Counts)> = zones.into_iter().collect();
    zones.sort_by_key(|a| std::cmp::Reverse(a.1.total()));
    for (zone, c) in zones.iter().take(40) {
        s.push_str(&table_row(zone, c));
    }
    s.push('\n');

    s.push_str("## Per act kind\n\n");
    s.push_str(&table_header("act"));
    let mut acts: Vec<(&String, &ActTotals)> = d.act_kinds.iter().collect();
    acts.sort_by_key(|a| std::cmp::Reverse(a.1.counts.total()));
    for (act, t) in acts {
        s.push_str(&table_row(act, &t.counts));
    }
    s.push('\n');

    s.push_str("## Top failing quests (ranked by how many races hit them)\n\n");
    s.push_str(
        "| quest | level | name | races | failing step | reason |\n|---:|---:|---|---|---|---|\n",
    );
    for f in &d.top_failures {
        let cause = f
            .causes
            .iter()
            .max_by_key(|(_, n)| **n)
            .map(|(c, _)| c.clone())
            .unwrap_or_default();
        let (step, reason) = cause
            .split_once(": ")
            .map_or((cause.clone(), String::new()), |(s, r)| {
                (s.to_string(), r.to_string())
            });
        s.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} |\n",
            f.quest,
            f.level,
            escape(&f.name),
            f.races.join(", "),
            escape(&step),
            escape(&reason)
        ));
    }
    s.push('\n');

    s.push_str("## Bug report fingerprints\n\n");
    if d.bug_fingerprints.is_empty() {
        s.push_str("No new report file was written during the crawl.\n");
    } else {
        s.push_str("| fingerprint | reports | quests | races |\n|---|---:|---|---|\n");
        for f in &d.bug_fingerprints {
            s.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                f.fingerprint,
                f.reports,
                f.quests
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(", "),
                f.races.iter().cloned().collect::<Vec<_>>().join(", ")
            ));
        }
    }
    s
}

/// First level of the 1-5, 6-10, ... band a quest level belongs to.
fn band(level: u8) -> u16 {
    let level = u16::from(level.max(1));
    (level - 1) / 5 * 5 + 1
}

fn add_counts(a: Counts, b: Counts) -> Counts {
    Counts {
        passed: a.passed + b.passed,
        passed_gm: a.passed_gm + b.passed_gm,
        failed: a.failed + b.failed,
        unsupported: a.unsupported + b.unsupported,
        skipped: a.skipped + b.skipped,
        completed_before: a.completed_before + b.completed_before,
    }
}

fn table_header(first: &str) -> String {
    let mut s = format!("| {first} |");
    for column in COLUMNS {
        s.push_str(&format!(" {column} |"));
    }
    s.push('\n');
    s.push_str("|---|");
    for _ in COLUMNS {
        s.push_str("---:|");
    }
    s.push('\n');
    s
}

fn table_row(name: &str, counts: &Counts) -> String {
    let mut s = format!("| {} |", escape(name));
    for value in counts.values() {
        s.push_str(&format!(" {value} |"));
    }
    s.push('\n');
    s
}

fn fingerprint_of(path: &str) -> Option<String> {
    let name = Path::new(path).file_name()?.to_str()?;
    let stem = name.strip_suffix(".json")?;
    let (fp, _n) = stem.rsplit_once('-')?;
    if fp.len() < 8 {
        return None;
    }
    Some(fp.to_string())
}

fn trim(s: &str) -> String {
    s.chars().take(240).collect()
}

fn escape(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ")
}

fn chrono_like_now() -> String {
    // Avoid a chrono dependency in the test client: UTC seconds since the epoch is enough for
    // a local dashboard.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("unix {secs}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(quest: u32, status: &str, gm_assisted: bool) -> QuestResult {
        QuestResult {
            quest,
            name: format!("quest {quest}"),
            level: 3,
            zone: "zone".into(),
            race: "nuian".into(),
            status: status.into(),
            step: "complete".into(),
            reason: String::new(),
            gm_assisted,
            gm_assisted_steps: if gm_assisted {
                vec!["objective 1 QuestActObjItemGather".into()]
            } else {
                Vec::new()
            },
            act_kinds: Vec::new(),
            objectives: Vec::new(),
            duration_ms: 0,
            warnings: Vec::new(),
            reports: Vec::new(),
            account_id: 1,
        }
    }

    #[test]
    fn gm_assisted_passes_are_their_own_column() {
        let mut counts = Counts::default();
        counts.add(&result(1, "passed", false));
        counts.add(&result(2, "passed", true));
        counts.add(&result(3, "failed", false));
        counts.add(&result(4, "completed-before", false));
        assert_eq!(counts.passed, 1);
        assert_eq!(counts.passed_gm, 1);
        assert_eq!(counts.failed, 1);
        assert_eq!(counts.completed_before, 1);
        assert_eq!(counts.total(), 4);

        let mut dashboard = Dashboard {
            generated: "test".into(),
            results: 4,
            totals: counts,
            ..Dashboard::default()
        };
        let race = dashboard.races.entry("nuian".into()).or_default();
        race.counts = counts;
        for level in [11, 1, 6] {
            race.by_band.insert(band(level), counts);
        }
        let md = render(&dashboard);
        assert!(
            md.contains("1 passed, 1 passed (GM-assisted), 1 failed"),
            "{md}"
        );
        let first = md.find("| 1-5 |").expect("1-5 row");
        let second = md.find("| 6-10 |").expect("6-10 row");
        let third = md.find("| 11-15 |").expect("11-15 row");
        assert!(first < second && second < third, "{md}");
    }

    #[test]
    fn bands_do_not_overlap() {
        assert_eq!(band(0), 1);
        assert_eq!(band(1), 1);
        assert_eq!(band(5), 1);
        assert_eq!(band(6), 6);
        assert_eq!(band(10), 6);
        assert_eq!(band(11), 11);
        assert_eq!(band(26), 26);
        let labels: Vec<String> = [26u8, 11, 6, 1]
            .into_iter()
            .map(band)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(|lo| format!("{lo}-{}", lo + 4))
            .collect();
        assert_eq!(labels, ["1-5", "6-10", "11-15", "26-30"]);
    }
}
