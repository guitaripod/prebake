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
    /// Count apps that keep the screen awake (a browser playing music or video, a call) as you
    /// being present. Off by default: away means no keyboard or mouse input, and a tab left
    /// playing would otherwise hold baking off for hours. Needs a compositor with
    /// ext-idle-notify-v1 version 2 (KDE Plasma 6.7+, sway 1.11+, Hyprland, niri, COSMIC);
    /// older ones always respect inhibitors.
    pub respect_inhibitors: bool,
    /// Keep baking on battery power.
    pub run_on_battery: bool,
    /// Tell logind your session is idle while you are away and no game runs, so tools that
    /// wait for an idle session (SCSKiller's "When idle", logind's `IdleAction`) wait for you
    /// too on desktops that never set logind's idle hint, such as KDE Plasma. For them an app
    /// keeping the screen awake (a film, a call) counts as you being present, for up to three
    /// hours without input.
    pub idle_hint: bool,
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
            respect_inhibitors: false,
            run_on_battery: false,
            idle_hint: false,
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

/// Memory a replay worker is assumed to need until a running replay shows what it takes.
/// Large games measure about 1.2 GiB per worker (Diablo IV on NVIDIA).
pub const WORKER_MEMORY: u64 = 1 << 30;
const MIN_RESERVE: u64 = 4 << 30;

/// What a replay may use: the memory the kernel can hand out minus a reserve for the desktop,
/// and no more than the limit on prebake's own cgroup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryBudget {
    pub available: Option<u64>,
    pub reserve: u64,
    pub ceiling: Option<u64>,
}

impl MemoryBudget {
    pub fn read() -> Self {
        Self {
            available: meminfo("MemAvailable:"),
            reserve: (total_memory_bytes() / 8).max(MIN_RESERVE),
            ceiling: cgroup_ceiling(),
        }
    }

    /// How many workers fit. `held` is memory the running replay already uses, which would come
    /// back if it were restarted; `per_worker` is what one worker needs. A tenth of the cgroup
    /// limit stays free for the databases Fossilize reads and for prebake itself.
    pub fn workers(&self, held: u64, per_worker: u64) -> u32 {
        let free = self.available.map(|available| (available + held).saturating_sub(self.reserve));
        let limited = self.ceiling.map(|ceiling| ceiling - ceiling / 10);
        let budget = match (free, limited) {
            (Some(free), Some(limited)) => free.min(limited),
            (Some(budget), None) | (None, Some(budget)) => budget,
            (None, None) => return u32::MAX,
        };
        (budget / per_worker.max(1)).clamp(1, u64::from(u32::MAX)) as u32
    }
}

/// The lower of `memory.high` and `memory.max` on prebake's own cgroup, when either is set.
fn cgroup_ceiling() -> Option<u64> {
    let membership = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let path = membership.lines().find_map(|l| l.strip_prefix("0::"))?;
    let dir = Path::new("/sys/fs/cgroup").join(path.trim_start_matches('/'));
    ["memory.high", "memory.max"]
        .iter()
        .filter_map(|file| std::fs::read_to_string(dir.join(file)).ok()?.trim().parse::<u64>().ok())
        .min()
}

fn meminfo(field: &str) -> Option<u64> {
    std::fs::read_to_string("/proc/meminfo").ok().and_then(|text| {
        text.lines()
            .find_map(|l| l.strip_prefix(field))
            .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
            .map(|kib| kib * 1024)
    })
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

# Treat apps that keep the screen awake (music or video in a browser, calls) as you being present.
respect_inhibitors = false

# Keep baking on battery power.
run_on_battery = false

# Tell logind the session is idle while you are away and no game runs, for other tools that wait for an idle session.
idle_hint = false

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
    fn memory_budget_keeps_a_reserve_and_counts_memory_the_replay_holds() {
        let gib = 1u64 << 30;
        let budget = |available, ceiling| MemoryBudget {
            available,
            reserve: 4 * gib,
            ceiling,
        };
        assert_eq!(budget(None, None).workers(0, gib), u32::MAX);
        assert_eq!(budget(Some(2 * gib), None).workers(0, gib), 1);
        assert_eq!(budget(Some(14 * gib), None).workers(0, gib), 10);
        assert_eq!(budget(Some(4 * gib), None).workers(10 * gib, gib), 10);
        assert_eq!(budget(Some(4 * gib), None).workers(10 * gib, 2 * gib), 5);
    }

    #[test]
    fn memory_budget_stays_inside_the_cgroup_limit() {
        let gib = 1u64 << 30;
        let budget = MemoryBudget {
            available: Some(64 * gib),
            reserve: 4 * gib,
            ceiling: Some(20 * gib),
        };
        assert_eq!(budget.workers(0, gib), 18);
        let unknown = MemoryBudget { available: None, ..budget };
        assert_eq!(unknown.workers(0, 2 * gib), 9);
        assert!(MemoryBudget::read().reserve >= MIN_RESERVE);
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
