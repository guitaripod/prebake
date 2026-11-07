use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// `~/.config/prebake/config.toml`. Every field is optional.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// How long without keyboard or mouse input counts as being away.
    #[serde(with = "humantime_serde_compat")]
    pub idle_after: Duration,
    /// Replay workers while you are away. 0 picks one per CPU thread, minus one, within memory.
    pub workers_away: u32,
    /// Replay workers while you are at the computer. 0 pauses until you leave.
    pub workers_present: u32,
    /// Count apps that block the screensaver (video players, browsers playing media) as you
    /// being present.
    pub respect_inhibitors: bool,
    /// Keep baking on battery power.
    pub run_on_battery: bool,
    /// Steam installation to use instead of the one found automatically.
    pub steam_root: Option<PathBuf>,
    /// Vulkan device index to replay on instead of the first discrete GPU.
    pub device_index: Option<u32>,
    /// Seconds a single pipeline may take before Fossilize gives up on it.
    pub pipeline_timeout: u32,
    /// Steam app ids prebake leaves alone.
    pub exclude: Vec<u32>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            idle_after: Duration::from_secs(5 * 60),
            workers_away: 0,
            workers_present: 0,
            respect_inhibitors: true,
            run_on_battery: false,
            steam_root: None,
            device_index: None,
            pipeline_timeout: 60,
            exclude: Vec::new(),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("reading {}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Workers to use while away: the configured count, or one per CPU thread minus one,
    /// capped at one per 2 GiB of RAM so a big replay cannot push the desktop into swap.
    pub fn away_workers(&self) -> u32 {
        if self.workers_away > 0 {
            return self.workers_away;
        }
        let cpus = std::thread::available_parallelism().map_or(2, |n| n.get() as u32);
        let by_memory = (total_memory_bytes() / (2 << 30)) as u32;
        cpus.saturating_sub(1).min(by_memory).max(1)
    }
}

fn total_memory_bytes() -> u64 {
    meminfo("MemTotal:").unwrap_or(8 << 30)
}

/// Bytes the kernel reports as available to new work.
pub fn available_memory_bytes() -> Option<u64> {
    meminfo("MemAvailable:")
}

fn meminfo(field: &str) -> Option<u64> {
    std::fs::read_to_string("/proc/meminfo").ok().and_then(|text| {
        text.lines()
            .find_map(|l| l.strip_prefix(field))
            .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
            .map(|kib| kib * 1024)
    })
}

/// Memory a replay worker is assumed to need, driver included.
pub const WORKER_MEMORY: u64 = 400 << 20;
const MEMORY_RESERVE: u64 = 4 << 30;

/// How many workers fit in memory while keeping a reserve for the desktop. `held` is memory
/// the running replay already uses, which would come back if it were restarted.
pub fn memory_cap(available: Option<u64>, held: u64) -> u32 {
    match available {
        Some(available) => ((available + held).saturating_sub(MEMORY_RESERVE) / WORKER_MEMORY).clamp(1, u64::from(u32::MAX)) as u32,
        None => u32::MAX,
    }
}

mod humantime_serde_compat {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(value: &Duration, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&humantime::format_duration(*value).to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
        let text = String::deserialize(deserializer)?;
        humantime::parse_duration(&text).map_err(serde::de::Error::custom)
    }
}

pub const EXAMPLE: &str = r#"# prebake configuration. Every setting is optional; these are the defaults.

# How long without keyboard or mouse input counts as being away.
idle_after = "5m"

# Replay workers while you are away. 0 = one per CPU thread minus one, within memory.
workers_away = 0

# Replay workers while you are at the computer. 0 = wait until you leave.
workers_present = 0

# Treat apps that keep the screen awake (video, calls) as you being present.
respect_inhibitors = true

# Keep baking on battery power.
run_on_battery = false

# Seconds one pipeline may take before Fossilize skips it.
pipeline_timeout = 60

# Steam app ids to leave alone.
exclude = []

# steam_root = "/home/you/.local/share/Steam"
# device_index = 0
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_example_is_the_defaults() {
        let parsed: Config = toml::from_str(EXAMPLE).unwrap();
        assert_eq!(parsed, Config::default());
    }

    #[test]
    fn parses_durations_and_rejects_unknown_keys() {
        let parsed: Config = toml::from_str("idle_after = \"90s\"\nworkers_present = 2\nexclude = [730]\n").unwrap();
        assert_eq!(parsed.idle_after, Duration::from_secs(90));
        assert_eq!(parsed.workers_present, 2);
        assert_eq!(parsed.exclude, vec![730]);
        assert!(toml::from_str::<Config>("idle = \"5m\"\n").is_err());
    }

    #[test]
    fn memory_cap_keeps_a_reserve_and_counts_memory_the_replay_holds() {
        assert_eq!(memory_cap(None, 0), u32::MAX);
        assert_eq!(memory_cap(Some(2 << 30), 0), 1);
        assert_eq!(memory_cap(Some((4 << 30) + 10 * WORKER_MEMORY), 0), 10);
        assert_eq!(memory_cap(Some(4 << 30), 10 * WORKER_MEMORY), 10);
    }

    #[test]
    fn away_workers_respects_an_explicit_count() {
        let config = Config {
            workers_away: 6,
            ..Config::default()
        };
        assert_eq!(config.away_workers(), 6);
        assert!(Config::default().away_workers() >= 1);
    }
}
