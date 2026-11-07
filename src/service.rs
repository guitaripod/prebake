use anyhow::{Context, Result, bail};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const UNIT: &str = "prebake.service";
const PACKAGED: [&str; 2] = [
    "/usr/lib/systemd/user/prebake.service",
    "/usr/local/lib/systemd/user/prebake.service",
];

/// The systemd user unit prebake runs as. `KillMode=mixed` lets the daemon stop the replay
/// itself on shutdown, so Fossilize gets to kill its own workers.
pub fn unit_text(exe: &Path) -> String {
    format!(
        "[Unit]\n\
Description=prebake: bake Steam's Vulkan shader caches while you are away\n\
Documentation=https://github.com/guitaripod/prebake\n\
PartOf=graphical-session.target\n\
After=graphical-session.target\n\
\n\
[Service]\n\
Type=simple\n\
ExecStart={} daemon\n\
Restart=on-failure\n\
RestartSec=30\n\
KillMode=mixed\n\
TimeoutStopSec=20\n\
\n\
[Install]\n\
WantedBy=graphical-session.target\n",
        exe.display()
    )
}

fn user_unit_path() -> Result<PathBuf> {
    let base = directories::BaseDirs::new().context("finding the home directory")?;
    Ok(base.config_dir().join("systemd/user").join(UNIT))
}

/// Writes the user unit (unless a packaged one exists) and enables and starts it. A user
/// unit left from a Cargo install would shadow the packaged one, so it is removed then.
pub fn install() -> Result<String> {
    let packaged = PACKAGED.iter().any(|p| Path::new(p).exists());
    let mut written = String::new();
    let user_unit = user_unit_path()?;
    if packaged && user_unit.exists() {
        fs::remove_file(&user_unit).with_context(|| format!("removing {}", user_unit.display()))?;
        systemctl(&["daemon-reload"])?;
        written = format!("removed {}, which shadowed the packaged unit", user_unit.display());
    }
    if !packaged {
        let exe = std::env::current_exe().context("locating the prebake executable")?;
        let exe = exe.canonicalize().unwrap_or(exe);
        let path = user_unit_path()?;
        fs::create_dir_all(path.parent().expect("unit path has a parent"))?;
        fs::write(&path, unit_text(&exe)).with_context(|| format!("writing {}", path.display()))?;
        written = path.display().to_string();
        systemctl(&["daemon-reload"])?;
    }
    systemctl(&["enable", "--now", UNIT])?;
    systemctl(&["restart", UNIT])?;
    Ok(written)
}

/// Stops and disables the service and removes the user unit. Returns whether a unit file
/// was removed; a packaged unit stays until the package is removed.
pub fn uninstall() -> Result<bool> {
    let (_, enabled) = state();
    if enabled != "not-found" {
        systemctl(&["disable", "--now", UNIT])?;
    }
    let path = user_unit_path()?;
    if !path.exists() {
        return Ok(false);
    }
    fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    systemctl(&["daemon-reload"])?;
    Ok(true)
}

/// `systemctl --user is-active` and `is-enabled` for the unit.
pub fn state() -> (String, String) {
    let query = |verb: &str| {
        Command::new("systemctl")
            .args(["--user", verb, UNIT])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|_| "unknown".into())
    };
    (query("is-active"), query("is-enabled"))
}

fn systemctl(args: &[&str]) -> Result<()> {
    let status = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .status()
        .context("running systemctl --user")?;
    if !status.success() {
        bail!("systemctl --user {} failed", args.join(" "));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unit_runs_the_daemon_in_the_graphical_session() {
        let text = unit_text(Path::new("/usr/bin/prebake"));
        assert!(text.contains("ExecStart=/usr/bin/prebake daemon\n"));
        assert!(text.contains("PartOf=graphical-session.target\n"));
        assert!(text.contains("KillMode=mixed\n"));
        assert!(text.ends_with("WantedBy=graphical-session.target\n"));
    }

    #[test]
    fn the_packaged_unit_matches_the_generated_one() {
        let packaged = include_str!("../packaging/prebake.service");
        assert_eq!(packaged, unit_text(Path::new("/usr/bin/prebake")));
    }
}
