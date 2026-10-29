//! `BugReports` configuration section (Rust only).

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// `BugReports` in `Config.json` / `Config.Local.json`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct BugReportsConfig {
    /// `Enabled`: when false nothing is captured (no packet ring, no layer work, no timers).
    #[serde(rename = "enabled", deserialize_with = "crate::config::lenient")]
    pub enabled: bool,
    /// `Directory`: report root; empty/absent means `<app dir>/BugReports`.
    #[serde(rename = "directory")]
    pub directory: Option<String>,
    /// How many full report files one fingerprint may write per run.
    #[serde(
        rename = "maxfullreportsperfingerprint",
        deserialize_with = "crate::config::lenient"
    )]
    pub max_full_reports_per_fingerprint: u32,
    /// `NearbyRadius` in metres for the nearby unit / doodad snapshot.
    #[serde(rename = "nearbyradius", deserialize_with = "crate::config::lenient")]
    pub nearby_radius: f32,
    /// Packets kept per connection (`0` disables the packet ring).
    #[serde(rename = "packetring", deserialize_with = "crate::config::lenient")]
    pub packet_ring: usize,
    /// Log lines kept for the report (`0` disables the log ring).
    #[serde(rename = "logring", deserialize_with = "crate::config::lenient")]
    pub log_ring: usize,
    /// Full reports written per second (`0` disables full files; counters still update).
    #[serde(rename = "ratepersecond", deserialize_with = "crate::config::lenient")]
    pub rate_per_second: u32,
    /// Distinct fingerprints kept per run; new ones only bump `capped_reports`.
    #[serde(
        rename = "maxfingerprints",
        deserialize_with = "crate::config::lenient"
    )]
    pub max_fingerprints: usize,
    /// Full report files per run; later occurrences only bump counters.
    #[serde(rename = "maxfullreports", deserialize_with = "crate::config::lenient")]
    pub max_full_reports: u64,
    /// Total bytes of report files per run.
    #[serde(rename = "maxtotalbytes", deserialize_with = "crate::config::lenient")]
    pub max_total_bytes: u64,
}

impl Default for BugReportsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            directory: None,
            max_full_reports_per_fingerprint: 3,
            nearby_radius: 40.0,
            packet_ring: 64,
            log_ring: 200,
            rate_per_second: 5,
            max_fingerprints: 2_000,
            max_full_reports: 2_000,
            max_total_bytes: 64 * 1024 * 1024,
        }
    }
}

impl BugReportsConfig {
    /// `Directory` resolved against the application directory.
    pub fn resolve_directory(&self, app_path: &Path) -> PathBuf {
        match self.directory.as_deref().map(str::trim) {
            Some(dir) if !dir.is_empty() => PathBuf::from(dir),
            _ => app_path.join("BugReports"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_documented_section() {
        let config = crate::config::AppConfig::from_value(serde_json::json!({})).unwrap();
        let b = &config.bug_reports;
        assert!(!b.enabled);
        assert_eq!(b.max_full_reports_per_fingerprint, 3);
        assert_eq!(b.nearby_radius, 40.0);
        assert_eq!(b.packet_ring, 64);
        assert_eq!(b.log_ring, 200);
        assert_eq!(b.rate_per_second, 5);
        assert_eq!(b.max_fingerprints, 2_000);
        assert_eq!(b.max_full_reports, 2_000);
        assert_eq!(b.max_total_bytes, 64 * 1024 * 1024);
        assert!(b.directory.is_none());
    }

    #[test]
    fn explicit_values_bind_with_lenient_scalars() {
        let config = crate::config::AppConfig::from_value(serde_json::json!({
            "BugReports": {
                "Enabled": "true",
                "Directory": "D:/reports",
                "MaxFullReportsPerFingerprint": "1",
                "NearbyRadius": "25.5",
                "PacketRing": 8,
                "LogRing": 12,
                "RatePerSecond": "2",
                "MaxFingerprints": "10",
                "MaxFullReports": 5,
                "MaxTotalBytes": "1024"
            }
        }))
        .unwrap();
        let b = &config.bug_reports;
        assert!(b.enabled);
        assert_eq!(b.directory.as_deref(), Some("D:/reports"));
        assert_eq!(b.max_full_reports_per_fingerprint, 1);
        assert_eq!(b.nearby_radius, 25.5);
        assert_eq!(b.packet_ring, 8);
        assert_eq!(b.log_ring, 12);
        assert_eq!(b.rate_per_second, 2);
        assert_eq!(b.max_fingerprints, 10);
        assert_eq!(b.max_full_reports, 5);
        assert_eq!(b.max_total_bytes, 1024);
        assert_eq!(
            b.resolve_directory(Path::new("C:/app")),
            PathBuf::from("D:/reports")
        );
        assert_eq!(
            BugReportsConfig::default().resolve_directory(Path::new("C:/app")),
            PathBuf::from("C:/app/BugReports")
        );
    }
}
