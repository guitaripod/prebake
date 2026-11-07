use crate::state::{AppState, AppStatus, Status};
use std::fmt::Write;
use std::time::Duration;

/// Formats the status for a terminal. Ready games are summarised unless `all` is set.
pub fn status(status: &Status, live: bool, all: bool) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{}", headline(status, live));
    if let Some(device) = &status.device {
        let _ = writeln!(out, "GPU       {} (cache key {})", device.name, short(&device.uuid));
    }
    if live {
        let _ = writeln!(out, "Presence  {} ({})", status.presence, status.idle_source);
    }
    let _ = writeln!(
        out,
        "Steam     background processing {}",
        if status.steam_background_processing {
            "on: Steam finishes baked games on its own"
        } else {
            "off: Steam finishes each baked game in a short pass at launch"
        }
    );
    if let Some(error) = &status.last_error {
        let _ = writeln!(out, "Error     {error}");
    }

    let count = |state: AppState| status.apps.iter().filter(|a| a.state == state).count();
    let _ = writeln!(
        out,
        "\n{} ready · {} waiting · {} baking · {} with Steam · {} failed",
        count(AppState::Ready),
        count(AppState::Waiting),
        count(AppState::Baking),
        count(AppState::Steam),
        count(AppState::Failed)
    );
    let shown: Vec<&AppStatus> = status.apps.iter().filter(|a| all || a.state != AppState::Ready).collect();
    if shown.is_empty() {
        let _ = writeln!(out, "Every game with a Steam shader cache is baked.");
        return out;
    }
    let width = shown.iter().map(|a| a.name.chars().count().min(40)).max().unwrap_or(4).max(4);
    let _ = writeln!(out, "\n{:<width$}  {:<8}  {:>11}", "Game", "State", "To compile");
    for app in shown {
        let name: String = app.name.chars().take(40).collect();
        let pending = match app.pending {
            Some(n) => group_digits(n as u64),
            None if app.state == AppState::Ready => "0".into(),
            None => "unscanned".into(),
        };
        let _ = writeln!(out, "{name:<width$}  {:<8}  {pending:>11}", state_word(app.state));
    }
    out
}

fn headline(status: &Status, live: bool) -> String {
    if !live {
        return "prebake is not running (start it with: prebake install)".into();
    }
    let count = |state: AppState| status.apps.iter().filter(|a| a.state == state).count();
    match &status.job {
        Some(job) if job.running == 0 => format!("Holding {}: workers paused ({})", job.name, status.reason),
        Some(job) => {
            let p = job.progress;
            let percent = (p.done * 100).checked_div(p.total).unwrap_or(0);
            format!(
                "Baking {} ({} of {}) · {} workers ({} running) · {}% of {} · {}",
                job.name,
                job.group_index,
                job.groups,
                job.workers,
                job.running,
                percent,
                group_digits(p.total),
                status.reason
            )
        }
        None => match status.activity.as_str() {
            "done" if count(AppState::Failed) + count(AppState::Steam) == 0 => "Everything is baked; watching for new shaders".into(),
            "done" => format!(
                "Nothing more to bake now: {} with Steam, {} failed and retried later",
                count(AppState::Steam),
                count(AppState::Failed)
            ),
            "paused" => match status.paused_until {
                Some(until) => format!(
                    "Paused for {} more (prebake resume to continue)",
                    humantime::format_duration(Duration::from_secs(until.saturating_sub(status.updated) / 60 * 60))
                ),
                None => "Paused".into(),
            },
            "scanning" => "Checking shader caches".into(),
            other => format!("{} ({})", capitalize(other), status.reason),
        },
    }
}

fn state_word(state: AppState) -> &'static str {
    match state {
        AppState::Ready => "ready",
        AppState::Waiting => "waiting",
        AppState::Baking => "baking",
        AppState::Steam => "steam",
        AppState::Failed => "failed",
    }
}

fn short(uuid: &str) -> &str {
    &uuid[..uuid.len().min(8)]
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

pub fn group_digits(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::JobStatus;

    #[test]
    fn groups_digits() {
        assert_eq!(group_digits(0), "0");
        assert_eq!(group_digits(999), "999");
        assert_eq!(group_digits(1000), "1,000");
        assert_eq!(group_digits(660_077), "660,077");
        assert_eq!(group_digits(12_345_678), "12,345,678");
    }

    #[test]
    fn shows_waiting_games_and_summarises_ready_ones() {
        let app = |id, name: &str, state, pending| AppStatus {
            app: id,
            name: name.into(),
            state,
            pipelines: None,
            pending,
            last_played: 0,
        };
        let status = Status {
            pid: 1,
            activity: "baking".into(),
            reason: "you are away".into(),
            presence: "away".into(),
            idle_source: "Wayland ext-idle-notify".into(),
            job: Some(JobStatus {
                app: 2322010,
                name: "God of War Ragnarök".into(),
                group: "3554f158047e28bf".into(),
                group_index: 2,
                groups: 2,
                workers: 27,
                running: 27,
                ..JobStatus::default()
            }),
            apps: vec![
                app(2322010, "God of War Ragnarök", AppState::Baking, Some(412_104)),
                app(1091500, "Cyberpunk 2077", AppState::Ready, None),
                app(730, "Counter-Strike 2", AppState::Waiting, None),
            ],
            ..Status::default()
        };
        let text = super::status(&status, true, false);
        assert!(text.starts_with("Baking God of War Ragnarök (2 of 2) · 27 workers (27 running)"));
        assert!(text.contains("1 ready · 1 waiting · 1 baking"));
        assert!(text.contains("412,104"));
        assert!(text.contains("unscanned"));
        assert!(!text.contains("Cyberpunk"));
        assert!(super::status(&status, true, true).contains("Cyberpunk"));

        let mut held = status.clone();
        held.job.as_mut().unwrap().running = 0;
        held.reason = "a game is running".into();
        assert!(super::status(&held, true, false).starts_with("Holding God of War Ragnarök: workers paused (a game is running)"));

        let mut paused = status.clone();
        paused.job = None;
        paused.activity = "paused".into();
        paused.updated = 1000;
        paused.paused_until = Some(1000 + 3600 + 59);
        assert!(super::status(&paused, true, false).starts_with("Paused for 1h more"));

        let mut finished = paused;
        finished.activity = "done".into();
        finished.apps[0].state = AppState::Failed;
        assert!(super::status(&finished, true, false).starts_with("Nothing more to bake now: 0 with Steam, 1 failed"));
    }
}
