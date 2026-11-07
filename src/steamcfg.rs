use crate::steam::Steam;
use crate::vdf;
use anyhow::{Context, Result, bail};
use std::fs;
use std::path::Path;

const BACKGROUND_KEY: [&str; 6] = [
    "InstallConfigStore",
    "Software",
    "Valve",
    "Steam",
    "ShaderCacheManager",
    "EnableShaderBackgroundProcessing",
];
const THREADS_KEY: &str = "unShaderBackgroundProcessingThreads";

/// Turns Steam's "Allow background processing of Vulkan shaders" on or off and optionally
/// sets its thread count in `steam_dev.cfg`. Steam keeps its settings in memory and writes
/// `config.vdf` when it exits, so this refuses to run while Steam is open.
pub fn set_background(steam: &Steam, steam_running: bool, enabled: bool, threads: Option<u32>) -> Result<()> {
    if steam_running {
        bail!("Steam is running and would overwrite the change when it exits; quit it first (steam -shutdown)");
    }
    let path = steam.config_path();
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let updated = vdf::set_string(&text, &BACKGROUND_KEY, if enabled { "1" } else { "0" })?;
    if updated != text {
        let backup = path.with_extension("vdf.prebake-backup");
        fs::copy(&path, &backup).with_context(|| format!("backing up to {}", backup.display()))?;
        write_atomically(&path, &updated)?;
    }
    if let Some(threads) = threads {
        set_dev_value(&steam.dev_config_path(), THREADS_KEY, &threads.to_string())?;
    }
    Ok(())
}

/// The thread count set in `steam_dev.cfg`, if any.
pub fn background_threads(steam: &Steam) -> Option<u32> {
    let text = fs::read_to_string(steam.dev_config_path()).ok()?;
    text.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        (parts.next()? == THREADS_KEY).then(|| parts.next()?.trim_matches('"').parse().ok())?
    })
}

fn set_dev_value(path: &Path, key: &str, value: &str) -> Result<()> {
    let text = fs::read_to_string(path).unwrap_or_default();
    let mut found = false;
    let mut lines: Vec<String> = text
        .lines()
        .map(|line| {
            if line.split_whitespace().next() == Some(key) {
                found = true;
                format!("{key} {value}")
            } else {
                line.to_string()
            }
        })
        .collect();
    if !found {
        lines.push(format!("{key} {value}"));
    }
    let mut out = lines.join("\n");
    out.push('\n');
    write_atomically(path, &out)
}

fn write_atomically(path: &Path, text: &str) -> Result<()> {
    let tmp = path.with_extension("prebake-tmp");
    fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_steam(dir: &Path) -> Steam {
        fs::create_dir_all(dir.join("config")).unwrap();
        fs::write(
            dir.join("config/config.vdf"),
            "\"InstallConfigStore\"\n{\n\t\"Software\"\n\t{\n\t\t\"Valve\"\n\t\t{\n\t\t\t\"Steam\"\n\t\t\t{\n\t\t\t\t\"AutoUpdateWindowEnabled\"\t\t\"0\"\n\t\t\t}\n\t\t}\n\t}\n}\n",
        )
        .unwrap();
        Steam {
            root: dir.to_path_buf(),
            libraries: vec![dir.to_path_buf()],
        }
    }

    #[test]
    fn enables_background_processing_and_sets_threads() {
        let dir = tempfile::tempdir().unwrap();
        let steam = fake_steam(dir.path());
        fs::write(steam.dev_config_path(), "@nClientDownloadEnableHTTP2PlatformLinux 0\n").unwrap();
        set_background(&steam, false, true, Some(4)).unwrap();
        let config = crate::steam::parse_config(&fs::read_to_string(steam.config_path()).unwrap()).unwrap();
        assert!(config.background_processing);
        assert_eq!(background_threads(&steam), Some(4));
        assert!(
            fs::read_to_string(steam.dev_config_path())
                .unwrap()
                .starts_with("@nClientDownloadEnableHTTP2PlatformLinux 0\n")
        );
        assert!(steam.config_path().with_extension("vdf.prebake-backup").exists());

        set_background(&steam, false, false, Some(2)).unwrap();
        let config = crate::steam::parse_config(&fs::read_to_string(steam.config_path()).unwrap()).unwrap();
        assert!(!config.background_processing);
        assert_eq!(background_threads(&steam), Some(2));
        assert_eq!(fs::read_to_string(steam.dev_config_path()).unwrap().matches(THREADS_KEY).count(), 1);
    }

    #[test]
    fn refuses_while_steam_runs() {
        let dir = tempfile::tempdir().unwrap();
        let steam = fake_steam(dir.path());
        let before = fs::read_to_string(steam.config_path()).unwrap();
        assert!(set_background(&steam, true, true, None).is_err());
        assert_eq!(fs::read_to_string(steam.config_path()).unwrap(), before);
    }
}
