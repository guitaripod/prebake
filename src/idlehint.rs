use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use zbus::OwnedObjectPath;

const LOGIN1: &str = "org.freedesktop.login1";
const SESSION: &str = "org.freedesktop.login1.Session";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
const RETRY: Duration = Duration::from_secs(60);
const RECHECK: Duration = Duration::from_secs(5 * 60);
const CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// The parts of systemd-logind that the idle hint needs.
pub trait Logind {
    /// The object path of the user's graphical session, if one is open.
    fn display_session(&mut self) -> Result<Option<String>>;
    /// The session's `IdleSinceHint`: 0 until anything ever set its idle hint.
    fn idle_since(&mut self, session: &str) -> Result<u64>;
    fn set_idle(&mut self, session: &str, idle: bool) -> Result<()>;
}

/// Mirrors prebake's idea of "away" into logind's idle hint on the user's graphical session,
/// so other tools that wait for an idle session (SCSKiller's "When idle", logind's
/// `IdleAction`) wait for the person too, on desktops that never set the hint themselves,
/// such as KDE Plasma.
pub struct IdleHint<L: Logind> {
    logind: L,
    attempted: Option<bool>,
    dirty: Option<String>,
    marker: Option<PathBuf>,
    checked_at: Option<Instant>,
    retry_at: Option<Instant>,
}

impl IdleHint<SystemLogind> {
    pub fn system(marker: PathBuf) -> Self {
        Self::new(SystemLogind::default()).with_marker(marker)
    }
}

/// Clears a hint that a killed prebake left at idle, when the next start no longer shares it.
pub fn clear_left_over(marker: &Path) {
    if marker.exists() {
        drop(IdleHint::system(marker.to_path_buf()));
        remove_marker(marker);
    }
}

impl<L: Logind> IdleHint<L> {
    pub fn new(logind: L) -> Self {
        Self {
            logind,
            attempted: None,
            dirty: None,
            marker: None,
            checked_at: None,
            retry_at: None,
        }
    }

    /// Keeps the session whose hint may read idle in `marker` as well, so a prebake that was
    /// killed before it could clear the hint has the next start clear it.
    pub fn with_marker(mut self, marker: PathBuf) -> Self {
        self.dirty = std::fs::read_to_string(&marker)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        self.marker = Some(marker);
        self
    }

    /// Sets the hint at once when `idle` changed, and re-asserts it every few minutes in case
    /// logind restarted. A failure is logged once and retried a minute later, or on every call
    /// while a hint prebake set may still read idle with the person back.
    pub fn publish(&mut self, idle: bool, now: Instant) {
        let changed = self.attempted != Some(idle);
        if !changed && !self.due(idle, now) {
            return;
        }
        self.attempted = Some(idle);
        match self.apply(idle) {
            Ok(()) => {
                if self.retry_at.take().is_some() {
                    tracing::info!("logind takes the idle hint again");
                }
                if changed {
                    tracing::info!(idle, "logind idle hint");
                }
                self.checked_at = Some(now);
            }
            Err(err) => {
                if self.retry_at.is_none() {
                    tracing::warn!("setting logind's idle hint: {err:#}; retrying");
                }
                self.checked_at = None;
                self.retry_at = Some(now + RETRY);
            }
        }
    }

    /// An unchanged hint is sent again when the last check is older than [`RECHECK`], once a
    /// failure's retry time has come, or straight away while a failed clear leaves it idle.
    fn due(&self, idle: bool, now: Instant) -> bool {
        match self.retry_at {
            Some(_) if !idle && self.dirty.is_some() => true,
            Some(at) => now >= at,
            None => self.checked_at.is_none_or(|at| now.duration_since(at) >= RECHECK),
        }
    }

    /// `dirty` names the session whose hint may be idle from the moment prebake asks for it
    /// until a present is confirmed, so a failure in between still gets cleared.
    fn apply(&mut self, idle: bool) -> Result<()> {
        let Some(session) = self.logind.display_session()? else {
            return Ok(());
        };
        if idle {
            self.mark(Some(session.clone()));
            return self.logind.set_idle(&session, true);
        }
        self.forget_other_session(&session);
        if self.logind.idle_since(&session)? == 0 {
            self.mark(Some(session.clone()));
            self.stamp_present(&session)?;
        } else {
            self.logind.set_idle(&session, false)?;
        }
        self.mark(None);
        Ok(())
    }

    /// A hint left on a session that is no longer the display session (an earlier login's)
    /// gets one try at present and is then let go.
    fn forget_other_session(&mut self, session: &str) {
        if let Some(other) = self.dirty.clone().filter(|o| o != session) {
            if let Err(err) = self.logind.set_idle(&other, false) {
                tracing::debug!("clearing the idle hint of {other}: {err:#}");
            }
            self.mark(None);
        }
    }

    /// A hint that was never set reads as `IdleSinceHint` 0, which readers such as SCSKiller
    /// take as "idle time unknown" and treat like idle. Flipping it to idle and straight back
    /// leaves a timestamp, so they read "present" until prebake says otherwise.
    fn stamp_present(&mut self, session: &str) -> Result<()> {
        self.logind.set_idle(session, true)?;
        self.logind.set_idle(session, false)
    }

    fn mark(&mut self, session: Option<String>) {
        if self.dirty == session {
            return;
        }
        if let Some(marker) = &self.marker {
            match &session {
                Some(s) => {
                    if let Err(err) = write_marker(marker, s) {
                        tracing::warn!("writing {}: {err:#}", marker.display());
                    }
                }
                None => remove_marker(marker),
            }
        }
        self.dirty = session;
    }

    /// Hands the session back as present when prebake stops, so a dead prebake never leaves
    /// other tools thinking nobody is at the computer.
    fn release(&mut self) {
        let Some(session) = self.dirty.clone() else { return };
        match self.logind.set_idle(&session, false) {
            Ok(()) => self.mark(None),
            Err(err) => tracing::warn!("clearing logind's idle hint: {err:#}"),
        }
    }
}

impl<L: Logind> Drop for IdleHint<L> {
    fn drop(&mut self) {
        self.release();
    }
}

fn write_marker(marker: &Path, session: &str) -> Result<()> {
    if let Some(dir) = marker.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(marker, session)?;
    Ok(())
}

fn remove_marker(marker: &Path) {
    if let Err(err) = std::fs::remove_file(marker)
        && err.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!("removing {}: {err:#}", marker.display());
    }
}

/// logind on the system bus.
#[derive(Default)]
pub struct SystemLogind {
    conn: Option<zbus::Connection>,
}

impl SystemLogind {
    fn call<T>(
        &mut self,
        path: &str,
        interface: &str,
        method: &str,
        body: &(impl serde::Serialize + zbus::DynamicType),
        read: impl FnOnce(zbus::Message) -> Result<T>,
    ) -> Result<T> {
        let result = zbus::block_on(async {
            let conn = match self.conn.clone().filter(|c| !c.is_closed()) {
                Some(conn) => conn,
                None => {
                    let conn = zbus::connection::Builder::system()
                        .method_timeout(CALL_TIMEOUT)
                        .build()
                        .await
                        .context("connecting to the system bus")?;
                    self.conn = Some(conn.clone());
                    conn
                }
            };
            let reply = conn
                .call_method(Some(LOGIN1), path, Some(interface), method, body)
                .await
                .with_context(|| format!("{interface}.{method} on {path}"))?;
            anyhow::Ok(reply)
        });
        match result {
            Ok(reply) => read(reply),
            Err(err) => {
                self.conn = None;
                Err(err)
            }
        }
    }

    fn property(&mut self, path: &str, interface: &str, name: &str) -> Result<zbus::OwnedVariant> {
        self.call(path, PROPERTIES, "Get", &(interface, name), |reply| {
            Ok(reply.body().deserialize::<zbus::OwnedVariant>()?)
        })
    }
}

impl Logind for SystemLogind {
    fn display_session(&mut self) -> Result<Option<String>> {
        let value = self.property("/org/freedesktop/login1/user/self", "org.freedesktop.login1.User", "Display")?;
        let (_, path) = <(String, OwnedObjectPath)>::try_from(value).context("reading the user's display session")?;
        Ok(Some(path.as_str().to_string()).filter(|p| p != "/"))
    }

    fn idle_since(&mut self, session: &str) -> Result<u64> {
        let value = self.property(session, SESSION, "IdleSinceHint")?;
        u64::try_from(value).context("reading IdleSinceHint")
    }

    fn set_idle(&mut self, session: &str, idle: bool) -> Result<()> {
        self.call(session, SESSION, "SetIdleHint", &(idle,), |_| Ok(()))
    }
}

/// What `prebake doctor` reports: the session the hint goes to and logind's `IdleAction`.
pub fn describe() -> Result<String> {
    let mut logind = SystemLogind::default();
    let Some(session) = logind.display_session()? else {
        bail!("no graphical session is open, so there is nothing to set the hint on");
    };
    let kind = String::try_from(logind.property(&session, SESSION, "Type")?)?;
    if !matches!(kind.as_str(), "wayland" | "x11" | "mir") {
        bail!("{session} is a {kind} session; logind takes idle hints only from graphical ones");
    }
    let action = String::try_from(logind.property("/org/freedesktop/login1", "org.freedesktop.login1.Manager", "IdleAction")?)?;
    let id = String::try_from(logind.property(&session, SESSION, "Id")?)?;
    Ok(if action == "ignore" {
        format!("shared with logind on {kind} session {id}")
    } else {
        format!("shared with logind on {kind} session {id}; its IdleAction={action} will act while you are away")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Default)]
    struct Fake {
        session: Option<String>,
        since: u64,
        hint: bool,
        fail: bool,
        fail_clear: bool,
        calls: Rc<RefCell<Vec<String>>>,
    }

    impl Logind for Fake {
        fn display_session(&mut self) -> Result<Option<String>> {
            self.calls.borrow_mut().push("display".into());
            if self.fail {
                bail!("bus down");
            }
            Ok(self.session.clone())
        }

        fn idle_since(&mut self, _: &str) -> Result<u64> {
            self.calls.borrow_mut().push("since".into());
            Ok(self.since)
        }

        fn set_idle(&mut self, _: &str, idle: bool) -> Result<()> {
            self.calls.borrow_mut().push(format!("set {idle}"));
            if self.fail_clear && !idle {
                bail!("logind refused");
            }
            if self.hint != idle {
                self.hint = idle;
                self.since = 1;
            }
            Ok(())
        }
    }

    fn fake() -> (Fake, Rc<RefCell<Vec<String>>>) {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let fake = Fake {
            session: Some("/org/freedesktop/login1/session/_33".into()),
            calls: Rc::clone(&calls),
            ..Fake::default()
        };
        (fake, calls)
    }

    fn take(calls: &Rc<RefCell<Vec<String>>>) -> Vec<String> {
        std::mem::take(&mut *calls.borrow_mut())
    }

    #[test]
    fn a_never_set_hint_gets_a_present_timestamp() {
        let (logind, calls) = fake();
        let mut hint = IdleHint::new(logind);
        hint.publish(false, Instant::now());
        assert_eq!(take(&calls), ["display", "since", "set true", "set false"]);
        assert!(!hint.logind.hint);
        assert_ne!(hint.logind.since, 0);
        assert_eq!(hint.dirty, None);
    }

    #[test]
    fn only_changes_reach_logind_until_the_recheck() {
        let (logind, calls) = fake();
        let mut hint = IdleHint::new(logind);
        let start = Instant::now();
        hint.publish(false, start);
        take(&calls);
        hint.publish(false, start + Duration::from_secs(2));
        assert!(take(&calls).is_empty());
        hint.publish(true, start + Duration::from_secs(4));
        assert_eq!(take(&calls), ["display", "set true"]);
        assert!(hint.logind.hint);
        hint.publish(true, start + Duration::from_secs(6));
        assert!(take(&calls).is_empty());
        hint.publish(true, start + Duration::from_secs(4) + RECHECK);
        assert_eq!(take(&calls), ["display", "set true"]);
        hint.publish(false, start + Duration::from_secs(6) + RECHECK);
        assert_eq!(take(&calls), ["display", "since", "set false"]);
        assert!(!hint.logind.hint);
    }

    #[test]
    fn a_restarted_logind_is_stamped_again_at_the_recheck() {
        let (logind, calls) = fake();
        let mut hint = IdleHint::new(logind);
        let start = Instant::now();
        hint.publish(false, start);
        hint.logind.since = 0;
        take(&calls);
        hint.publish(false, start + RECHECK);
        assert_eq!(take(&calls), ["display", "since", "set true", "set false"]);
        assert_ne!(hint.logind.since, 0);
    }

    #[test]
    fn failures_back_off_for_a_minute() {
        let (mut logind, calls) = fake();
        logind.fail = true;
        let mut hint = IdleHint::new(logind);
        let start = Instant::now();
        hint.publish(true, start);
        hint.publish(true, start + Duration::from_secs(30));
        assert_eq!(take(&calls), ["display"]);
        hint.logind.fail = false;
        hint.publish(true, start + RETRY);
        assert_eq!(take(&calls), ["display", "set true"]);
        assert!(hint.logind.hint);
    }

    #[test]
    fn coming_back_is_not_held_up_by_a_failed_retry() {
        let (logind, calls) = fake();
        let mut hint = IdleHint::new(logind);
        let start = Instant::now();
        hint.publish(true, start);
        hint.logind.fail = true;
        hint.publish(true, start + RECHECK);
        hint.logind.fail = false;
        take(&calls);
        hint.publish(false, start + RECHECK + Duration::from_secs(2));
        assert_eq!(take(&calls), ["display", "since", "set false"]);
        assert!(!hint.logind.hint);
    }

    #[test]
    fn a_failed_clear_is_still_released_on_stop() {
        let (logind, calls) = fake();
        let mut hint = IdleHint::new(logind);
        let start = Instant::now();
        hint.publish(true, start);
        hint.logind.fail_clear = true;
        hint.publish(false, start + Duration::from_secs(2));
        assert!(hint.logind.hint);
        hint.logind.fail_clear = false;
        take(&calls);
        hint.release();
        assert_eq!(take(&calls), ["set false"]);
        assert!(!hint.logind.hint);
    }

    #[test]
    fn a_stamp_cut_short_is_still_released_on_stop() {
        let (mut logind, calls) = fake();
        logind.fail_clear = true;
        let mut hint = IdleHint::new(logind);
        hint.publish(false, Instant::now());
        assert!(hint.logind.hint);
        hint.logind.fail_clear = false;
        take(&calls);
        drop(hint);
        assert_eq!(take(&calls), ["set false"]);
    }

    #[test]
    fn no_graphical_session_sets_nothing() {
        let (mut logind, calls) = fake();
        logind.session = None;
        let mut hint = IdleHint::new(logind);
        hint.publish(true, Instant::now());
        assert_eq!(take(&calls), ["display"]);
        assert!(!hint.logind.hint);
    }

    #[test]
    fn stopping_hands_the_session_back_as_present() {
        let (logind, calls) = fake();
        let mut hint = IdleHint::new(logind);
        hint.publish(true, Instant::now());
        take(&calls);
        hint.release();
        assert_eq!(take(&calls), ["set false"]);
        assert!(!hint.logind.hint);
        drop(hint);
        assert!(take(&calls).is_empty());
    }

    #[test]
    fn stopping_while_present_leaves_logind_alone() {
        let (logind, calls) = fake();
        let mut hint = IdleHint::new(logind);
        hint.publish(false, Instant::now());
        take(&calls);
        drop(hint);
        assert!(take(&calls).is_empty());
    }

    #[test]
    fn a_failed_clear_is_retried_on_every_call() {
        let (logind, calls) = fake();
        let mut hint = IdleHint::new(logind);
        let start = Instant::now();
        hint.publish(true, start);
        hint.logind.fail_clear = true;
        hint.publish(false, start + Duration::from_secs(2));
        hint.logind.fail_clear = false;
        take(&calls);
        hint.publish(false, start + Duration::from_secs(4));
        assert_eq!(take(&calls), ["display", "since", "set false"]);
        assert!(!hint.logind.hint);
        hint.publish(false, start + Duration::from_secs(6));
        assert!(take(&calls).is_empty());
    }

    #[test]
    fn the_marker_follows_a_hint_that_may_read_idle() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("idle-hint");
        let (logind, _calls) = fake();
        let mut hint = IdleHint::new(logind).with_marker(marker.clone());
        let start = Instant::now();
        hint.publish(true, start);
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "/org/freedesktop/login1/session/_33");
        hint.publish(false, start + Duration::from_secs(2));
        assert!(!marker.exists());
    }

    #[test]
    fn a_killed_prebakes_hint_is_cleared_from_its_marker() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("idle-hint");
        std::fs::write(&marker, "/org/freedesktop/login1/session/_33").unwrap();
        let (mut logind, calls) = fake();
        logind.hint = true;
        logind.since = 7;
        let hint = IdleHint::new(logind).with_marker(marker.clone());
        assert_eq!(hint.dirty.as_deref(), Some("/org/freedesktop/login1/session/_33"));
        drop(hint);
        assert_eq!(take(&calls), ["set false"]);
        assert!(!marker.exists());
    }

    #[test]
    fn an_earlier_sessions_hint_is_cleared_and_let_go() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("idle-hint");
        std::fs::write(&marker, "/org/freedesktop/login1/session/_32").unwrap();
        let (logind, calls) = fake();
        let mut hint = IdleHint::new(logind).with_marker(marker.clone());
        hint.publish(false, Instant::now());
        assert_eq!(take(&calls), ["display", "set false", "since", "set true", "set false"]);
        assert_eq!(hint.dirty, None);
        assert!(!marker.exists());
    }
}
