# Changelog

All notable changes to prebake are listed here. The format follows [Keep a Changelog](https://keepachangelog.com), and versions follow [Semantic Versioning](https://semver.org).

## [Unreleased]

### Added
- `idle_hint = true` sets logind's idle hint on your graphical session while you are away and no game runs, and hands it back as present when you return or prebake stops, so other tools that wait for an idle session (SCSKiller's "When idle" compiles) wait for you on desktops that never set the hint, such as KDE Plasma. For them an app keeping the screen awake (a film, a call) counts as present for up to three hours without input. `prebake doctor` shows the session it goes to.

### Changed
- Replays fit in memory instead of finding out by pushing the desktop into swap: each worker is first assumed to need 1 GiB (Diablo IV measures about 1.2 GiB per worker on NVIDIA, where the old 400 MiB guess let 27 workers take 32 GiB and fill swap), the running replay's real memory replaces the estimate as it grows, an eighth of RAM (at least 4 GiB) stays free for the desktop, and the worker count stays inside the service's own memory limit.
- The service unit caps prebake at 35% of RAM with no swap (`MemoryMax=35%`, `MemorySwapMax=0`) and keeps running when the kernel ends a worker at that limit (`OOMPolicy=continue`); Fossilize replaces the worker. Run `prebake install` again to update a unit written by an earlier version.

## [0.1.0] - 2026-11-07

### Added
- A systemd user service that bakes Steam's Vulkan shader caches with Steam's own `fossilize_replay` while you are away, newest games first, so games start without "Processing Vulkan shaders".
- Replays match Steam's: the same databases per game (base cache plus the current Proton and driver buckets), the same arguments and the same driver cache locations, and replayer-cache fragments are merged the way Steam merges them, so Steam skips everything prebake compiled.
- Workers are steered over Fossilize's control socket: all of them while you are away, `workers_present` (none by default) while you are at the computer, frozen the moment a game starts and shut down if it keeps running, and capped by available memory.
- Idle detection through Wayland `ext-idle-notify-v1`, GNOME's IdleMonitor or the X11 screensaver extension; a locked screen counts as away, and games registered with Feral GameMode count as games.
- `prebake status`, `now`, `pause`, `resume`, `run`, `doctor`, `install` and `uninstall`.
- `prebake steam --background on|off --threads N` sets Steam's own background shader processing while Steam is closed, keeping a backup of `config.vdf`.
