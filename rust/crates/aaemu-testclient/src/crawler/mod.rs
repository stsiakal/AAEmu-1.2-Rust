//! Quest crawler: build a catalog from the 1.2 game data and play every quest of one race
//! through real client packets, using GM commands only for setup (level, teleport, items and a
//! missing NPC/doodad spawn). See [`crate::crawler`].

pub mod catalog;
pub mod dashboard;
pub mod runner;

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context};

/// One of the four playable 1.2 races.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Race {
    Nuian,
    Elf,
    Firran,
    Harani,
}

impl Race {
    /// `characters.char_race_id` of the race (the 1.2 client's race byte).
    pub fn id(self) -> u8 {
        match self {
            Race::Nuian => 1,
            Race::Elf => 4,
            Race::Firran => 6,
            Race::Harani => 5,
        }
    }

    /// The character's faction (`characters.faction_id`).
    pub fn faction_id(self) -> u32 {
        match self {
            Race::Nuian => 101,
            Race::Elf => 103,
            Race::Firran => 113,
            Race::Harani => 109,
        }
    }

    /// The mother faction (`system_factions.mother_id`): 148 Nuia, 149 Haranya.
    pub fn mother_faction_id(self) -> u32 {
        match self {
            Race::Nuian | Race::Elf => 148,
            Race::Firran | Race::Harani => 149,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Race::Nuian => "nuian",
            Race::Elf => "elf",
            Race::Firran => "firran",
            Race::Harani => "harani",
        }
    }

    pub fn character_name(self) -> &'static str {
        match self {
            Race::Nuian => "CrawlerNuian",
            Race::Elf => "CrawlerElf",
            Race::Firran => "CrawlerFirran",
            Race::Harani => "CrawlerHarani",
        }
    }
}

impl std::str::FromStr for Race {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        Ok(match s.trim().to_ascii_lowercase().as_str() {
            "nuian" => Race::Nuian,
            "elf" => Race::Elf,
            "firran" | "ferre" => Race::Firran,
            "harani" | "hariharan" => Race::Harani,
            other => bail!("unknown race '{other}' (nuian, elf, firran, harani)"),
        })
    }
}

/// Settings of one crawler process.
#[derive(Clone, Debug)]
pub struct CrawlConfig {
    pub race: Race,
    pub login: String,
    pub user: String,
    pub password: String,
    pub character: String,
    pub out: PathBuf,
    /// `(min, max)` quest levels.
    pub levels: (u8, u8),
    pub from_quest: Option<u32>,
    pub limit: Option<usize>,
    pub compact: PathBuf,
    pub data_dir: PathBuf,
    /// Game server config, used to grant the crawler account GM access (optional).
    pub game_config: Option<PathBuf>,
    pub mysql_bin: Option<PathBuf>,
    pub server_log: Option<PathBuf>,
    pub bugreports: Option<PathBuf>,
    pub quest_timeout: Duration,
    pub objective_timeout: Duration,
    /// Level the crawler raises the character to before each quest (`0` = the quest level).
    pub min_level: u8,
    /// Do not update the account's access level through MySQL.
    pub no_grant_gm: bool,
}

impl CrawlConfig {
    pub fn defaults() -> Self {
        Self {
            race: Race::Nuian,
            login: "127.0.0.1:1237".into(),
            user: "crawl-nuian".into(),
            password: "testpass".into(),
            character: Race::Nuian.character_name().into(),
            out: PathBuf::from("crawl"),
            levels: (1, 15),
            from_quest: None,
            limit: None,
            compact: PathBuf::from("rust/crates/aaemu-game/app/Data/compact.sqlite3"),
            data_dir: PathBuf::from("rust/crates/aaemu-game/app/Data"),
            game_config: None,
            mysql_bin: None,
            server_log: None,
            bugreports: None,
            quest_timeout: Duration::from_secs(180),
            objective_timeout: Duration::from_secs(45),
            min_level: 0,
            no_grant_gm: false,
        }
    }

    /// The per-race JSONL result file.
    pub fn result_path(&self) -> PathBuf {
        self.out.join(format!("{}.jsonl", self.race.name()))
    }

    pub fn catalog_path(&self) -> PathBuf {
        self.out.join("catalog.json")
    }
}

/// Runs one crawler process.
pub async fn run(cfg: CrawlConfig) -> anyhow::Result<()> {
    std::fs::create_dir_all(&cfg.out)
        .with_context(|| format!("creating output directory {}", cfg.out.display()))?;
    let catalog = catalog::Catalog::load(&cfg.compact, &cfg.data_dir)?;
    let path = cfg.catalog_path();
    catalog.write(&path)?;
    println!(
        "[crawl] catalog: {} quests, {} npcs, {} doodads -> {}",
        catalog.quests.len(),
        catalog.npc_spawns.len(),
        catalog.doodad_spawns.len(),
        path.display()
    );
    let summary = catalog.summary(&cfg);
    println!(
        "[crawl] {}: {} quests in levels {}-{} ({} with objectives unsupported)",
        cfg.race.name(),
        summary.playable,
        cfg.levels.0,
        cfg.levels.1,
        summary.unsupported
    );
    runner::run(&cfg, &catalog).await?;
    dashboard::write(&cfg.out)?;
    Ok(())
}

/// Rewrites the dashboard from every `*.jsonl` result file in `out`.
pub fn dashboard_only(out: &Path) -> anyhow::Result<()> {
    dashboard::write(out)
}
