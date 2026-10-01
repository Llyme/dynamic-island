# NADI — Not Another Dynamic Island

A Dynamic Island for the Windows desktop. A small pill sits at the top edge of the screen with a pair of
eyes, and shows what is going on: what is playing, which game or project you are in, what is downloading,
which notifications came in, what is next on your calendar. Click it and it expands into a hub panel.

NADI is built with [Tauri v2](https://v2.tauri.app/): a Rust backend and a vanilla JS/canvas frontend.
It is Windows-only for now. The goal is to stay light: it should never make the computer lag.

## What it shows

- **Eyes.** The idle pill. They follow the cursor and react to the sound playing on the system. The sound's
  light can bleed outside the island, like YouTube's ambient mode.
- **Media.** Now-playing with controls and a seek bar, from the Windows media session.
- **Games and work.** Detects running games, and what you are working on, even when the window is not
  focused. The coding card shows the git changes of the project (branch, files, added and deleted lines);
  the browsing card shows what is in the page and gives you buttons to click it from the island.
- **Downloads.** Active downloads from browsers, Steam and qBittorrent, in one card. Finished ones open
  their folder or can be put away with a click.
- **Notifications.** Captures the notifications of all apps.
- **Calendar.** Upcoming events from an `.ics` feed, with a reminder.
- **Claude usage.** Usage rings and a peek, using your existing Claude Code login.
- **Hub.** The expanded panel. Every card starts collapsed and opens with a click on its header.

To call the island, rest the cursor on the top edge of a monitor: a glow builds up, and the island lands.
Right-click the island to pin or unpin it. Settings are in the tray icon's menu.

## Requirements

- Windows 10 or 11 (with the WebView2 runtime, which is part of Windows 11)
- [Rust](https://rustup.rs/) and the Tauri CLI: `cargo install tauri-cli --version "^2"`

## Run and build

```powershell
cd src-tauri
cargo tauri dev      # run it
cargo tauri build    # release exe and NSIS installer
```

The installer is written to `src-tauri/target/release/bundle/nsis/`. Tests: `cargo test` in `src-tauri/`
(a few tests that need real apps running are marked `#[ignore]`).

Debug aids, set as environment variables before starting the exe: `DI_OPEN_HUB=1` opens the hub,
`DI_OPEN_SETTINGS=1` opens the settings pane, `DI_DEMO_DOWNLOADS=1` shows fake downloads.

## Where things live

- `ui/` is the frontend: `index.html`, `main.js`, `style.css` for the compact island and `hub.css` for
  the hub. There is no build step.
- `src-tauri/src/` is the backend. `lib.rs` has the window state machine (reveal, hide, springs, resize);
  there is one module per feature: `media`, `audio`, `game`, `work`, `project`, `browse`, `pagetext`,
  `downloads`, `notify` and `notify_listener`, `calendar`, `usage`, `stats` and `gpu`, `settings`.
- `scripts/gen_icons.py` regenerates `src-tauri/icons/` (standard library only).
- Settings are stored in `%APPDATA%\NADI\settings.json`.

## How it works

The window is one frameless, transparent, always-on-top Tauri window. A 16 ms loop in `lib.rs` polls the
global cursor position (the webview only gets mouse events while the cursor is over it) and drives
everything: the edge dwell and glow, the slide in and out, the spring resize between views, and
click-through. The window is a little larger than the island, and the margin is click-through, so the
glow can shine outside the island without blocking the windows underneath.

Background work (game and work detection, downloads, OCR of the page text) runs on low-priority threads
and only does what is needed while something is shown.
