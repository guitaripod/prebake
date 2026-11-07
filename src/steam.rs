use crate::vdf::{Document, Value};
use anyhow::{Context, Result, bail};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

const STEAM_SECTION: [&str; 4] = ["InstallConfigStore", "Software", "Valve", "Steam"];
const STATE_FULLY_INSTALLED: u64 = 4;

/// A native (non-Flatpak) Steam installation and the libraries it knows about.
#[derive(Debug, Clone)]
pub struct Steam {
    pub root: PathBuf,
    pub libraries: Vec<PathBuf>,
}

/// An installed Steam game.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    pub id: u32,
    pub name: String,
    pub library: PathBuf,
    pub last_played: u64,
}

/// The parts of Steam's `config/config.vdf` that decide what Steam replays.
#[derive(Debug, Clone, Default)]
pub struct SteamConfig {
    pub processing_queue: Vec<u32>,
    pub current_buckets: BTreeSet<String>,
    pub background_processing: bool,
}

impl Steam {
    pub fn locate(configured: Option<&Path>) -> Result<Self> {
        let root = match configured {
            Some(path) => {
                if !is_steam_root(path) {
                    bail!("{} is not a Steam installation (no steamapps/ or ubuntu12_64/)", path.display());
                }
                path.canonicalize().with_context(|| format!("resolving {}", path.display()))?
            }
            None => find_root()
                .context("no native Steam installation found in ~/.local/share/Steam or ~/.steam/steam (set steam_root in the config)")?,
        };
        let libraries = read_libraries(&root)?;
        Ok(Self { root, libraries })
    }

    pub fn fossilize_replay(&self) -> PathBuf {
        self.root.join("ubuntu12_64/fossilize_replay")
    }

    pub fn engine_filters(&self) -> Option<PathBuf> {
        let path = self.root.join("fossilize_engine_filters.json");
        path.is_file().then_some(path)
    }

    pub fn config_path(&self) -> PathBuf {
        self.root.join("config/config.vdf")
    }

    pub fn dev_config_path(&self) -> PathBuf {
        self.root.join("steam_dev.cfg")
    }

    pub fn shader_cache_dir(&self, app: &App) -> PathBuf {
        app.library.join("steamapps/shadercache").join(app.id.to_string())
    }

    pub fn apps(&self) -> Result<Vec<App>> {
        let mut apps = BTreeMap::new();
        for library in &self.libraries {
            let steamapps = library.join("steamapps");
            let Ok(entries) = fs::read_dir(&steamapps) else { continue };
            for entry in entries.flatten() {
                let name = entry.file_name();
                let Some(name) = name.to_str() else { continue };
                if !(name.starts_with("appmanifest_") && name.ends_with(".acf")) {
                    continue;
                }
                match read_manifest(&entry.path(), library) {
                    Ok(Some(app)) => {
                        apps.insert(app.id, app);
                    }
                    Ok(None) => {}
                    Err(err) => tracing::debug!("skipping {}: {err:#}", entry.path().display()),
                }
            }
        }
        Ok(apps.into_values().collect())
    }

    pub fn config(&self) -> Result<SteamConfig> {
        let path = self.config_path();
        let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        parse_config(&text)
    }

    /// Reads the buckets Steam last chose for each app from `logs/shader_log.txt` (and the
    /// previous log), which is the only place Steam records that per-app decision.
    pub fn logged_buckets(&self) -> BTreeMap<u32, Vec<String>> {
        let mut found = BTreeMap::new();
        for name in ["shader_log.previous.txt", "shader_log.txt"] {
            if let Ok(text) = fs::read_to_string(self.root.join("logs").join(name)) {
                parse_bucket_log(&text, &mut found);
            }
        }
        found
    }
}

fn find_root() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    [home.join(".local/share/Steam"), home.join(".steam/steam"), home.join(".steam/root")]
        .into_iter()
        .filter_map(|path| path.canonicalize().ok())
        .find(|path| is_steam_root(path))
}

fn is_steam_root(path: &Path) -> bool {
    path.join("steamapps").is_dir() && path.join("ubuntu12_64").is_dir()
}

/// Returns true when only a Flatpak Steam is present, which prebake does not drive because
/// its games see the Flatpak runtime's graphics driver rather than the host's.
pub fn flatpak_only() -> bool {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return false;
    };
    find_root().is_none() && home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam").is_dir()
}

fn read_libraries(root: &Path) -> Result<Vec<PathBuf>> {
    let mut libraries = vec![root.to_path_buf()];
    let path = root.join("steamapps/libraryfolders.vdf");
    if let Ok(text) = fs::read_to_string(&path) {
        let doc = Document::parse(&text).with_context(|| format!("parsing {}", path.display()))?;
        let folders = doc.path(&["libraryfolders"]).or_else(|| doc.path(&["LibraryFolders"]));
        for pair in folders.map(Value::pairs).unwrap_or_default() {
            let library = match &pair.value {
                Value::Str { text, .. } if pair.key.parse::<u32>().is_ok() => Some(text.as_str()),
                Value::Obj { .. } => pair.value.get("path").and_then(Value::as_str),
                Value::Str { .. } => None,
            };
            if let Some(library) = library {
                let library = PathBuf::from(library);
                let resolved = library.canonicalize().unwrap_or(library);
                if resolved.join("steamapps").is_dir() && !libraries.contains(&resolved) {
                    libraries.push(resolved);
                }
            }
        }
    }
    Ok(libraries)
}

fn read_manifest(path: &Path, library: &Path) -> Result<Option<App>> {
    let text = fs::read_to_string(path)?;
    let doc = Document::parse(&text)?;
    let field = |key: &str| doc.path(&["AppState", key]).and_then(Value::as_str);
    let Some(id) = field("appid").and_then(|id| id.parse().ok()) else {
        return Ok(None);
    };
    let flags: u64 = field("StateFlags").and_then(|f| f.parse().ok()).unwrap_or(0);
    if flags & STATE_FULLY_INSTALLED == 0 {
        return Ok(None);
    }
    Ok(Some(App {
        id,
        name: field("name").unwrap_or("").to_string(),
        library: library.to_path_buf(),
        last_played: field("LastPlayed").and_then(|t| t.parse().ok()).unwrap_or(0),
    }))
}

pub fn parse_config(text: &str) -> Result<SteamConfig> {
    let doc = Document::parse(text).context("parsing config.vdf")?;
    let Some(steam) = doc.path(&STEAM_SECTION) else {
        return Ok(SteamConfig::default());
    };
    let manager = steam.get("ShaderCacheManager");
    let processing_queue = manager
        .and_then(|m| m.get("ProcessingQueue"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .split(';')
        .filter_map(|id| id.trim().parse().ok())
        .collect();
    let mut current_buckets = BTreeSet::new();
    for pair in manager
        .and_then(|m| m.get("CompatToolBucketHashes"))
        .map(Value::pairs)
        .unwrap_or_default()
    {
        for part in pair.value.as_str().unwrap_or("").split(';') {
            if let Some(bucket) = pipeline_bucket(part) {
                current_buckets.insert(bucket);
            }
        }
    }
    let background_processing = manager
        .and_then(|m| m.get("EnableShaderBackgroundProcessing"))
        .and_then(Value::as_str)
        .is_some_and(|v| v.trim() == "1");
    Ok(SteamConfig {
        processing_queue,
        current_buckets,
        background_processing,
    })
}

/// Extracts the bucket id from `G7:VulkanPipelinesV6_<id>` or `SteamSwarm / G7:VulkanPipelinesV6_<id>`.
fn pipeline_bucket(text: &str) -> Option<String> {
    let (_, id) = text.trim().split_once("VulkanPipelinesV6_")?;
    let id = id.trim();
    (id.len() == 16 && id.bytes().all(|b| b.is_ascii_hexdigit())).then(|| id.to_ascii_lowercase())
}

pub fn parse_bucket_log(text: &str, found: &mut BTreeMap<u32, Vec<String>>) {
    let mut lines = text.lines().map(strip_timestamp).peekable();
    while let Some(line) = lines.next() {
        let Some(rest) = line.strip_prefix("Found ") else { continue };
        let Some((count, rest)) = rest.split_once(" buckets for AppId ") else {
            continue;
        };
        let Some((app, _)) = rest.split_once(' ') else { continue };
        let (Ok(count), Ok(app)) = (count.parse::<usize>(), app.parse::<u32>()) else {
            continue;
        };
        let mut buckets = Vec::new();
        for _ in 0..count {
            match lines.peek().map(|l| l.starts_with("SteamSwarm / ")) {
                Some(true) => {
                    if let Some(bucket) = lines.next().and_then(pipeline_bucket) {
                        buckets.push(bucket);
                    }
                }
                _ => break,
            }
        }
        found.insert(app, buckets);
    }
}

fn strip_timestamp(line: &str) -> &str {
    match line.strip_prefix('[').and_then(|rest| rest.split_once("] ")) {
        Some((_, message)) => message,
        None => line,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_queue_buckets_and_background_flag() {
        let text = "\"InstallConfigStore\"\n{\n\t\"Software\"\n\t{\n\t\t\"Valve\"\n\t\t{\n\t\t\t\"Steam\"\n\t\t\t{\n\t\t\t\t\"ShaderCacheManager\"\n\t\t\t\t{\n\t\t\t\t\t\"ProcessingQueue\"\t\t\"553850;2322010;\"\n\t\t\t\t\t\"EnableShaderBackgroundProcessing\"\t\t\"1\"\n\t\t\t\t\t\"CompatToolBucketHashes\"\n\t\t\t\t\t{\n\t\t\t\t\t\t\"961940\"\t\t\"3823376;\"\n\t\t\t\t\t\t\"1493710\"\t\t\"26306234;G7:VulkanPipelinesV6_923fa87291dfec1c;G7:VulkanPipelinesV6_3554f158047e28bf\"\n\t\t\t\t\t\t\"0\"\t\t\"0;G7:VulkanPipelinesV6_2e6c105801134e9c;G7:VulkanPipelinesV6_923fa87291dfec1c\"\n\t\t\t\t\t}\n\t\t\t\t}\n\t\t\t}\n\t\t}\n\t}\n}\n";
        let config = parse_config(text).unwrap();
        assert_eq!(config.processing_queue, vec![553850, 2322010]);
        assert!(config.background_processing);
        assert_eq!(
            config.current_buckets.into_iter().collect::<Vec<_>>(),
            vec!["2e6c105801134e9c", "3554f158047e28bf", "923fa87291dfec1c"]
        );
    }

    #[test]
    fn missing_sections_mean_nothing_queued_and_background_off() {
        let config = parse_config("\"InstallConfigStore\"\n{\n}\n").unwrap();
        assert!(config.processing_queue.is_empty());
        assert!(!config.background_processing);
    }

    #[test]
    fn reads_the_latest_bucket_choice_per_app_from_the_shader_log() {
        let log = "[2026-11-07 12:04:15] Found 2 buckets for AppId 2322010 CompatTool: GE-Proton11-11-x86_64:\n[2026-11-07 12:04:15] SteamSwarm / G7:VulkanPipelinesV6_2e6c105801134e9c\n[2026-11-07 12:04:15] SteamSwarm / G7:VulkanPipelinesV6_923fa87291dfec1c\n[2026-11-07 12:04:16] Unrelated line\n[2026-11-07 12:43:18] Found 2 buckets for AppId 2322010 CompatTool: GE-Proton11-12-x86_64:\n[2026-11-07 12:43:18] SteamSwarm / G7:VulkanPipelinesV6_923fa87291dfec1c\n[2026-11-07 12:43:18] SteamSwarm / G7:VulkanPipelinesV6_3554f158047e28bf\n[2026-11-07 12:43:19] Found 1 buckets for AppId 292030 CompatTool: proton:\n[2026-11-07 12:43:19] Something else\n";
        let mut found = BTreeMap::new();
        parse_bucket_log(log, &mut found);
        assert_eq!(found[&2322010], vec!["923fa87291dfec1c", "3554f158047e28bf"]);
        assert!(found[&292030].is_empty());
    }

    #[test]
    fn discovers_libraries_and_installed_apps() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Steam");
        let lib = dir.path().join("Library");
        fs::create_dir_all(root.join("steamapps")).unwrap();
        fs::create_dir_all(root.join("ubuntu12_64")).unwrap();
        fs::create_dir_all(lib.join("steamapps")).unwrap();
        fs::write(
            root.join("steamapps/libraryfolders.vdf"),
            format!(
                "\"libraryfolders\"\n{{\n\t\"0\"\n\t{{\n\t\t\"path\"\t\t\"{}\"\n\t}}\n\t\"1\"\n\t{{\n\t\t\"path\"\t\t\"{}\"\n\t}}\n}}\n",
                root.display(),
                lib.display()
            ),
        )
        .unwrap();
        fs::write(
            lib.join("steamapps/appmanifest_2322010.acf"),
            "\"AppState\"\n{\n\t\"appid\"\t\t\"2322010\"\n\t\"name\"\t\t\"God of War Ragnarök\"\n\t\"StateFlags\"\t\t\"4\"\n\t\"LastPlayed\"\t\t\"1794049741\"\n}\n",
        )
        .unwrap();
        fs::write(
            lib.join("steamapps/appmanifest_10.acf"),
            "\"AppState\"\n{\n\t\"appid\"\t\t\"10\"\n\t\"name\"\t\t\"Downloading\"\n\t\"StateFlags\"\t\t\"1026\"\n}\n",
        )
        .unwrap();
        let steam = Steam::locate(Some(&root)).unwrap();
        assert_eq!(steam.libraries.len(), 2);
        let apps = steam.apps().unwrap();
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].name, "God of War Ragnarök");
        assert_eq!(apps[0].last_played, 1794049741);
        assert_eq!(
            steam.shader_cache_dir(&apps[0]),
            lib.canonicalize().unwrap().join("steamapps/shadercache/2322010")
        );
    }
}
