# Changelog

All notable changes to prebake are listed here. The format follows [Keep a Changelog](https://keepachangelog.com), and versions follow [Semantic Versioning](https://semver.org).

## [Unreleased]

## [0.1.0] - 2026-11-07

### Added
- A systemd user service that bakes Steam's Vulkan shader caches with Steam's own `fossilize_replay` while you are away, newest games first, so games start without "Processing Vulkan shaders".
- Replays match Steam's: the same databases per game (base cache plus the current Proton and driver buckets), the same arguments and the same driver cache locations, and replayer-cache fragments are merged the way Steam merges them, so Steam skips everything prebake compiled.
- Workers are steered over Fossilize's control socket: all of them while you are away, `workers_present` (none by default) while you are at the computer, frozen the moment a game starts and shut down if it keeps running, and capped by available memory.
- Idle detection through Wayland `ext-idle-notify-v1`, GNOME's IdleMonitor or the X11 screensaver extension; a locked screen counts as away, and games registered with Feral GameMode count as games.
- `prebake status`, `now`, `pause`, `resume`, `run`, `doctor`, `install` and `uninstall`.
- `prebake steam --background on|off --threads N` sets Steam's own background shader processing while Steam is closed, keeping a backup of `config.vdf`.
