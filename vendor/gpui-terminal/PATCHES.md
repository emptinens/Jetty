# gpui-terminal vendored patches

Source: crates.io `gpui-terminal` 0.1.0 (upstream repo `zortax/gpui-terminal`), MIT OR Apache-2.0.
`LICENSE-MIT` and `LICENSE-APACHE` are kept as shipped.

Verified against zed `gpui` revision `ba8159b4d324d137e08993d85fb023b484388ede`.

## Local changes

1. `src/view.rs` in `on_mouse_down`: `window.focus(&self.focus_handle)` became
   `window.focus(&self.focus_handle, cx)` (`Window::focus` gained the `cx` argument).
2. `src/render.rs` text painting: `shaped_line.paint(origin, line_height, window, cx)` became
   `shaped_line.paint(origin, line_height, gpui::TextAlign::default(), None, window, cx)`
   (`ShapedLine::paint` gained `align` and `align_width`).
3. Removed the upstream `[[bin]]` target and `src/main.rs`: that example does not compile against
   current gpui (1-arg `FocusHandle::focus`, `Application::new`) and would break workspace builds.
   Also dropped crates.io packaging artifacts (`Cargo.toml.orig`, `Cargo.lock`,
   `.cargo_vcs_info.json`, `.cargo-ok`).