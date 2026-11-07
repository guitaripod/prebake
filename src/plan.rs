use crate::foz;
use crate::steam::{App, Steam, SteamConfig};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

const PIPELINE_DIR: &str = "fozpipelinesv6";
const BUCKET_PREFIX: &str = "steamapprun_pipeline_cache.";
const LOCAL_DB: &str = "steamapp_pipeline_cache.foz";
const SWARM_DB: &str = "steam_pipeline_cache.foz";

/// One `fossilize_replay` invocation as Steam makes it: a directory holding the pipeline
/// databases plus the whitelist and replayer cache that record what was already replayed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Group {
    pub app: u32,
    pub dir: PathBuf,
    pub bucket: Option<String>,
    pub databases: Vec<PathBuf>,
}

/// An installed game together with the replays Steam would run for it.
#[derive(Debug, Clone)]
pub struct AppPlan {
    pub app: App,
    pub cache_dir: PathBuf,
    pub buckets: Vec<String>,
    pub groups: Vec<Group>,
}

/// Size and modification time of a file, used to notice when Steam rewrote a database.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stamp {
    pub path: PathBuf,
    pub len: u64,
    pub modified_ns: u128,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    pub pipelines: usize,
    pub pending: usize,
}

impl Group {
    pub fn whitelist_stem(&self) -> PathBuf {
        self.dir.join("steam_pipeline_cache_whitelist")
    }

    pub fn replay_cache_stem(&self) -> PathBuf {
        self.dir.join("replay_cache")
    }

    pub fn whitelist_base(&self) -> PathBuf {
        self.dir.join("steam_pipeline_cache_whitelist.foz")
    }

    pub fn replay_cache_base(&self, uuid: &str) -> PathBuf {
        self.dir.join(format!("replay_cache.{uuid}.foz"))
    }

    /// Steam whitelists the first database implicitly only when it is the game's own
    /// recording, so downloaded shaders are always validated.
    pub fn implicit_whitelist(&self) -> bool {
        self.databases.first().and_then(|p| p.file_name()).is_some_and(|n| n == LOCAL_DB)
    }

    pub fn stamps(&self) -> Vec<Stamp> {
        self.databases.iter().filter_map(|p| stamp(p)).collect()
    }

    pub fn label(&self) -> String {
        match &self.bucket {
            Some(bucket) => bucket.clone(),
            None => "base".to_string(),
        }
    }

    pub fn pending(&self, uuid: &str) -> Result<Pending> {
        let pipelines = foz::pipeline_keys(&self.databases)?;
        let done = foz::keys_with_fragments(&self.replay_cache_base(uuid))?;
        let pending = pipelines.iter().filter(|key| !done.contains(key)).count();
        Ok(Pending {
            pipelines: pipelines.len(),
            pending,
        })
    }
}

pub fn stamp(path: &Path) -> Option<Stamp> {
    let meta = fs::metadata(path).ok()?;
    let modified_ns = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_nanos();
    Some(Stamp {
        path: path.to_path_buf(),
        len: meta.len(),
        modified_ns,
    })
}

/// Builds the replay plan for every installed game that has Steam pipeline databases,
/// most recently played first.
pub fn build(steam: &Steam, config: &SteamConfig, excluded: &[u32]) -> Result<Vec<AppPlan>> {
    let logged = steam.logged_buckets();
    let mut plans = Vec::new();
    for app in steam.apps()? {
        if excluded.contains(&app.id) {
            continue;
        }
        let cache_dir = steam.shader_cache_dir(&app);
        let buckets = match logged.get(&app.id) {
            Some(buckets) if !buckets.is_empty() => buckets.clone(),
            _ => config.current_buckets.iter().cloned().collect(),
        };
        let groups = groups_in(&cache_dir, app.id, &buckets);
        if !groups.is_empty() {
            plans.push(AppPlan {
                app,
                cache_dir,
                buckets,
                groups,
            });
        }
    }
    let queue_rank: BTreeMap<u32, usize> = config.processing_queue.iter().enumerate().map(|(i, id)| (*id, i)).collect();
    plans.sort_by(|a, b| {
        b.app
            .last_played
            .cmp(&a.app.last_played)
            .then_with(|| {
                queue_rank
                    .get(&a.app.id)
                    .unwrap_or(&usize::MAX)
                    .cmp(queue_rank.get(&b.app.id).unwrap_or(&usize::MAX))
            })
            .then_with(|| a.app.id.cmp(&b.app.id))
    });
    Ok(plans)
}

/// Lists the replays Steam runs for one game: the base directory, then one per current
/// bucket that holds databases. Buckets left over from older Proton or driver versions are
/// skipped because Steam no longer replays them.
pub fn groups_in(cache_dir: &Path, app: u32, buckets: &[String]) -> Vec<Group> {
    let base = cache_dir.join(PIPELINE_DIR);
    let mut groups = Vec::new();
    if let Some(group) = group_at(&base, app, None) {
        groups.push(group);
    }
    for bucket in buckets {
        let dir = base.join(format!("{BUCKET_PREFIX}{bucket}"));
        if let Some(group) = group_at(&dir, app, Some(bucket.clone())) {
            groups.push(group);
        }
    }
    groups
}

fn group_at(dir: &Path, app: u32, bucket: Option<String>) -> Option<Group> {
    let databases: Vec<PathBuf> = [LOCAL_DB, SWARM_DB]
        .iter()
        .map(|name| dir.join(name))
        .filter(|path| fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() > 16))
        .collect();
    (!databases.is_empty()).then(|| Group {
        app,
        dir: dir.to_path_buf(),
        bucket,
        databases,
    })
}

/// The environment Steam gives `fossilize_replay` for a game, so the driver writes its cache
/// exactly where the game will look for it. Implicit Vulkan layers (overlays, recorders) are
/// switched off because the replay must only talk to the driver.
pub fn replay_env(plan_cache_dir: &Path, buckets: &[String], steam: &Steam) -> Vec<(String, String)> {
    let dir = plan_cache_dir.display().to_string();
    let mut read_only = String::from("steam_cache,steam_precompiled");
    for bucket in buckets {
        read_only.push_str(",steam_precompiled_");
        read_only.push_str(bucket);
    }
    let radv = match std::env::var("RADV_DEBUG") {
        Ok(existing) if !existing.is_empty() => format!("{existing},nomemorycache"),
        _ => "nomemorycache".to_string(),
    };
    let mut env = vec![
        ("__GL_SHADER_DISK_CACHE".into(), "1".into()),
        ("__GL_SHADER_DISK_CACHE_PATH".into(), format!("{dir}/nvidiav1")),
        ("__GL_SHADER_DISK_CACHE_APP_NAME".into(), "steamapp_shader_cache".into()),
        (
            "__GL_SHADER_DISK_CACHE_READ_ONLY_APP_NAME".into(),
            "steam_shader_cache;steamapp_merged_shader_cache".into(),
        ),
        ("__GL_SHADER_DISK_CACHE_SIZE".into(), "10737418240".into()),
        ("__GL_SHADER_DISK_CACHE_SKIP_CLEANUP".into(), "1".into()),
        ("MESA_GLSL_CACHE_DIR".into(), dir.clone()),
        ("MESA_SHADER_CACHE_DIR".into(), dir.clone()),
        ("MESA_DISK_CACHE_SINGLE_FILE".into(), "1".into()),
        ("MESA_DISK_CACHE_READ_ONLY_FOZ_DBS".into(), read_only),
        ("MESA_GLSL_CACHE_MAX_SIZE".into(), "5G".into()),
        ("MESA_SHADER_CACHE_MAX_SIZE".into(), "5G".into()),
        ("AMD_VK_PIPELINE_CACHE_FILENAME".into(), "steamapp_shader_cache".into()),
        ("AMD_VK_PIPELINE_CACHE_PATH".into(), format!("{dir}/AMDv1")),
        ("AMD_VK_USE_PIPELINE_CACHE".into(), "1".into()),
        ("DXVK_STATE_CACHE_PATH".into(), format!("{dir}/DXVK_state_cache")),
        ("RADV_DEBUG".into(), radv),
        ("VK_LOADER_LAYERS_DISABLE".into(), "~implicit~".into()),
        ("DISABLE_MANGOHUD".into(), "1".into()),
    ];
    if let Some(filters) = steam.engine_filters() {
        env.push(("FOSSILIZE_APPLICATION_INFO_FILTER_PATH".into(), filters.display().to_string()));
    }
    env
}

/// Creates the driver cache directories Steam creates before it launches a game. The NVIDIA
/// driver silently skips caching when `__GL_SHADER_DISK_CACHE_PATH` does not exist, so a
/// game Steam never started on this machine would otherwise be replayed for nothing.
pub fn prepare_cache_dirs(cache_dir: &Path) -> std::io::Result<()> {
    for name in ["nvidiav1", "AMDv1"] {
        fs::create_dir_all(cache_dir.join(name))?;
    }
    Ok(())
}

/// Variables that would stop the driver from writing a cache or inject extra layers.
pub const SCRUBBED_ENV: &[&str] = &[
    "MESA_SHADER_CACHE_DISABLE",
    "MESA_GLSL_CACHE_DISABLE",
    "VK_INSTANCE_LAYERS",
    "VK_LOADER_LAYERS_ENABLE",
    "ENABLE_VK_LAYER_VALVE_steam_fossilize_1",
    "ENABLE_VK_LAYER_VALVE_steam_overlay_1",
    "LD_PRELOAD",
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::foz::testing::write_archive;

    #[test]
    fn finds_base_and_current_bucket_groups_only() {
        let dir = tempfile::tempdir().unwrap();
        let pipelines = dir.path().join(PIPELINE_DIR);
        let current = pipelines.join("steamapprun_pipeline_cache.3554f158047e28bf");
        let stale = pipelines.join("steamapprun_pipeline_cache.f8e37139bec012fd");
        let empty = pipelines.join("steamapprun_pipeline_cache.923fa87291dfec1c");
        for d in [&current, &stale, &empty] {
            fs::create_dir_all(d).unwrap();
        }
        write_archive(&pipelines.join(SWARM_DB), &[(6, 1, b"")]);
        write_archive(&current.join(LOCAL_DB), &[(6, 2, b"")]);
        write_archive(&current.join(SWARM_DB), &[(6, 3, b"")]);
        write_archive(&stale.join(SWARM_DB), &[(6, 4, b"")]);
        fs::write(empty.join("TOUCH"), b"").unwrap();
        fs::write(empty.join(SWARM_DB), b"").unwrap();

        let buckets = vec!["923fa87291dfec1c".to_string(), "3554f158047e28bf".to_string()];
        let groups = groups_in(dir.path(), 7, &buckets);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].dir, pipelines);
        assert_eq!(groups[0].databases, vec![pipelines.join(SWARM_DB)]);
        assert!(!groups[0].implicit_whitelist());
        assert_eq!(groups[1].bucket.as_deref(), Some("3554f158047e28bf"));
        assert_eq!(groups[1].databases, vec![current.join(LOCAL_DB), current.join(SWARM_DB)]);
        assert!(groups[1].implicit_whitelist());
    }

    #[test]
    fn prepares_the_driver_cache_directories_steam_would_create() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("shadercache/287390");
        prepare_cache_dirs(&cache).unwrap();
        assert!(cache.join("nvidiav1").is_dir());
        assert!(cache.join("AMDv1").is_dir());
        prepare_cache_dirs(&cache).unwrap();
    }

    #[test]
    fn counts_pipelines_missing_from_the_replay_cache_and_its_fragments() {
        let dir = tempfile::tempdir().unwrap();
        let group_dir = dir.path().join(PIPELINE_DIR);
        fs::create_dir_all(&group_dir).unwrap();
        write_archive(
            &group_dir.join(SWARM_DB),
            &[(4, 9, b"module"), (6, 1, b""), (6, 2, b""), (7, 3, b""), (9, 4, b"")],
        );
        write_archive(&group_dir.join("replay_cache.abcd.foz"), &[(6, 1, b"")]);
        write_archive(&group_dir.join("replay_cache.abcd.3.foz"), &[(7, 3, b"")]);
        write_archive(&group_dir.join("replay_cache.ffff.foz"), &[(6, 2, b"")]);
        let group = &groups_in(dir.path(), 1, &[])[0];
        assert_eq!(group.pending("abcd").unwrap(), Pending { pipelines: 4, pending: 2 });
        assert_eq!(group.pending("ffff").unwrap(), Pending { pipelines: 4, pending: 3 });
        assert_eq!(group.pending("0000").unwrap(), Pending { pipelines: 4, pending: 4 });
    }
}
