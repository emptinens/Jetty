# Jetty

Minimal, fast GUI AI agent orchestrator with a CLI: terminal sessions that run any agent CLI in a folder. Linux/Wayland first. MIT, public repo.

## Decided (do not re-open)

- Rust, gpui + gpui_platform (zed git, pinned rev). Linux/Wayland first. No Electron, Tauri, Slint, webview. No Zed fork.
- In-app terminal: vendored gpui-terminal + portable-pty, spawned by Jetty. No custom VTE parser.
- Session = name + directory + command. Any CLI, no presets.
- Defaults: command `$SHELL`, directory `$HOME`. A session bound to a project opens its CLI in that folder.
- Persistence: one JSON file in the config dir (`$XDG_CONFIG_HOME/jetty/sessions.json`, fallback `~/.config`).
- Quit: dialog, leave running or kill. Reattach works only for sessions left running.

## Workflow

- Small increments: build after each change, run the app when behavior changed, verify before push.
- New dependency needs an explicit yes.
- Checkpoint with the user before the next big piece. Do not one-shot a plan.

## Build

```
cargo run     # opens the window
cargo build   # compile check
```

Needs Rust 1.99+, wayland, xkbcommon, vulkan (all present on the dev box).

## Stack facts (probe-verified)

- `gpui_platform` has no crates.io release: deps are zed git at revision `ba8159b`, plus `[patch.crates-io] gpui = { git = "https://github.com/zed-industries/zed", rev = "ba8159b" }`.
- `vendor/gpui-terminal` is upstream 0.1.0 with 2 fixes (see its PATCHES.md). Delete its stale `src/main.rs` and `[[bin]]`: upstream's example does not compile on current gpui and breaks workspace builds.
- gpui API: `use gpui::AppContext` for `cx.new`; clone a `FocusHandle` before `focus(window, cx)`.
- Quit interception: `Window::on_window_should_close`, `App::on_app_quit`.
- Terminal: `TerminalView::new(writer, reader, config, cx)` + `with_resize_callback` (resize the PTY there) + `with_exit_callback`. Default config font `"monospace"` at 14px.
- Vendored code keeps its upstream MIT/Apache license files.

## Out of scope (for now)

Agent and CLI orchestration layer, the `jetty` CLI binary, reattach to running sessions, mouse selection, scrollback navigation (gpui-terminal lacks it), Windows and macOS.