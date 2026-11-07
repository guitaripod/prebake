use anyhow::{Context, Result, bail};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notification_v1::{self, ExtIdleNotificationV1};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notifier_v1::ExtIdleNotifierV1;
use x11rb::connection::Connection as _;
use x11rb::protocol::screensaver::ConnectionExt as _;

const POLL: Duration = Duration::from_secs(3);
const RETRY: Duration = Duration::from_secs(15);

/// What prebake knows about the person at the computer, updated by background threads.
#[derive(Debug, Default)]
pub struct Presence {
    idle: AtomicBool,
    locked: AtomicBool,
    games: AtomicU32,
    source: Mutex<String>,
}

impl Presence {
    pub fn away(&self) -> bool {
        self.idle.load(Ordering::Relaxed) || self.locked.load(Ordering::Relaxed)
    }

    pub fn locked(&self) -> bool {
        self.locked.load(Ordering::Relaxed)
    }

    /// Games registered with Feral GameMode.
    pub fn gamemode_games(&self) -> u32 {
        self.games.load(Ordering::Relaxed)
    }

    pub fn source(&self) -> String {
        self.source.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn set_source(&self, source: &str) {
        *self.source.lock().unwrap_or_else(|e| e.into_inner()) = source.to_string();
    }
}

/// Starts the idle and session watchers. Idle time comes from the Wayland
/// `ext-idle-notify-v1` protocol (KDE Plasma, Hyprland, sway, niri, COSMIC and other
/// wlroots or Smithay compositors), from Mutter's IdleMonitor on GNOME, or from the X11
/// screensaver extension. A locked screen always counts as away.
pub fn watch(idle_after: Duration, respect_inhibitors: bool) -> Arc<Presence> {
    let presence = Arc::new(Presence::default());
    presence.set_source("starting");
    let idle = Arc::clone(&presence);
    std::thread::Builder::new()
        .name("idle".into())
        .spawn(move || watch_idle(idle, idle_after, respect_inhibitors))
        .expect("spawning the idle watcher");
    let session = Arc::clone(&presence);
    std::thread::Builder::new()
        .name("session".into())
        .spawn(move || watch_session(&session))
        .expect("spawning the session watcher");
    presence
}

fn watch_idle(presence: Arc<Presence>, idle_after: Duration, respect_inhibitors: bool) {
    let timeout_ms = u32::try_from(idle_after.as_millis()).unwrap_or(u32::MAX).max(1000);
    loop {
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            match run_wayland(&presence, timeout_ms, respect_inhibitors) {
                Ok(never) => match never {},
                Err(WaylandError::Unsupported) => {}
                Err(WaylandError::Lost(err)) => {
                    presence.idle.store(false, Ordering::Relaxed);
                    tracing::warn!("Wayland idle watcher stopped: {err:#}; retrying");
                    std::thread::sleep(RETRY);
                    continue;
                }
            }
        }
        if poll_idle(&presence, idle_after, respect_inhibitors, "GNOME IdleMonitor", mutter_idle_ms).is_some() {
            continue;
        }
        if x11_session() && poll_idle(&presence, idle_after, respect_inhibitors, "X11 screensaver", x11_idle_ms).is_some() {
            continue;
        }
        presence.idle.store(false, Ordering::Relaxed);
        presence.set_source("screen lock only");
        std::thread::sleep(RETRY * 4);
    }
}

/// Polls an idle-time source until it stops answering. Returns `None` when it never
/// answered at all, so the caller can try the next source.
fn poll_idle(presence: &Presence, idle_after: Duration, respect_inhibitors: bool, name: &str, read: fn() -> Result<u64>) -> Option<()> {
    read().ok()?;
    presence.set_source(name);
    while let Ok(ms) = read() {
        let idle = Duration::from_millis(ms) >= idle_after && !(respect_inhibitors && session_inhibited());
        presence.idle.store(idle, Ordering::Relaxed);
        std::thread::sleep(POLL);
    }
    presence.idle.store(false, Ordering::Relaxed);
    Some(())
}

/// An X11 session, not XWayland inside a Wayland one: XWayland only sees input aimed at X11
/// windows, so its idle time grows while you type into native Wayland apps.
fn x11_session() -> bool {
    std::env::var_os("DISPLAY").is_some() && std::env::var_os("WAYLAND_DISPLAY").is_none()
}

/// True when an app asked the session not to go idle (GNOME's session manager, or the
/// freedesktop power-management inhibit interface elsewhere).
fn session_inhibited() -> bool {
    const IDLE: u32 = 8;
    zbus::block_on(async {
        let Ok(conn) = session_bus().await else { return false };
        if let Ok(reply) = conn
            .call_method(
                Some("org.gnome.SessionManager"),
                "/org/gnome/SessionManager",
                Some("org.gnome.SessionManager"),
                "IsInhibited",
                &(IDLE,),
            )
            .await
            && let Ok(inhibited) = reply.body().deserialize::<bool>()
        {
            return inhibited;
        }
        if let Ok(reply) = conn
            .call_method(
                Some("org.freedesktop.PowerManagement"),
                "/org/freedesktop/PowerManagement/Inhibit",
                Some("org.freedesktop.PowerManagement.Inhibit"),
                "HasInhibit",
                &(),
            )
            .await
            && let Ok(inhibited) = reply.body().deserialize::<bool>()
        {
            return inhibited;
        }
        false
    })
}

enum WaylandError {
    Unsupported,
    Lost(anyhow::Error),
}

struct WaylandState {
    presence: Arc<Presence>,
}

fn run_wayland(presence: &Arc<Presence>, timeout_ms: u32, respect_inhibitors: bool) -> Result<std::convert::Infallible, WaylandError> {
    let conn = Connection::connect_to_env().map_err(|_| WaylandError::Unsupported)?;
    let (globals, mut queue) = registry_queue_init::<WaylandState>(&conn).map_err(|e| WaylandError::Lost(e.into()))?;
    let qh = queue.handle();
    let notifier: ExtIdleNotifierV1 = globals.bind(&qh, 1..=2, ()).map_err(|_| WaylandError::Unsupported)?;
    let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=1, ()).map_err(|e| WaylandError::Lost(e.into()))?;
    let _notification = if !respect_inhibitors && notifier.version() >= 2 {
        notifier.get_input_idle_notification(timeout_ms, &seat, &qh, ())
    } else {
        notifier.get_idle_notification(timeout_ms, &seat, &qh, ())
    };
    presence.idle.store(false, Ordering::Relaxed);
    presence.set_source(if !respect_inhibitors && notifier.version() >= 2 {
        "Wayland ext-idle-notify, input only"
    } else {
        "Wayland ext-idle-notify, screen-awake apps count as present"
    });
    let mut state = WaylandState {
        presence: Arc::clone(presence),
    };
    loop {
        queue.blocking_dispatch(&mut state).map_err(|e| WaylandError::Lost(e.into()))?;
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for WaylandState {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for WaylandState {
    fn event(_: &mut Self, _: &wl_seat::WlSeat, _: wl_seat::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ExtIdleNotifierV1, ()> for WaylandState {
    fn event(_: &mut Self, _: &ExtIdleNotifierV1, _: <ExtIdleNotifierV1 as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ExtIdleNotificationV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _: &ExtIdleNotificationV1,
        event: ext_idle_notification_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_idle_notification_v1::Event::Idled => state.presence.idle.store(true, Ordering::Relaxed),
            ext_idle_notification_v1::Event::Resumed => state.presence.idle.store(false, Ordering::Relaxed),
            _ => {}
        }
    }
}

/// Names the idle source the service would use, for `prebake doctor`.
pub fn probe_idle_source() -> Option<&'static str> {
    if std::env::var_os("WAYLAND_DISPLAY").is_some()
        && let Ok(conn) = Connection::connect_to_env()
        && let Ok((globals, _queue)) = registry_queue_init::<WaylandState>(&conn)
        && globals
            .contents()
            .with_list(|list| list.iter().any(|g| g.interface == "ext_idle_notifier_v1"))
    {
        return Some("Wayland ext-idle-notify-v1");
    }
    if mutter_idle_ms().is_ok() {
        return Some("GNOME IdleMonitor");
    }
    if x11_session() && x11_idle_ms().is_ok() {
        return Some("X11 screensaver");
    }
    None
}

/// True when Feral GameMode answers on the session bus.
pub fn gamemode_available() -> bool {
    gamemode_clients().is_ok()
}

fn mutter_idle_ms() -> Result<u64> {
    zbus::block_on(async {
        let conn = session_bus().await?;
        let ms: u64 = conn
            .call_method(
                Some("org.gnome.Mutter.IdleMonitor"),
                "/org/gnome/Mutter/IdleMonitor/Core",
                Some("org.gnome.Mutter.IdleMonitor"),
                "GetIdletime",
                &(),
            )
            .await?
            .body()
            .deserialize()?;
        Ok(ms)
    })
}

fn x11_idle_ms() -> Result<u64> {
    let (conn, screen) = x11rb::connect(None).context("connecting to X11")?;
    let root = conn.setup().roots[screen].root;
    let reply = conn.screensaver_query_info(root)?.reply()?;
    Ok(u64::from(reply.ms_since_user_input))
}

fn watch_session(presence: &Presence) {
    let mut bus_was_up = true;
    loop {
        let bus_up = bus_alive();
        if bus_up != bus_was_up {
            if bus_up {
                tracing::info!("session bus is back");
            } else {
                tracing::warn!("session bus unreachable; screen lock and GameMode are unknown until it returns");
            }
            bus_was_up = bus_up;
        }
        presence.locked.store(bus_up && screen_locked().unwrap_or(false), Ordering::Relaxed);
        presence
            .games
            .store(if bus_up { gamemode_clients().unwrap_or(0) } else { 0 }, Ordering::Relaxed);
        std::thread::sleep(POLL);
    }
}

/// Pings the bus daemon and drops the cached connection when it does not answer, so the
/// next call reconnects after a session bus restart.
fn bus_alive() -> bool {
    let alive = zbus::block_on(async {
        let conn = session_bus().await?;
        conn.call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "GetId",
            &(),
        )
        .await?;
        anyhow::Ok(())
    })
    .is_ok();
    if !alive {
        *BUS.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
    alive
}

fn screen_locked() -> Result<bool> {
    zbus::block_on(async {
        let conn = session_bus().await?;
        for (service, path) in [
            ("org.freedesktop.ScreenSaver", "/org/freedesktop/ScreenSaver"),
            ("org.gnome.ScreenSaver", "/org/gnome/ScreenSaver"),
        ] {
            if let Ok(reply) = conn.call_method(Some(service), path, Some(service), "GetActive", &()).await
                && let Ok(active) = reply.body().deserialize::<bool>()
            {
                return Ok(active);
            }
        }
        bail!("no screensaver service answered")
    })
}

fn gamemode_clients() -> Result<u32> {
    zbus::block_on(async {
        let conn = session_bus().await?;
        let reply = conn
            .call_method(
                Some("com.feralinteractive.GameMode"),
                "/com/feralinteractive/GameMode",
                Some("org.freedesktop.DBus.Properties"),
                "Get",
                &("com.feralinteractive.GameMode", "ClientCount"),
            )
            .await?;
        let value: zbus::OwnedVariant = reply.body().deserialize()?;
        let count = i32::try_from(value)?;
        Ok(count.max(0) as u32)
    })
}

static BUS: Mutex<Option<zbus::Connection>> = Mutex::new(None);

async fn session_bus() -> Result<zbus::Connection> {
    let cached = BUS.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if let Some(conn) = cached.filter(|c| !c.is_closed()) {
        return Ok(conn);
    }
    let conn = zbus::Connection::session().await.context("connecting to the session bus")?;
    *BUS.lock().unwrap_or_else(|e| e.into_inner()) = Some(conn.clone());
    Ok(conn)
}

/// True when the machine runs on battery: a battery is present and no mains supply is online.
pub fn on_battery() -> bool {
    let Ok(entries) = std::fs::read_dir("/sys/class/power_supply") else {
        return false;
    };
    let mut battery = false;
    for entry in entries.flatten() {
        let path = entry.path();
        let kind = std::fs::read_to_string(path.join("type")).unwrap_or_default();
        match kind.trim() {
            "Mains" | "USB" => {
                if std::fs::read_to_string(path.join("online")).is_ok_and(|v| v.trim() == "1") {
                    return false;
                }
            }
            "Battery" => {
                battery |= std::fs::read_to_string(path.join("scope")).map_or(true, |s| s.trim() != "Device");
            }
            _ => {}
        }
    }
    battery
}
