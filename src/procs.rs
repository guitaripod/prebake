use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// What is running on the machine that prebake has to make way for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub games: BTreeSet<u64>,
    pub steam_replay_dirs: BTreeSet<PathBuf>,
    pub steam_running: bool,
}

/// Scans `/proc` for Steam game launches (`reaper SteamLaunch AppId=…`), for replays Steam
/// itself is running, and for the Steam client. Processes in `own_group` are prebake's.
pub fn scan(steam_root: &Path, own_group: Option<i32>) -> Snapshot {
    let mut snapshot = Snapshot::default();
    let Ok(entries) = fs::read_dir("/proc") else {
        return snapshot;
    };
    for entry in entries.flatten() {
        if !entry.file_name().to_str().is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit())) {
            continue;
        }
        let dir = entry.path();
        let Ok(comm) = fs::read_to_string(dir.join("comm")) else { continue };
        match comm.trim_end() {
            "reaper" => {
                if let Some(app) = steam_launch_app(&read_cmdline(&dir)) {
                    snapshot.games.insert(app);
                }
            }
            "fossilize_repla" => {
                if own_group.is_some_and(|group| process_group(&dir) == Some(group)) {
                    continue;
                }
                snapshot.steam_replay_dirs.extend(replay_dirs(&read_cmdline(&dir)));
            }
            "steam" | "steamwebhelper" if !snapshot.steam_running && is_steam_client(&dir, steam_root) => {
                snapshot.steam_running = true;
            }
            _ => {}
        }
    }
    snapshot
}

/// Resident anonymous and shared memory of every process in a process group: what a replay
/// holds that would come back if it stopped.
pub fn group_memory(pgid: i32) -> u64 {
    let Ok(entries) = fs::read_dir("/proc") else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| entry.file_name().to_str().is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit())))
        .filter(|entry| process_group(&entry.path()) == Some(pgid))
        .filter_map(|entry| fs::read_to_string(entry.path().join("status")).ok())
        .map(|status| status_kib(&status, "RssAnon:") + status_kib(&status, "RssShmem:"))
        .sum::<u64>()
        * 1024
}

fn status_kib(status: &str, field: &str) -> u64 {
    status
        .lines()
        .find_map(|l| l.strip_prefix(field))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        .unwrap_or(0)
}

fn read_cmdline(dir: &Path) -> Vec<String> {
    fs::read(dir.join("cmdline"))
        .map(|bytes| {
            bytes
                .split(|b| *b == 0)
                .filter(|part| !part.is_empty())
                .map(|part| String::from_utf8_lossy(part).into_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn process_group(dir: &Path) -> Option<i32> {
    let stat = fs::read_to_string(dir.join("stat")).ok()?;
    let after_name = &stat[stat.rfind(')')? + 2..];
    after_name.split_whitespace().nth(2)?.parse().ok()
}

fn is_steam_client(dir: &Path, steam_root: &Path) -> bool {
    fs::read_link(dir.join("exe")).is_ok_and(|exe| {
        let exe = exe.to_string_lossy();
        let exe = exe.trim_end_matches(" (deleted)");
        Path::new(exe).starts_with(steam_root)
    })
}

/// Reads the Steam app id from a `reaper SteamLaunch AppId=<id> -- …` command line.
pub fn steam_launch_app(args: &[String]) -> Option<u64> {
    let launch = args.iter().position(|a| a == "SteamLaunch")?;
    args[launch..]
        .iter()
        .take_while(|a| *a != "--")
        .find_map(|a| a.strip_prefix("AppId=")?.parse().ok())
}

/// The directories whose databases a `fossilize_replay` command line replays.
pub fn replay_dirs(args: &[String]) -> Vec<PathBuf> {
    args.iter()
        .skip(1)
        .take_while(|a| !a.starts_with("--"))
        .filter(|a| a.ends_with(".foz"))
        .filter_map(|a| Path::new(a).parent().map(Path::to_path_buf))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(text: &str) -> Vec<String> {
        text.split(' ').map(String::from).collect()
    }

    #[test]
    fn reads_the_app_id_of_a_steam_launch() {
        let cmd = args(
            "/home/u/.local/share/Steam/steamrt64/reaper SteamLaunch AppId=2322010 -- /x/_v2-entry-point --verb=waitforexitandrun -- AppId=5",
        );
        assert_eq!(steam_launch_app(&cmd), Some(2322010));
        assert_eq!(
            steam_launch_app(&args("reaper SteamLaunch AppId=9303876951129194496 -- game")),
            Some(9303876951129194496)
        );
        assert_eq!(steam_launch_app(&args("/usr/bin/reaper --help")), None);
    }

    #[test]
    fn reads_the_directories_a_replay_works_in() {
        let cmd = args(
            "/s/ubuntu12_64/fossilize_replay /c/2322010/fozpipelinesv6/steamapprun_pipeline_cache.35/steamapp_pipeline_cache.foz /c/2322010/fozpipelinesv6/steamapprun_pipeline_cache.35/steam_pipeline_cache.foz --master-process --replayer-cache /c/2322010/fozpipelinesv6/x/replay_cache",
        );
        assert_eq!(
            replay_dirs(&cmd),
            vec![
                PathBuf::from("/c/2322010/fozpipelinesv6/steamapprun_pipeline_cache.35"),
                PathBuf::from("/c/2322010/fozpipelinesv6/steamapprun_pipeline_cache.35")
            ]
        );
    }

    #[test]
    fn measures_the_memory_of_its_own_process_group() {
        let own = process_group(Path::new("/proc/self")).unwrap();
        assert!(group_memory(own) > 0);
        assert_eq!(group_memory(i32::MAX), 0);
        assert_eq!(status_kib("Name:\tx\nRssAnon:\t  2048 kB\n", "RssAnon:"), 2048);
    }

    #[test]
    fn scanning_this_machine_does_not_fail() {
        let snapshot = scan(Path::new("/nonexistent"), None);
        assert!(!snapshot.steam_running);
    }
}
