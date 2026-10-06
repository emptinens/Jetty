# Jetty

Minimal, fast GUI AI agent orchestrator with a CLI: terminal sessions that run any agent CLI in a folder. One binary, headed and headless. Linux/Wayland first. MIT, public repo.

## Decided (do not re-open)

- Rust, gpui + gpui_platform (zed git, pinned rev), GPU-rendered, Wayland/X11 on Linux. No Electron, Tauri, Slint, webview. No Zed fork.
- Zed crates: the Apache-2.0 set only (gpui, gpui_platform, gpui_tokio, gpui_util, gpui_macros, gpui_shared_string, collections, http_client, util_macros). `ui`, `markdown`, `theme`, `editor` are GPL-3.0-or-later: never depend on them. Own markdown, own theme, own components.
- One binary: `jetty` runs the GUI; `jetty headless` runs the engine as a Linux daemon. The engine owns sessions and PTYs; GUI and CLI are clients.
- Async: Tokio, bridged into the UI with gpui_tokio.
- RPC: typed request/response/stream. In-memory duplex when the engine is in-process; WebSocket (tokio-tungstenite + rustls) across processes.
- Data: Loro (loro + loro-protocol) for session transcripts and the workspace registry; rusqlite for local snapshots; sync only when signed in.
- Text: pulldown-cmark; our own syntax highlighter. Geist and Geist Mono.
- In-app terminal: vendored gpui-terminal + portable-pty. No custom VTE parser.
- Session = name + directory + command. Any CLI, no presets. Defaults `$SHELL` and `$HOME`; a session bound to a project opens its CLI in that folder.
- Today the registry is one JSON file (`$XDG_CONFIG_HOME/jetty/sessions.json`); rusqlite and Loro replace it when that layer lands.
- Quit: dialog, leave running or kill. Reattach works only for sessions left running.

## Workflow

- Small increments: build after each change, run the app when behavior changed, verify before push.
- New dependency needs an explicit yes.
- Checkpoint with the user before the next big piece. Do not one-shot a plan.

## Build

```
cargo run              # GUI
cargo run -- headless  # engine only
cargo test             # unit tests
```

Needs Rust 1.99+, wayland, xkbcommon, vulkan (all present on the dev box).

## Stack facts (probe-verified)

- `gpui_platform` has no crates.io release: deps are zed git at revision `ba8159b4d324d137e08993d85fb023b484388ede`, plus `[patch.crates-io] gpui = { git = "https://github.com/zed-industries/zed", rev = "ba8159b4d324d137e08993d85fb023b484388ede" }` once gpui-terminal is vendored.
- `vendor/gpui-terminal` is upstream 0.1.0 with 2 fixes (see its PATCHES.md). Delete its stale `src/main.rs` and `[[bin]]`: upstream's example does not compile on current gpui and breaks workspace builds.
- gpui API: `use gpui::AppContext` for `cx.new`; clone a `FocusHandle` before `focus(window, cx)`; a clickable div needs `.id(...)` (`InteractiveElement` in scope) and `.on_click` (`StatefulInteractiveElement` in scope).
- gpui_tokio: `init(cx)` installs the tokio runtime, `Tokio::spawn(cx, fut)` and `Tokio::handle(cx)` bridge work both ways.
- Quit interception: `Window::on_window_should_close`, `App::on_app_quit`.
- Terminal: `TerminalView::new(writer, reader, config, cx)` + `with_resize_callback` (resize the PTY there) + `with_exit_callback`.
- Vendored code keeps its upstream MIT/Apache license files.

## Out of scope (for now)

Reattach to running sessions, mouse selection, scrollback navigation (gpui-terminal lacks it), accounts and sync, Windows and macOS.