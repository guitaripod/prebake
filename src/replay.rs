use crate::plan::{Group, SCRUBBED_ENV};
use crate::sys;
use anyhow::{Context, Result};
use nix::sys::socket::{AddressFamily, MsgFlags, SockFlag, SockType, send, socketpair};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How far a replay has come: pipelines recorded in the replayer cache since it started,
/// out of those that were still missing when it started.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    pub done: u64,
    pub total: u64,
}

pub struct Options<'a> {
    pub fossilize: &'a Path,
    pub env: &'a [(String, String)],
    pub workers: u32,
    pub device_index: u32,
    pub timeout_seconds: u32,
}

/// A running `fossilize_replay --master-process` for one group, steered over its control
/// socket the same way Steam steers it.
pub struct Job {
    pub group: Group,
    pub workers: u32,
    pub started: Instant,
    target: u32,
    child: Child,
    pgid: i32,
    control: Option<OwnedFd>,
    reader: Option<JoinHandle<()>>,
    pending: u64,
    recorded_at_start: u64,
    replay_cache: PathBuf,
}

impl Job {
    pub fn start(group: &Group, options: &Options, uuid: &str, pending: u64) -> Result<Self> {
        let (ours, theirs) = socketpair(AddressFamily::Unix, SockType::SeqPacket, None, SockFlag::SOCK_CLOEXEC)
            .context("creating the replayer control socket")?;
        let (reader, writer) = std::io::pipe().context("creating the replayer output pipe")?;
        let mut command = Command::new(options.fossilize);
        command
            .args(arguments(group, options, theirs.as_raw_fd()))
            .stdin(Stdio::null())
            .stdout(writer.try_clone()?)
            .stderr(writer);
        for name in SCRUBBED_ENV {
            command.env_remove(name);
        }
        command.envs(options.env.iter().map(|(k, v)| (k, v)));
        sys::background_child(&mut command, theirs.as_raw_fd());
        let child = command
            .spawn()
            .with_context(|| format!("starting {}", options.fossilize.display()))?;
        drop(command);
        drop(theirs);
        let app = group.app;
        let reader = std::thread::Builder::new()
            .name("replay-output".into())
            .spawn(move || drain_output(reader, app))
            .context("starting the output reader")?;
        let replay_cache = group.replay_cache_base(uuid);
        let mut job = Self {
            group: group.clone(),
            workers: options.workers,
            started: Instant::now(),
            target: u32::MAX,
            pgid: child.id() as i32,
            child,
            control: Some(ours),
            reader: Some(reader),
            pending,
            recorded_at_start: recorded_entries(&replay_cache),
            replay_cache,
        };
        job.set_target(options.workers)?;
        Ok(job)
    }

    /// Worker processes currently allowed to run.
    pub fn target(&self) -> u32 {
        self.target
    }

    pub fn progress(&self) -> Progress {
        let done = recorded_entries(&self.replay_cache).saturating_sub(self.recorded_at_start);
        Progress {
            done: done.min(self.pending),
            total: self.pending,
        }
    }

    pub fn pid(&self) -> i32 {
        self.pgid
    }

    /// Sets how many workers may run; Fossilize stops (SIGSTOP) the rest and wakes them
    /// again later. A fixed count replaces Fossilize's own I/O-stall heuristic, which can
    /// hold a replay at one worker for as long as any other program keeps the disk busy;
    /// the replay already runs in the idle I/O class, so it yields the disk anyway.
    pub fn set_target(&mut self, workers: u32) -> Result<()> {
        if workers == self.target {
            return Ok(());
        }
        if let Some(control) = &self.control {
            let command = format!("RUNNING_TARGET {workers}");
            send(control.as_raw_fd(), command.as_bytes(), MsgFlags::MSG_NOSIGNAL).context("sending to the replayer")?;
        }
        self.target = workers;
        Ok(())
    }

    /// Returns the exit status once the replayer has finished.
    pub fn poll(&mut self) -> Result<Option<ExitStatus>> {
        let status = self.child.try_wait()?;
        if status.is_some() {
            self.join_reader();
        }
        Ok(status)
    }

    /// Stops the replay. Closing the control socket makes Fossilize kill its workers and
    /// exit; anything still alive after a grace period is killed with its process group.
    pub fn stop(mut self) -> Result<ExitStatus> {
        self.control.take();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait()? {
                sys::kill_group(self.pgid);
                self.join_reader();
                return Ok(status);
            }
            if Instant::now() >= deadline {
                sys::kill_group(self.pgid);
                let status = self.child.wait()?;
                self.join_reader();
                return Ok(status);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn join_reader(&mut self) {
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn arguments(group: &Group, options: &Options, control_fd: i32) -> Vec<String> {
    let mut args: Vec<String> = group.databases.iter().map(|p| p.display().to_string()).collect();
    args.extend(
        [
            "--master-process",
            "--quiet-slave",
            "--spirv-val",
            "--num-threads",
            &options.workers.max(1).to_string(),
            "--on-disk-validation-whitelist",
            &group.whitelist_stem().display().to_string(),
            "--replayer-cache",
            &group.replay_cache_stem().display().to_string(),
            "--device-index",
            &options.device_index.to_string(),
            "--timeout-seconds",
            &options.timeout_seconds.to_string(),
            "--control-fd",
            &control_fd.to_string(),
        ]
        .map(String::from),
    );
    if group.implicit_whitelist() {
        args.extend(["--implicit-whitelist".to_string(), "0".to_string()]);
    }
    args
}

/// Reads the replayer's output so it never blocks on a full pipe, keeping errors and
/// crashed pipelines for the debug log. `--progress` is not used: it makes
/// `fossilize_replay` run the real master as a child that never sees the control socket.
fn drain_output(reader: std::io::PipeReader, app: u32) {
    for line in BufReader::new(reader).lines() {
        let Ok(line) = line else { break };
        if line.starts_with("Fossilize ERROR") || line.contains("crashed or hung") {
            tracing::debug!(app, "{}", line.trim());
        }
    }
}

/// Entries in a replayer cache and its fragments, counted from file sizes: every entry the
/// replayer records is a bare 56-byte header after the 16-byte file header.
pub fn recorded_entries(base: &Path) -> u64 {
    let mut files = crate::foz::fragments(base).unwrap_or_default();
    files.push(base.to_path_buf());
    files
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len().saturating_sub(16) / 56)
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_recorded_entries_from_file_sizes() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("replay_cache.u.foz");
        crate::foz::testing::write_archive(&base, &[(6, 1, b""), (6, 2, b"")]);
        crate::foz::testing::write_archive(&dir.path().join("replay_cache.u.4.foz"), &[(7, 3, b"")]);
        std::fs::write(dir.path().join("replay_cache.u.5.foz"), b"").unwrap();
        assert_eq!(recorded_entries(&base), 3);
        assert_eq!(recorded_entries(&dir.path().join("replay_cache.none.foz")), 0);
    }

    #[test]
    fn builds_steam_compatible_arguments() {
        let dir = PathBuf::from("/lib/steamapps/shadercache/1/fozpipelinesv6/steamapprun_pipeline_cache.abc");
        let group = Group {
            app: 1,
            dir: dir.clone(),
            bucket: Some("abc".into()),
            databases: vec![dir.join("steamapp_pipeline_cache.foz"), dir.join("steam_pipeline_cache.foz")],
        };
        let options = Options {
            fossilize: Path::new("/steam/ubuntu12_64/fossilize_replay"),
            env: &[],
            workers: 27,
            device_index: 0,
            timeout_seconds: 60,
        };
        let args = arguments(&group, &options, 9).join(" ");
        assert!(args.starts_with(&format!(
            "{0}/steamapp_pipeline_cache.foz {0}/steam_pipeline_cache.foz --master-process",
            dir.display()
        )));
        assert!(args.contains(&format!("--replayer-cache {}/replay_cache ", dir.display())));
        assert!(args.contains(&format!(
            "--on-disk-validation-whitelist {}/steam_pipeline_cache_whitelist ",
            dir.display()
        )));
        assert!(args.contains("--num-threads 27 "));
        assert!(args.contains("--control-fd 9"));
        assert!(args.ends_with("--implicit-whitelist 0"));
    }

    /// Drives the real `fossilize_replay` against a scratch copy of a Steam replay directory:
    /// `PREBAKE_TEST_GROUP=<dir> PREBAKE_TEST_FOSSILIZE=<path> cargo test -- --ignored`.
    /// The driver cache goes to a temporary directory, so no real Steam cache is touched.
    #[test]
    #[ignore = "needs a GPU, Steam's fossilize_replay and a scratch copy of a shader cache"]
    fn steers_a_real_replay() {
        let dir = PathBuf::from(std::env::var("PREBAKE_TEST_GROUP").expect("PREBAKE_TEST_GROUP"));
        let fossilize = PathBuf::from(std::env::var("PREBAKE_TEST_FOSSILIZE").expect("PREBAKE_TEST_FOSSILIZE"));
        let databases: Vec<PathBuf> = ["steamapp_pipeline_cache.foz", "steam_pipeline_cache.foz"]
            .iter()
            .map(|n| dir.join(n))
            .filter(|p| p.exists())
            .collect();
        let group = Group {
            app: 1,
            dir: dir.clone(),
            bucket: None,
            databases,
        };
        let cache = tempfile::tempdir().unwrap();
        let env = vec![
            ("__GL_SHADER_DISK_CACHE_PATH".to_string(), cache.path().display().to_string()),
            ("__GL_SHADER_DISK_CACHE_APP_NAME".to_string(), "steamapp_shader_cache".to_string()),
            ("__GL_SHADER_DISK_CACHE_SKIP_CLEANUP".to_string(), "1".to_string()),
            ("VK_LOADER_LAYERS_DISABLE".to_string(), "~implicit~".to_string()),
        ];
        let options = Options {
            fossilize: &fossilize,
            env: &env,
            workers: 2,
            device_index: 0,
            timeout_seconds: 60,
        };
        let mut job = Job::start(&group, &options, "d7567106f26893a3f6c975eefa1448b9", 1000).unwrap();
        let states = |job: &Job| -> Vec<char> {
            std::fs::read_dir("/proc")
                .unwrap()
                .flatten()
                .filter_map(|e| std::fs::read_to_string(e.path().join("stat")).ok())
                .filter_map(|stat| {
                    let rest = stat[stat.rfind(')')? + 2..].to_string();
                    let mut fields = rest.split_whitespace();
                    let state = fields.next()?.chars().next()?;
                    let pgrp: i32 = fields.nth(1)?.parse().ok()?;
                    (pgrp == job.pid()).then_some(state)
                })
                .collect()
        };
        std::thread::sleep(Duration::from_secs(8));
        assert!(job.poll().unwrap().is_none(), "replay ended before it could be steered");
        assert_eq!(job.progress().total, 1000);

        job.set_target(0).unwrap();
        std::thread::sleep(Duration::from_secs(3));
        let paused = states(&job);
        assert!(paused.iter().filter(|s| **s == 'T').count() >= 2, "workers not stopped: {paused:?}");

        job.set_target(2).unwrap();
        std::thread::sleep(Duration::from_secs(3));
        let resumed = states(&job);
        assert!(!resumed.contains(&'T'), "workers still stopped: {resumed:?}");

        let pgid = job.pid();
        job.stop().unwrap();
        std::thread::sleep(Duration::from_millis(500));
        let left: Vec<char> = std::fs::read_dir("/proc")
            .unwrap()
            .flatten()
            .filter_map(|e| std::fs::read_to_string(e.path().join("stat")).ok())
            .filter(|stat| stat[stat.rfind(')').unwrap() + 2..].split_whitespace().nth(2) == Some(&pgid.to_string()))
            .map(|_| 'x')
            .collect();
        assert!(left.is_empty(), "processes left in the replay group");
        let cache = dir.join("replay_cache.d7567106f26893a3f6c975eefa1448b9.foz");
        assert!(!crate::foz::fragments(&cache).unwrap().is_empty(), "the replay recorded nothing");
    }
}
