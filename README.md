# Jetty

Minimal, fast GUI terminal workspace for running AI agent CLIs in folders, with one binary that also runs the engine headless. Linux/Wayland first.

## What works today

- The engine owns sessions and PTYs; a typed request/response/stream protocol runs over an in-process duplex.
- GUI: session sidebar, a create-session form (name, directory, command), one live terminal pane per session, and a quit dialog offering "Kill Sessions" or "Leave Running".
- `jetty headless` runs the engine with no window (no transport yet, so it idles).
- 31 tests cover the engine, session store, input path, and form logic.

## Run

```
cargo run              # GUI
cargo run -- headless  # engine only
cargo test             # unit tests
```

Requirements: Rust 1.99+, Wayland, xkbcommon, Vulkan. The first build clones the pinned Zed revision for `gpui`/`gpui_platform`.

Configuration: one JSON file, `$XDG_CONFIG_HOME/jetty/sessions.json` (fallback `~/.config/jetty/sessions.json`). On first run it is seeded with `shell` in `$HOME`; every session is just a name, a directory, and a command, so any CLI works.

## Decided, not built yet

- WebSocket transport (tokio-tungstenite + rustls) and a real attachable daemon.
- rusqlite snapshots and Loro CRDTs for session transcripts and the workspace registry.
- Markdown rendering (pulldown-cmark), our own syntax highlighter, our own theme, Geist/Geist Mono.
- Reattach to sessions left running.

## Out of scope

Windows and macOS, mouse text selection, and scrollback navigation (the vendored terminal component does not implement it).

## Vendoring and licensing

- `vendor/gpui-terminal` is crates.io `gpui-terminal` 0.1.0 with two API fixes and its stale binary target removed; see `vendor/gpui-terminal/PATCHES.md`. Its MIT/Apache-2.0 license files are kept.
- Only Apache-2.0 Zed crates are used (`gpui`, `gpui_platform`, `gpui_tokio`, and their support crates). Zed's GPL-3.0 crates (`ui`, `markdown`, `theme`, `editor`) are deliberately avoided.

MIT licensed. See `AGENTS.md` for the decided architecture and the working rules.