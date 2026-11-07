use crate::config::Config;
use crate::foz;
use crate::gpu::{self, Device};
use crate::plan::{self, AppPlan, Group, Pending};
use crate::presence::{self, Presence};
use crate::procs::{self, Snapshot};
use crate::replay::{Job, Options};
use crate::state::{self, AppState, AppStatus, Control, Dirs, JobStatus, Records, Status, now};
use crate::steam::Steam;
use anyhow::{Context, Result, bail};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const TICK: Duration = Duration::from_secs(2);
const REPLAN_EVERY: Duration = Duration::from_secs(15 * 60);
const REPROBE_GPU_EVERY: Duration = Duration::from_secs(60 * 60);
const SHRINK_AFTER: Duration = Duration::from_secs(2 * 60);
const SCAN_BUDGET: Duration = Duration::from_secs(5);
const FREEZE_GRACE: Duration = Duration::from_secs(20);

/// Everything the policy looks at to decide how hard to work.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Inputs {
    pub paused: bool,
    pub boosted: bool,
    pub game_running: bool,
    pub on_battery: bool,
    pub away: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Stop(&'static str),
    Freeze(&'static str),
    Run { workers: u32, reason: &'static str },
}

/// The policy: never compete with a game or a pause, work flat out while you are away
/// (or asked to), and use `workers_present` while you are at the computer.
pub fn decide(inputs: Inputs, away_workers: u32, present_workers: u32, run_on_battery: bool) -> Decision {
    if inputs.paused {
        return Decision::Stop("paused");
    }
    if inputs.game_running {
        return Decision::Freeze("a game is running");
    }
    if inputs.on_battery && !run_on_battery {
        return Decision::Stop("on battery");
    }
    if inputs.boosted {
        return Decision::Run {
            workers: away_workers,
            reason: "asked to bake now",
        };
    }
    if inputs.away {
        return Decision::Run {
            workers: away_workers,
            reason: "you are away",
        };
    }
    Decision::Run {
        workers: present_workers,
        reason: "you are at the computer",
    }
}

/// Which games to bake: everything, or only the listed apps until they are done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Service,
    Once { apps: Vec<u32>, workers: Option<u32> },
}

pub struct Daemon {
    config: Config,
    dirs: Dirs,
    mode: Mode,
    steam: Steam,
    presence: Arc<Presence>,
    records: Records,
    plans: Vec<AppPlan>,
    planned_at: Option<Instant>,
    device: Option<Device>,
    probed_at: Option<Instant>,
    job: Option<Job>,
    shrink_since: Option<Instant>,
    frozen_since: Option<Instant>,
    snapshot: Snapshot,
    steam_background: bool,
    activity: String,
    reason: String,
    last_error: Option<String>,
    deferred: BTreeSet<PathBuf>,
}

impl Daemon {
    pub fn new(config: Config, dirs: Dirs, mode: Mode) -> Result<Self> {
        if crate::steam::flatpak_only() && config.steam_root.is_none() {
            bail!("only a Flatpak Steam was found; prebake drives a native Steam installation");
        }
        let steam = Steam::locate(config.steam_root.as_deref())?;
        if !steam.fossilize_replay().is_file() {
            bail!(
                "{} is missing; start Steam once so it installs it",
                steam.fossilize_replay().display()
            );
        }
        let presence = presence::watch(config.idle_after, config.respect_inhibitors);
        let mut records: Records = state::read_json(&dirs.records());
        records.prune();
        Ok(Self {
            config,
            dirs,
            mode,
            steam,
            presence,
            records,
            plans: Vec::new(),
            planned_at: None,
            device: None,
            probed_at: None,
            job: None,
            shrink_since: None,
            frozen_since: None,
            snapshot: Snapshot::default(),
            steam_background: false,
            activity: "starting".into(),
            reason: String::new(),
            last_error: None,
            deferred: BTreeSet::new(),
        })
    }

    /// Runs until `stop` is set, or in once mode until the requested games are baked.
    pub fn run(&mut self, stop: &AtomicBool) -> Result<()> {
        while !stop.load(Ordering::Relaxed) {
            if let Err(err) = self.tick() {
                tracing::error!("{err:#}");
                self.last_error = Some(format!("{err:#}"));
            }
            self.write_status();
            if matches!(self.mode, Mode::Once { .. }) && self.job.is_none() && self.activity == "done" {
                break;
            }
            sleep_unless(stop, TICK);
        }
        self.shutdown();
        Ok(())
    }

    fn shutdown(&mut self) {
        if let Some(job) = self.job.take() {
            let group = job.group.clone();
            match job.stop() {
                Ok(_) => tracing::info!(app = group.app, "stopped the replay for shutdown"),
                Err(err) => tracing::warn!("stopping the replay: {err:#}"),
            }
            self.merge(&group);
        }
        self.save_records();
        self.activity = "stopped".into();
        self.reason.clear();
        self.write_status();
    }

    fn tick(&mut self) -> Result<()> {
        let now_s = now();
        self.snapshot = procs::scan(&self.steam.root, self.job.as_ref().map(Job::pid));
        self.refresh_device()?;
        self.refresh_plan()?;
        let Some(device) = self.device.clone() else {
            self.activity = "waiting".into();
            self.reason = "no Vulkan device".into();
            return Ok(());
        };
        self.finish_job_if_done(&device.uuid, now_s)?;
        self.yield_to_steam();

        let control: Control = state::read_json(&self.dirs.control());
        let inputs = Inputs {
            paused: control.paused(now_s),
            boosted: control.boosted(now_s) || matches!(self.mode, Mode::Once { .. }),
            game_running: !self.snapshot.games.is_empty() || self.presence.gamemode_games() > 0,
            on_battery: presence::on_battery(),
            away: self.presence.away(),
        };
        let away_workers = match self.mode {
            Mode::Once { workers: Some(n), .. } => n,
            _ => self.config.away_workers(),
        };
        match decide(inputs, away_workers, self.config.workers_present, self.config.run_on_battery) {
            Decision::Stop(reason) => {
                if self.job.is_some() {
                    tracing::debug!(?inputs, games = ?self.snapshot.games, gamemode = self.presence.gamemode_games(), "stopping");
                }
                self.stop_job(reason);
                self.activity = if inputs.paused { "paused" } else { "waiting" }.into();
                self.reason = reason.into();
            }
            Decision::Freeze(reason) => {
                self.freeze(reason)?;
                self.activity = "waiting".into();
                self.reason = reason.into();
            }
            Decision::Run { workers, reason } => {
                self.frozen_since = None;
                self.reason = reason.into();
                let held = self.job.as_ref().map_or(0, |j| u64::from(j.workers) * crate::config::WORKER_MEMORY);
                let workers = workers.min(crate::config::memory_cap(crate::config::available_memory_bytes(), held));
                self.steer(workers, &device, now_s)?;
                if self.job.is_some() {
                    self.scan_some(&device.uuid);
                }
            }
        }
        Ok(())
    }

    /// Counts the shaders still missing for a few unscanned groups while a replay keeps the
    /// machine busy anyway, so `prebake status` fills in the whole backlog over time.
    fn scan_some(&mut self, uuid: &str) {
        let started = Instant::now();
        let unscanned: Vec<Group> = self
            .plans
            .iter()
            .flat_map(|p| p.groups.iter())
            .filter(|g| !self.records.done(g, uuid) && self.records.cached_pending(g, uuid).is_none())
            .filter(|g| self.job.as_ref().is_none_or(|j| j.group.app != g.app))
            .cloned()
            .collect();
        let mut changed = false;
        for group in unscanned {
            if started.elapsed() > SCAN_BUDGET / 5 {
                break;
            }
            match group.pending(uuid) {
                Ok(pending) => {
                    self.records.store_pending(&group, uuid, pending);
                    changed = true;
                }
                Err(err) => tracing::debug!("scanning {}: {err:#}", group.dir.display()),
            }
            for db in &group.databases {
                foz::forget(db);
            }
        }
        if changed {
            self.save_records();
        }
    }

    fn refresh_device(&mut self) -> Result<()> {
        if self.probed_at.is_some_and(|t| t.elapsed() < REPROBE_GPU_EVERY) {
            return Ok(());
        }
        if self.device.is_some() && (self.job.is_some() || !self.snapshot.games.is_empty()) {
            return Ok(());
        }
        self.probed_at = Some(Instant::now());
        let devices = gpu::probe_in_child()?;
        let device = gpu::choose(&devices, self.config.device_index).cloned();
        if device.as_ref().map(|d| &d.uuid) != self.device.as_ref().map(|d| &d.uuid) {
            if let Some(d) = &device {
                tracing::info!(gpu = %d.name, uuid = %d.uuid, index = d.index, "replaying on this device");
            }
            self.planned_at = None;
        }
        self.device = device;
        if self.device.is_none() {
            bail!("no Vulkan device to replay on (found {})", devices.len());
        }
        Ok(())
    }

    fn refresh_plan(&mut self) -> Result<()> {
        let stale = self.planned_at.is_none_or(|t| t.elapsed() >= REPLAN_EVERY);
        if !stale {
            return Ok(());
        }
        let config = self.steam.config().unwrap_or_default();
        self.steam_background = config.background_processing;
        let mut plans = plan::build(&self.steam, &config, &self.config.exclude)?;
        if let Mode::Once { apps, .. } = &self.mode
            && !apps.is_empty()
        {
            plans.retain(|p| apps.contains(&p.app.id));
        }
        self.plans = plans;
        self.planned_at = Some(Instant::now());
        self.deferred.clear();
        tracing::debug!(games = self.plans.len(), "planned");
        Ok(())
    }

    fn finish_job_if_done(&mut self, uuid: &str, now_s: u64) -> Result<()> {
        let Some(job) = self.job.as_mut() else { return Ok(()) };
        let Some(status) = job.poll()? else { return Ok(()) };
        let job = self.job.take().expect("job present");
        let group = job.group.clone();
        let progress = job.progress();
        self.merge(&group);
        if status.success() {
            self.records.finished(&group, uuid, now_s);
            if let Ok(pending) = group.pending(uuid) {
                self.records.store_pending(&group, uuid, pending);
            }
            tracing::info!(
                app = group.app,
                group = %group.label(),
                replayed = progress.done,
                seconds = job.started.elapsed().as_secs(),
                "baked"
            );
        } else {
            let delay = self.records.failed(&group, now_s);
            tracing::warn!(app = group.app, group = %group.label(), %status, retry_in_s = delay, "replay failed");
        }
        self.shrink_since = None;
        self.save_records();
        Ok(())
    }

    /// Steam replays a game itself when it is launched (or in the background when that is
    /// enabled). prebake then steps aside for that game, because Steam neither sees nor
    /// waits for prebake's replay.
    fn yield_to_steam(&mut self) {
        let Some(job) = &self.job else { return };
        if !self.steam_busy_with(job.group.app) {
            return;
        }
        let job = self.job.take().expect("job present");
        let group = job.group.clone();
        if let Err(err) = job.stop() {
            tracing::warn!("stopping the replay: {err:#}");
        }
        tracing::info!(app = group.app, "Steam started its own replay for this game; leaving it to Steam");
        self.deferred.insert(group.dir);
        self.shrink_since = None;
    }

    fn steam_busy_with(&self, app: u32) -> bool {
        let app_dirs: BTreeSet<&Path> = self.snapshot.steam_replay_dirs.iter().filter_map(|d| app_cache_dir(d)).collect();
        self.plans
            .iter()
            .find(|p| p.app.id == app)
            .is_some_and(|p| app_dirs.contains(p.cache_dir.as_path()))
    }

    /// Stops every worker at once (Steam briefly launches compatibility tools too, so a
    /// launch is not always a game) and gives the replay up only if it lasts.
    fn freeze(&mut self, reason: &str) -> Result<()> {
        let Some(job) = self.job.as_mut() else { return Ok(()) };
        job.set_target(0)?;
        let since = *self.frozen_since.get_or_insert_with(Instant::now);
        if since.elapsed() >= FREEZE_GRACE {
            self.stop_job(reason);
        }
        Ok(())
    }

    fn stop_job(&mut self, reason: &str) {
        if let Some(job) = self.job.take() {
            let group = job.group.clone();
            match job.stop() {
                Ok(_) => tracing::info!(app = group.app, reason, "paused baking"),
                Err(err) => tracing::warn!("stopping the replay: {err:#}"),
            }
            self.merge(&group);
            self.save_records();
        }
        self.shrink_since = None;
        self.frozen_since = None;
    }

    fn steer(&mut self, workers: u32, device: &Device, now_s: u64) -> Result<()> {
        if let Some(job) = self.job.as_mut() {
            if workers > job.workers {
                let group = job.group.clone();
                self.stop_job("more workers");
                return self.start(&group, workers, device);
            }
            job.set_target(workers)?;
            if workers == job.workers {
                self.shrink_since = None;
            } else {
                let since = *self.shrink_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= SHRINK_AFTER {
                    let group = job.group.clone();
                    self.stop_job("fewer workers");
                    if workers > 0 {
                        return self.start(&group, workers, device);
                    }
                }
            }
            self.activity = if workers == 0 { "holding" } else { "baking" }.into();
            return Ok(());
        }
        if workers == 0 {
            self.activity = "waiting".into();
            return Ok(());
        }
        match self.next_group(&device.uuid, now_s)? {
            Some(group) => self.start(&group, workers, device),
            None => {
                self.activity = if self.scanning_left(&device.uuid, now_s) {
                    "scanning"
                } else {
                    "done"
                }
                .into();
                Ok(())
            }
        }
    }

    fn start(&mut self, group: &Group, workers: u32, device: &Device) -> Result<()> {
        let Some(app_plan) = self.plans.iter().find(|p| p.app.id == group.app) else {
            bail!("no plan for app {}", group.app);
        };
        let env = plan::replay_env(&app_plan.cache_dir, &app_plan.buckets, &self.steam);
        plan::prepare_cache_dirs(&app_plan.cache_dir).with_context(|| format!("preparing {}", app_plan.cache_dir.display()))?;
        let name = app_plan.app.name.clone();
        self.merge(group);
        let fossilize = self.steam.fossilize_replay();
        let pending = self.records.cached_pending(group, &device.uuid).map_or(0, |p| p.pending as u64);
        let job = Job::start(
            group,
            &Options {
                fossilize: &fossilize,
                env: &env,
                workers,
                device_index: device.index,
                timeout_seconds: self.config.pipeline_timeout,
            },
            &device.uuid,
            pending,
        )?;
        tracing::info!(app = group.app, %name, group = %group.label(), workers, "baking");
        self.job = Some(job);
        self.shrink_since = None;
        self.activity = "baking".into();
        Ok(())
    }

    /// Finds the next group that still needs a replay. Groups whose databases are already
    /// fully covered by the replayer cache are marked done without starting Fossilize.
    fn next_group(&mut self, uuid: &str, now_s: u64) -> Result<Option<Group>> {
        let started = Instant::now();
        let candidates: Vec<Group> = self
            .plans
            .iter()
            .filter(|p| !self.steam_busy_with(p.app.id))
            .flat_map(|p| p.groups.iter().cloned())
            .filter(|g| !self.deferred.contains(&g.dir) && !self.records.done(g, uuid) && !self.records.backing_off(g, now_s))
            .collect();
        for group in candidates {
            if started.elapsed() > SCAN_BUDGET {
                return Ok(None);
            }
            let pending = match self.records.cached_pending(&group, uuid) {
                Some(pending) => pending,
                None => match group.pending(uuid) {
                    Ok(pending) => {
                        self.records.store_pending(&group, uuid, pending);
                        pending
                    }
                    Err(err) => {
                        let delay = self.records.failed(&group, now_s);
                        tracing::warn!(app = group.app, "reading {}: {err:#}; retrying in {delay} s", group.dir.display());
                        self.save_records();
                        continue;
                    }
                },
            };
            if pending.pending == 0 {
                self.records.finished(&group, uuid, now_s);
                self.save_records();
                continue;
            }
            return Ok(Some(group));
        }
        Ok(None)
    }

    fn scanning_left(&self, uuid: &str, now_s: u64) -> bool {
        self.plans
            .iter()
            .flat_map(|p| p.groups.iter())
            .any(|g| !self.deferred.contains(&g.dir) && !self.records.done(g, uuid) && !self.records.backing_off(g, now_s))
    }

    /// Folds replay fragments into the replayer cache and whitelist so the next replay, by
    /// prebake or by Steam, skips what was already compiled. Skipped while Steam is
    /// replaying the same directory, since Steam merges them itself when it finishes.
    fn merge(&mut self, group: &Group) {
        let Some(uuid) = self.device.as_ref().map(|d| d.uuid.clone()) else {
            return;
        };
        let fresh = procs::scan(&self.steam.root, self.job.as_ref().map(Job::pid));
        if fresh.steam_replay_dirs.contains(&group.dir) {
            return;
        }
        for base in [group.replay_cache_base(&uuid), group.whitelist_base()] {
            if let Err(err) = foz::merge_fragments(&base) {
                tracing::warn!("merging {}: {err:#}", base.display());
            }
        }
    }

    fn save_records(&self) {
        if let Err(err) = state::write_json(&self.dirs.records(), &self.records) {
            tracing::warn!("saving records: {err:#}");
        }
    }

    fn write_status(&self) {
        let status = self.status();
        if let Err(err) = state::write_json(&self.dirs.status(), &status) {
            tracing::warn!("writing status: {err:#}");
        }
    }

    pub fn status(&self) -> Status {
        let uuid = self.device.as_ref().map(|d| d.uuid.clone()).unwrap_or_default();
        let now_s = now();
        let job = self.job.as_ref().map(|job| {
            let plan = self.plans.iter().find(|p| p.app.id == job.group.app);
            JobStatus {
                app: job.group.app,
                name: plan.map(|p| p.app.name.clone()).unwrap_or_default(),
                group: job.group.label(),
                group_index: plan.and_then(|p| p.groups.iter().position(|g| g.dir == job.group.dir)).unwrap_or(0) + 1,
                groups: plan.map_or(1, |p| p.groups.len()),
                workers: job.workers,
                running: job.target(),
                started: now_s.saturating_sub(job.started.elapsed().as_secs()),
                progress: job.progress(),
            }
        });
        let apps = self
            .plans
            .iter()
            .map(|p| app_status(p, &self.records, &uuid, self.job.as_ref(), self.steam_busy_with(p.app.id), now_s))
            .collect();
        let presence = if self.presence.locked() {
            "locked"
        } else if self.presence.away() {
            "away"
        } else {
            "present"
        };
        Status {
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").into(),
            updated: now_s,
            activity: self.activity.clone(),
            reason: self.reason.clone(),
            presence: presence.into(),
            idle_source: self.presence.source(),
            device: self.device.clone(),
            steam_background_processing: self.steam_background,
            job,
            apps,
            last_error: self.last_error.clone(),
        }
    }
}

fn app_status(plan: &AppPlan, records: &Records, uuid: &str, job: Option<&Job>, steam_busy: bool, now_s: u64) -> AppStatus {
    let pending: Option<Vec<Pending>> = plan
        .groups
        .iter()
        .map(|g| {
            if records.done(g, uuid) {
                Some(Pending { pipelines: 0, pending: 0 })
            } else {
                records.cached_pending(g, uuid)
            }
        })
        .collect();
    let state = if job.is_some_and(|j| j.group.app == plan.app.id) {
        AppState::Baking
    } else if steam_busy {
        AppState::Steam
    } else if plan.groups.iter().all(|g| records.done(g, uuid)) {
        AppState::Ready
    } else if plan.groups.iter().any(|g| records.backing_off(g, now_s)) {
        AppState::Failed
    } else {
        AppState::Waiting
    };
    let known: Vec<Pending> = plan.groups.iter().filter_map(|g| records.cached_pending(g, uuid)).collect();
    AppStatus {
        app: plan.app.id,
        name: plan.app.name.clone(),
        state,
        pipelines: (known.len() == plan.groups.len()).then(|| known.iter().map(|p| p.pipelines).sum()),
        pending: pending.map(|all| all.iter().map(|p| p.pending).sum()),
        last_played: plan.app.last_played,
    }
}

/// Builds a status without the service: what is baked according to prebake's records.
/// With `scan`, every unscanned group is read now and the result is saved, which only
/// happens while the service is not running.
pub fn offline_status(config: &Config, dirs: &Dirs, scan: bool) -> Result<Status> {
    let steam = Steam::locate(config.steam_root.as_deref())?;
    let steam_config = steam.config().unwrap_or_default();
    let plans = plan::build(&steam, &steam_config, &config.exclude)?;
    let mut records: Records = state::read_json(&dirs.records());
    let devices = gpu::probe_in_child()?;
    let device = gpu::choose(&devices, config.device_index).cloned();
    let uuid = device.as_ref().map(|d| d.uuid.clone()).unwrap_or_default();
    let now_s = now();
    if scan && device.is_some() {
        for group in plans.iter().flat_map(|p| p.groups.iter()) {
            if records.done(group, &uuid) || records.cached_pending(group, &uuid).is_some() {
                continue;
            }
            let pending = group.pending(&uuid).with_context(|| format!("reading {}", group.dir.display()))?;
            records.store_pending(group, &uuid, pending);
            if pending.pending == 0 {
                records.finished(group, &uuid, now_s);
            }
        }
        state::write_json(&dirs.records(), &records)?;
    }
    let snapshot = procs::scan(&steam.root, None);
    let busy: BTreeSet<&Path> = snapshot.steam_replay_dirs.iter().filter_map(|d| app_cache_dir(d)).collect();
    let apps = plans
        .iter()
        .map(|p| app_status(p, &records, &uuid, None, busy.contains(p.cache_dir.as_path()), now_s))
        .collect();
    Ok(Status {
        version: env!("CARGO_PKG_VERSION").into(),
        updated: now_s,
        activity: "stopped".into(),
        device,
        steam_background_processing: steam_config.background_processing,
        apps,
        ..Status::default()
    })
}

/// The game's shader cache directory (`…/shadercache/<appid>`) for a replay directory.
fn app_cache_dir(replay_dir: &Path) -> Option<&Path> {
    replay_dir
        .ancestors()
        .find(|a| a.file_name().is_some_and(|n| n == "fozpipelinesv6"))
        .and_then(Path::parent)
}

fn sleep_unless(stop: &AtomicBool, duration: Duration) {
    let deadline = Instant::now() + duration;
    while !stop.load(Ordering::Relaxed) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs() -> Inputs {
        Inputs::default()
    }

    #[test]
    fn games_pauses_and_battery_always_stop_the_work() {
        let busy = Inputs {
            away: true,
            boosted: true,
            ..inputs()
        };
        assert_eq!(
            decide(
                Inputs {
                    game_running: true,
                    ..busy
                },
                27,
                2,
                false
            ),
            Decision::Freeze("a game is running")
        );
        assert_eq!(decide(Inputs { paused: true, ..busy }, 27, 2, false), Decision::Stop("paused"));
        assert_eq!(
            decide(Inputs { on_battery: true, ..busy }, 27, 2, false),
            Decision::Stop("on battery")
        );
        assert!(matches!(
            decide(Inputs { on_battery: true, ..busy }, 27, 2, true),
            Decision::Run { workers: 27, .. }
        ));
    }

    #[test]
    fn away_or_boosted_uses_every_worker_and_present_uses_the_present_count() {
        assert!(matches!(
            decide(Inputs { away: true, ..inputs() }, 27, 0, false),
            Decision::Run { workers: 27, .. }
        ));
        assert!(matches!(
            decide(Inputs { boosted: true, ..inputs() }, 27, 0, false),
            Decision::Run { workers: 27, .. }
        ));
        assert!(matches!(decide(inputs(), 27, 0, false), Decision::Run { workers: 0, .. }));
        assert!(matches!(decide(inputs(), 27, 2, false), Decision::Run { workers: 2, .. }));
    }

    #[test]
    fn finds_the_game_cache_dir_of_a_replay_dir() {
        let dir = Path::new("/l/steamapps/shadercache/2322010/fozpipelinesv6/steamapprun_pipeline_cache.35");
        assert_eq!(app_cache_dir(dir), Some(Path::new("/l/steamapps/shadercache/2322010")));
        assert_eq!(
            app_cache_dir(Path::new("/l/steamapps/shadercache/2322010/fozpipelinesv6")),
            Some(Path::new("/l/steamapps/shadercache/2322010"))
        );
        assert_eq!(app_cache_dir(Path::new("/tmp/x")), None);
    }
}
