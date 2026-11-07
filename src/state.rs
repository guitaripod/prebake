use crate::gpu::Device;
use crate::plan::{Group, Pending, Stamp, stamp};
use crate::replay::Progress;
use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Where prebake keeps its config and state. The live status goes to the runtime
/// directory (memory-backed), since the service rewrites it every few seconds.
#[derive(Debug, Clone)]
pub struct Dirs {
    pub config: PathBuf,
    pub state: PathBuf,
    pub runtime: PathBuf,
}

impl Dirs {
    pub fn new() -> Result<Self> {
        let base = directories::BaseDirs::new().context("finding the home directory")?;
        let state = base
            .state_dir()
            .map_or_else(|| base.home_dir().join(".local/state"), Path::to_path_buf);
        let state = state.join("prebake");
        let runtime = std::env::var_os("XDG_RUNTIME_DIR").map_or_else(|| state.clone(), |dir| PathBuf::from(dir).join("prebake"));
        Ok(Self {
            config: base.config_dir().join("prebake/config.toml"),
            state,
            runtime,
        })
    }

    pub fn records(&self) -> PathBuf {
        self.state.join("records.json")
    }

    pub fn status(&self) -> PathBuf {
        self.runtime.join("status.json")
    }

    pub fn control(&self) -> PathBuf {
        self.state.join("control.json")
    }
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

pub fn read_json<T: DeserializeOwned + Default>(path: &Path) -> T {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(value)?).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

/// What prebake remembers about each group between runs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Records {
    pub groups: BTreeMap<PathBuf, Record>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Record {
    pub uuid: String,
    pub stamps: Vec<Stamp>,
    pub finished: Option<u64>,
    pub pending: Option<Pending>,
    pub pending_key: Vec<Stamp>,
    pub failures: u32,
    pub retry_after: u64,
}

impl Records {
    /// True when the group was replayed to the end on this driver and Steam has not changed
    /// its databases since.
    pub fn done(&self, group: &Group, uuid: &str) -> bool {
        self.groups
            .get(&group.dir)
            .is_some_and(|r| r.finished.is_some() && r.uuid == uuid && r.stamps == group.stamps())
    }

    pub fn backing_off(&self, group: &Group, now: u64) -> bool {
        self.groups.get(&group.dir).is_some_and(|r| r.retry_after > now)
    }

    pub fn finished(&mut self, group: &Group, uuid: &str, now: u64) {
        let record = self.groups.entry(group.dir.clone()).or_default();
        record.uuid = uuid.to_string();
        record.stamps = group.stamps();
        record.finished = Some(now);
        record.failures = 0;
        record.retry_after = 0;
    }

    /// Notes a failed replay and backs off: an hour after the first failure, doubling up to a day.
    pub fn failed(&mut self, group: &Group, now: u64) -> u64 {
        let record = self.groups.entry(group.dir.clone()).or_default();
        record.failures += 1;
        let delay = (3600u64 << (record.failures - 1).min(5)).min(86_400);
        record.retry_after = now + delay;
        delay
    }

    /// The cached pending count, if nothing it depends on has changed.
    pub fn cached_pending(&self, group: &Group, uuid: &str) -> Option<Pending> {
        let record = self.groups.get(&group.dir)?;
        (record.uuid == uuid && record.pending_key == pending_key(group, uuid)).then_some(record.pending?)
    }

    pub fn store_pending(&mut self, group: &Group, uuid: &str, pending: Pending) {
        let key = pending_key(group, uuid);
        let record = self.groups.entry(group.dir.clone()).or_default();
        if record.uuid != uuid {
            record.finished = None;
        }
        record.uuid = uuid.to_string();
        record.pending = Some(pending);
        record.pending_key = key;
    }

    /// Drops records for groups that no longer exist on disk.
    pub fn prune(&mut self) {
        self.groups.retain(|dir, _| dir.is_dir());
    }
}

fn pending_key(group: &Group, uuid: &str) -> Vec<Stamp> {
    let base = group.replay_cache_base(uuid);
    let mut key = group.stamps();
    key.extend(stamp(&base));
    key.extend(crate::foz::fragments(&base).unwrap_or_default().iter().filter_map(|p| stamp(p)));
    key
}

/// Requests from the command line to the daemon.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Control {
    pub paused_until: Option<u64>,
    pub boost_until: Option<u64>,
}

impl Control {
    pub fn paused(&self, now: u64) -> bool {
        self.paused_until.is_some_and(|until| until > now)
    }

    pub fn boosted(&self, now: u64) -> bool {
        self.boost_until.is_some_and(|until| until > now)
    }
}

/// The daemon's view of the world, written every few seconds for `prebake status`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Status {
    pub pid: u32,
    pub version: String,
    pub updated: u64,
    pub activity: String,
    pub reason: String,
    pub presence: String,
    pub idle_source: String,
    pub device: Option<Device>,
    pub steam_background_processing: bool,
    pub job: Option<JobStatus>,
    pub apps: Vec<AppStatus>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JobStatus {
    pub app: u32,
    pub name: String,
    pub group: String,
    pub group_index: usize,
    pub groups: usize,
    pub workers: u32,
    pub running: u32,
    pub started: u64,
    pub progress: Progress,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AppStatus {
    pub app: u32,
    pub name: String,
    pub state: AppState,
    pub pipelines: Option<usize>,
    pub pending: Option<usize>,
    pub last_played: u64,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AppState {
    Ready,
    #[default]
    Waiting,
    Baking,
    Steam,
    Failed,
}

impl Status {
    /// The daemon wrote this within the last 30 seconds and is still running.
    pub fn live(&self, now: u64) -> bool {
        self.pid != 0 && now.saturating_sub(self.updated) <= 30 && Path::new(&format!("/proc/{}", self.pid)).exists()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::foz::testing::write_archive;

    fn group(dir: &Path) -> Group {
        fs::create_dir_all(dir).unwrap();
        let db = dir.join("steam_pipeline_cache.foz");
        write_archive(&db, &[(6, 1, b"")]);
        Group {
            app: 1,
            dir: dir.to_path_buf(),
            bucket: None,
            databases: vec![db],
        }
    }

    #[test]
    fn a_group_is_done_until_the_driver_or_its_databases_change() {
        let tmp = tempfile::tempdir().unwrap();
        let group = group(&tmp.path().join("g"));
        let mut records = Records::default();
        assert!(!records.done(&group, "uuid-a"));
        records.finished(&group, "uuid-a", 100);
        assert!(records.done(&group, "uuid-a"));
        assert!(!records.done(&group, "uuid-b"));
        write_archive(&group.databases[0], &[(6, 1, b""), (6, 2, b"")]);
        assert!(!records.done(&group, "uuid-a"));
    }

    #[test]
    fn failures_back_off_exponentially_up_to_a_day() {
        let tmp = tempfile::tempdir().unwrap();
        let group = group(&tmp.path().join("g"));
        let mut records = Records::default();
        assert_eq!(records.failed(&group, 0), 3600);
        assert!(records.backing_off(&group, 3599));
        assert!(!records.backing_off(&group, 3600));
        assert_eq!(records.failed(&group, 0), 7200);
        for _ in 0..10 {
            records.failed(&group, 0);
        }
        assert_eq!(records.groups[&group.dir].retry_after, 86_400);
        records.finished(&group, "u", 5);
        assert!(!records.backing_off(&group, 0));
    }

    #[test]
    fn cached_pending_is_invalidated_by_new_replay_fragments() {
        let tmp = tempfile::tempdir().unwrap();
        let group = group(&tmp.path().join("g"));
        let mut records = Records::default();
        let pending = Pending { pipelines: 1, pending: 1 };
        records.store_pending(&group, "u", pending);
        assert_eq!(records.cached_pending(&group, "u"), Some(pending));
        assert_eq!(records.cached_pending(&group, "v"), None);
        write_archive(&group.dir.join("replay_cache.u.1.foz"), &[(6, 1, b"")]);
        assert_eq!(records.cached_pending(&group, "u"), None);
    }

    #[test]
    fn control_requests_expire() {
        let control = Control {
            paused_until: Some(10),
            boost_until: Some(20),
        };
        assert!(control.paused(9));
        assert!(!control.paused(10));
        assert!(control.boosted(19));
        assert!(!Control::default().paused(0));
    }
}
