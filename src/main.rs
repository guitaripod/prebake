mod config;
mod daemon;
mod foz;
mod gpu;
mod plan;
mod presence;
mod procs;
mod render;
mod replay;
mod service;
mod state;
mod steam;
mod steamcfg;
mod sys;
mod vdf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use config::Config;
use daemon::{Daemon, Mode};
use nix::sys::signal::{SigSet, Signal};
use state::{Control, Dirs, Status, now};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Bakes Steam's Vulkan shader caches while you are away, so games start without
/// "Processing Vulkan shaders".
#[derive(Parser)]
#[command(name = "prebake", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Show what is baked and what is still waiting (the default).
    Status {
        /// Print the raw status as JSON.
        #[arg(long)]
        json: bool,
        /// List baked games too.
        #[arg(long)]
        all: bool,
        /// Count the shaders still to compile for every game (only while the service is stopped).
        #[arg(long)]
        scan: bool,
    },
    /// Bake in the foreground now, whether or not you are at the computer.
    Run {
        /// Steam app ids to bake; every game when empty.
        apps: Vec<u32>,
        /// Replay workers to use.
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
        workers: Option<u32>,
    },
    /// Ask the service to bake now, even while you are at the computer.
    Now {
        /// How long to keep going regardless of presence.
        #[arg(long = "for", value_parser = humantime::parse_duration, default_value = "3h")]
        duration: Duration,
    },
    /// Stop baking for a while.
    Pause {
        /// How long to pause.
        #[arg(long = "for", value_parser = humantime::parse_duration, default_value = "2h")]
        duration: Duration,
    },
    /// Cancel `pause` and `now`.
    Resume,
    /// Check the setup and explain what prebake will do on this machine.
    Doctor,
    /// Install, enable and start the systemd user service.
    Install,
    /// Stop and remove the systemd user service.
    Uninstall,
    /// Configure Steam's own background shader processing (Steam must be closed).
    Steam {
        /// Turn "Allow background processing of Vulkan shaders" on or off.
        #[arg(long, value_enum)]
        background: Switch,
        /// Threads Steam may use for it (written to steam_dev.cfg).
        #[arg(long)]
        threads: Option<u32>,
    },
    /// Print the config file location and the default settings.
    Config,
    /// Run the background service; this is what the systemd unit starts.
    Daemon,
    /// List Vulkan devices as prebake sees them.
    #[command(hide = true)]
    Gpu {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Switch {
    On,
    Off,
}

fn main() {
    let cli = Cli::parse();
    if let Err(err) = run(cli.command.unwrap_or(Command::Status {
        json: false,
        all: false,
        scan: false,
    })) {
        eprintln!("prebake: {err:#}");
        std::process::exit(1);
    }
}

fn run(command: Command) -> Result<()> {
    let dirs = Dirs::new()?;
    match command {
        Command::Status { json, all, scan } => show_status(&dirs, json, all, scan),
        Command::Run { apps, workers } => foreground(&dirs, apps, workers),
        Command::Now { duration } => request(
            &dirs,
            |c, until| {
                c.boost_until = Some(until);
                c.paused_until = None;
            },
            duration,
            "baking now until",
        ),
        Command::Pause { duration } => request(
            &dirs,
            |c, until| {
                c.paused_until = Some(until);
                c.boost_until = None;
            },
            duration,
            "paused until",
        ),
        Command::Resume => {
            state::write_json(&dirs.control(), &Control::default())?;
            println!("resumed: baking follows your presence again");
            Ok(())
        }
        Command::Doctor => doctor(&dirs),
        Command::Install => {
            let note = service::install()?;
            if !note.is_empty() {
                println!("{note}");
            }
            println!("prebake.service is enabled and running");
            Ok(())
        }
        Command::Uninstall => {
            if service::uninstall()? {
                println!("prebake.service is stopped and removed");
            } else {
                println!("prebake.service is stopped and disabled; the packaged unit stays until the package is removed");
            }
            Ok(())
        }
        Command::Steam { background, threads } => configure_steam(&dirs, background, threads),
        Command::Config => {
            println!("{}\n", dirs.config.display());
            print!("{}", config::EXAMPLE);
            Ok(())
        }
        Command::Daemon => service_main(&dirs),
        Command::Gpu { json } => {
            let devices = gpu::probe()?;
            if json {
                println!("{}", serde_json::to_string(&devices)?);
            } else {
                for d in devices {
                    println!("{}  {}{}  {}", d.index, d.name, if d.discrete { " (discrete)" } else { "" }, d.uuid);
                }
            }
            Ok(())
        }
    }
}

fn load_config(dirs: &Dirs) -> Result<Config> {
    Config::load(&dirs.config)
}

fn show_status(dirs: &Dirs, json: bool, all: bool, scan: bool) -> Result<()> {
    let config = load_config(dirs)?;
    let status: Status = state::read_json(&dirs.status());
    let live = status.live(now());
    if live && scan {
        bail!("the service is running and scans on its own; --scan is for when it is stopped");
    }
    let status = if live {
        status
    } else {
        daemon::offline_status(&config, dirs, scan)?
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        print!("{}", render::status(&status, live, all));
    }
    Ok(())
}

fn request(dirs: &Dirs, apply: impl FnOnce(&mut Control, u64), duration: Duration, what: &str) -> Result<()> {
    let until = now() + duration.as_secs();
    let mut control: Control = state::read_json(&dirs.control());
    apply(&mut control, until);
    state::write_json(&dirs.control(), &control)?;
    let status: Status = state::read_json(&dirs.status());
    let at = humantime::format_rfc3339_seconds(std::time::UNIX_EPOCH + Duration::from_secs(until));
    println!("{what} {at}");
    if !status.live(now()) {
        println!("note: the service is not running (prebake install)");
    }
    Ok(())
}

fn foreground(dirs: &Dirs, apps: Vec<u32>, workers: Option<u32>) -> Result<()> {
    let status: Status = state::read_json(&dirs.status());
    if status.live(now()) {
        bail!(
            "the service is running (pid {}); use `prebake now` to make it bake right away",
            status.pid
        );
    }
    init_logging();
    let config = load_config(dirs)?;
    let stop = stop_on_signals()?;
    let mut daemon = Daemon::new(config, dirs.clone(), Mode::Once { apps, workers })?;
    daemon.run(&stop)
}

fn service_main(dirs: &Dirs) -> Result<()> {
    init_logging();
    let config = load_config(dirs)?;
    let stop = stop_on_signals()?;
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "prebake service starting");
    let mut daemon = Daemon::new(config, dirs.clone(), Mode::Service)?;
    daemon.run(&stop)
}

fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_env("PREBAKE_LOG").unwrap_or_else(|_| "info".into());
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .without_time()
        .init();
}

/// Blocks SIGTERM, SIGINT and SIGHUP in every thread and waits for them on a dedicated one,
/// so a shutdown request lets the current replay stop cleanly.
fn stop_on_signals() -> Result<Arc<AtomicBool>> {
    let mut set = SigSet::empty();
    for signal in [Signal::SIGTERM, Signal::SIGINT, Signal::SIGHUP] {
        set.add(signal);
    }
    set.thread_block().context("blocking signals")?;
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    std::thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            if let Ok(signal) = set.wait() {
                tracing::info!(%signal, "stopping");
            }
            flag.store(true, Ordering::Relaxed);
        })
        .context("starting the signal thread")?;
    Ok(stop)
}

fn configure_steam(dirs: &Dirs, background: Switch, threads: Option<u32>) -> Result<()> {
    let config = load_config(dirs)?;
    let steam = steam::Steam::locate(config.steam_root.as_deref())?;
    let running = procs::scan(&steam.root, None).steam_running;
    let enabled = matches!(background, Switch::On);
    steamcfg::set_background(&steam, running, enabled, threads)?;
    println!(
        "Steam background shader processing is {}{}; start Steam again to apply it",
        if enabled { "on" } else { "off" },
        threads.map(|t| format!(" with {t} threads")).unwrap_or_default()
    );
    Ok(())
}

fn doctor(dirs: &Dirs) -> Result<()> {
    let ok = |label: &str, text: String| println!("  ok    {label:<14} {text}");
    let warn = |label: &str, text: String| println!("  note  {label:<14} {text}");
    let fail = |label: &str, text: String| println!("  fail  {label:<14} {text}");

    let config = match load_config(dirs) {
        Ok(config) => {
            ok(
                "config",
                format!("{}{}", dirs.config.display(), if dirs.config.exists() { "" } else { " (defaults)" }),
            );
            config
        }
        Err(err) => {
            fail("config", format!("{err:#}"));
            return Ok(());
        }
    };
    let steam = match steam::Steam::locate(config.steam_root.as_deref()) {
        Ok(steam) => {
            ok("steam", format!("{} ({} libraries)", steam.root.display(), steam.libraries.len()));
            steam
        }
        Err(err) => {
            if steam::flatpak_only() {
                fail(
                    "steam",
                    "only a Flatpak Steam is installed; prebake needs the native package".into(),
                );
            } else {
                fail("steam", format!("{err:#}"));
            }
            return Ok(());
        }
    };
    let fossilize = steam.fossilize_replay();
    if fossilize.is_file() {
        ok("fossilize", fossilize.display().to_string());
    } else {
        fail("fossilize", format!("{} is missing; start Steam once", fossilize.display()));
    }
    let device = match gpu::probe_in_child() {
        Ok(devices) => match gpu::choose(&devices, config.device_index) {
            Some(device) => {
                ok(
                    "gpu",
                    format!("{} (device {}, cache key {})", device.name, device.index, device.uuid),
                );
                Some(device.clone())
            }
            None => {
                fail("gpu", format!("no usable Vulkan device among {}", devices.len()));
                None
            }
        },
        Err(err) => {
            fail("gpu", format!("{err:#}"));
            None
        }
    };
    let steam_config = steam.config().unwrap_or_default();
    match plan::build(&steam, &steam_config, &config.exclude) {
        Ok(plans) => {
            let groups: usize = plans.iter().map(|p| p.groups.len()).sum();
            let records: state::Records = state::read_json(&dirs.records());
            let done = device.as_ref().map_or(0, |d| {
                plans
                    .iter()
                    .flat_map(|p| p.groups.iter())
                    .filter(|g| records.done(g, &d.uuid))
                    .count()
            });
            ok(
                "caches",
                format!(
                    "{} games with Steam shader caches, {} replays, {} baked; Steam has {} games queued",
                    plans.len(),
                    groups,
                    done,
                    steam_config.processing_queue.len()
                ),
            );
        }
        Err(err) => fail("caches", format!("{err:#}")),
    }
    match presence::probe_idle_source() {
        Some(source) => ok("idle", source.to_string()),
        None => warn(
            "idle",
            "no idle source in this session; prebake will only bake while the screen is locked".into(),
        ),
    }
    if presence::gamemode_available() {
        ok("gamemode", "GameMode answers; games it knows about pause baking too".into());
    } else {
        warn("gamemode", "not running; Steam games still pause baking".into());
    }
    let threads = steamcfg::background_threads(&steam)
        .map(|t| format!(", {t} threads"))
        .unwrap_or_default();
    if steam_config.background_processing {
        ok(
            "steam bg",
            format!("on{threads}: Steam finishes baked games before you launch them"),
        );
    } else {
        warn(
            "steam bg",
            "off: a baked game still shows a short \"Processing Vulkan shaders\" pass at launch. To let Steam finish that pass in the background, quit Steam and run: prebake steam --background on --threads 2".into(),
        );
    }
    let (active, enabled) = service::state();
    let status: Status = state::read_json(&dirs.status());
    if active == "active" && status.live(now()) {
        ok("service", format!("{active}, {enabled}; {}", status.reason));
    } else {
        warn("service", format!("{active}, {enabled}; install it with: prebake install"));
    }
    Ok(())
}
