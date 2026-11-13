# prebake

Bakes Steam's Vulkan shader caches while you are away from the computer, so games start without **"Processing Vulkan shaders"**.

Steam on Linux downloads pipeline caches for your games and records new ones while you play, but on a desktop it only turns them into driver shaders when you press Play. After a driver update, a Proton update or a big shader download, that can mean ten minutes or more of a progress bar before a game starts. prebake does that work ahead of time: when you step away it runs Steam's own `fossilize_replay` on every installed game's caches, newest games first, and stops the moment you come back or start a game.

```
$ prebake status
Baking God of War Ragnarök (2 of 2) · 27 workers (27 running) · 41% of 814,727 · you are away
GPU       NVIDIA RTX PRO 6000 Blackwell Workstation Edition (cache key d7567106)
Presence  away (Wayland ext-idle-notify, input only)
Steam     background processing off: Steam finishes each baked game in a short pass at launch

14 ready · 48 waiting · 1 baking · 0 with Steam · 0 failed

Game                                      State      To compile
God of War Ragnarök                       baking        480,689
UNCHARTED™: Legacy of Thieves Collection  waiting       201,377
Cyberpunk 2077                            waiting        12,191
…
```

## How it works

- **Same replay as Steam.** prebake finds the replays Steam would run for each game (the base pipeline cache plus one per current Proton/driver bucket, read from Steam's own config and shader log), and runs Steam's `ubuntu12_64/fossilize_replay` on them with the arguments and environment Steam uses. The driver writes its cache into the game's `steamapps/shadercache/<appid>` directory, exactly where the game reads it.
- **Steam skips what is baked.** Fossilize records every replayed pipeline in a replayer cache keyed by your GPU and driver (`replay_cache.<key>.foz`). prebake merges those records the way Steam does, so Steam's next pass over the same game skips everything prebake already compiled.
- **Steers, not just starts.** prebake talks to `fossilize_replay` over its control socket, the same channel Steam uses, to change how many workers run. Workers pause instantly and resume where they left off.
- **Never in your way.**
  - Away (no keyboard or mouse input for `idle_after`, or the screen is locked): one worker per CPU thread minus one, within memory.
  - At the computer: `workers_present` workers, 0 by default, so nothing runs.
  - A game starts (any `reaper SteamLaunch`, or a game registered with Feral GameMode): every worker stops at once; after 20 seconds the replay is shut down to free memory.
  - Steam starts its own replay for a game: prebake leaves that game to Steam.
  - On battery, or `prebake pause`: nothing runs.
  - Workers run at nice 19, `SCHED_BATCH` and idle I/O priority, like Steam's own.
  - Memory: workers are counted against what the running replay really holds, so a heavy game runs fewer of them, and the service is capped at 35% of RAM with no swap, so a replay never pushes your desktop into swap.
- **Starts over when it should.** A driver update changes the cache key, and a new shader download changes the databases; either way the affected games are baked again.

Idle time comes from the Wayland `ext-idle-notify-v1` protocol (KDE Plasma, Hyprland, sway, niri, COSMIC and other wlroots or Smithay compositors), from Mutter's IdleMonitor on GNOME, or from the X11 screensaver extension.

## What Steam still does

Steam keeps its own list of games whose shaders changed. For a game prebake has baked, Steam's pass is quick because every pipeline is already in the replayer cache, and Steam uses it to fold the driver's per-process cache files into the file the game reads. Who runs that pass is up to Steam's "Allow background processing of Vulkan shaders" setting:

- **Off** (the desktop default): Steam runs it when you press Play, so a baked game still shows "Processing Vulkan shaders", but only while Steam checks what prebake already compiled (it compiles only what is new since).
- **On**: Steam runs it in the background and baked games start straight away. prebake can turn it on for you, with a small thread budget so Steam's own share stays light:

  ```
  steam -shutdown
  prebake steam --background on --threads 2
  ```

  Then start Steam again. Steam keeps settings in memory and rewrites them when it exits, so the change is only made while Steam is closed; the old `config.vdf` is saved as `config.vdf.prebake-backup`.

## Install

Arch Linux (AUR):

```
yay -S prebake
systemctl --user enable --now prebake
```

With Cargo:

```
cargo install prebake
prebake install
```

`prebake install` writes `~/.config/systemd/user/prebake.service` (or uses the packaged unit), then enables and starts it. Linux release tarballs are on the [releases page](https://github.com/guitaripod/prebake/releases).

Then check the setup:

```
prebake doctor
```

## Use

| Command | What it does |
| --- | --- |
| `prebake` / `prebake status` | What is baked, what is waiting, what is happening now (`--all`, `--json`) |
| `prebake now [--for 3h]` | Bake now, even while you are at the computer |
| `prebake pause [--for 2h]` | Stop baking for a while |
| `prebake resume` | Cancel `pause` and `now` |
| `prebake run [APPID…] [--workers N]` | Bake in the foreground without the service |
| `prebake doctor` | Check Steam, the GPU, idle detection and the service |
| `prebake steam --background on\|off [--threads N]` | Set Steam's own background processing (Steam closed) |
| `prebake config` | Print the config path and the defaults |
| `prebake install` / `uninstall` | Manage the systemd user service |

Logs: `journalctl --user -u prebake`. `PREBAKE_LOG=debug` adds per-pipeline crash reports from Fossilize.

## Configuration

`~/.config/prebake/config.toml`, every setting optional:

```toml
idle_after = "5m"          # no input for this long counts as away
workers_away = 0           # 0 = one per CPU thread minus one, within memory
workers_present = 0        # 0 = wait until you leave
respect_inhibitors = false # true: apps that keep the screen awake (music, video, calls) count as you being present
run_on_battery = false
idle_hint = false          # true: set logind's idle hint while you are away and no game runs (see below)
pipeline_timeout = 60      # seconds one pipeline may take before Fossilize skips it
exclude = []               # Steam app ids to leave alone
# steam_root = "/home/you/.local/share/Steam"
# device_index = 0         # Vulkan device; default is the first discrete GPU
```

`idle_hint` is for desktops that never set logind's idle hint, such as KDE Plasma: other background tools that wait for an idle session, like SCSKiller's "When idle" compiles, then wait until you are away and pause the moment you are back or a game starts. For them an app that keeps the screen awake (a film, a call, a game's window) also counts as you being present, until three hours pass without input (compositors without ext-idle-notify v2 keep it present for as long as the app does). Leave it off on GNOME, which sets the hint itself. logind's own `IdleAction` acts on the hint too, so leave it off if you set one in `logind.conf`. `prebake doctor` shows the session it goes to.

## What it touches

Only the files Steam's own replay touches, in each game's `steamapps/shadercache/<appid>`: the driver cache (`nvidiav1/`, `mesa_shader_cache_sf/`, `AMDv1/`) and Fossilize's `replay_cache.*.foz` and `steam_pipeline_cache_whitelist*.foz`. Merges write a new file and rename it into place. prebake's own state lives in `~/.local/state/prebake`. With `idle_hint` on it also sets logind's idle hint on your graphical session, and hands it back as present when it stops.

## Requirements and limits

- Linux with the native Steam client (the Flatpak build is not supported: its games use the Flatpak runtime's driver, not the host's).
- A Vulkan driver: NVIDIA, Mesa (RADV, ANV, NVK) or AMDVLK.
- Non-Steam shortcuts are not baked; Steam does not process them either.
- Games that render with OpenGL have nothing to bake; Direct3D games under Proton and native Vulkan games do.

## License

GPL-3.0-only. Made by [Midgar](https://midgarcorp.cc).
