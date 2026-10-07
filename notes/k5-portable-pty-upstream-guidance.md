# k5: portable-pty upstream guidance on killing descendants

Date: 2026-10-07. Upstream is NOT github.com/wezterm/portable-pty (404, repo does not
exist). crates.io metadata says `repository: https://github.com/wezterm/wezterm` — the
crate is the `pty/` subdirectory of the wezterm monorepo; issues live in
wezterm/wezterm. Engine pins portable-pty 0.9.0 (verified in local cargo registry).

## (a) README/docs

- No `pty/README.md` in the monorepo (raw fetch returns 404; only a stub exists).
- Doc comments are the only in-source guidance (pinned 0.9.0, local cargo registry):
  - `MasterPty::process_group_leader()`: "If applicable to the type of the tty, return
    the local process id of the process group or session leader" — implemented via
    `libc::tcgetpgrp(master_fd)` (src/unix.rs:374-378).
  - `ChildKiller::kill`: "Terminate the child process" (src/lib.rs:152).
- Key: spawn topology makes the child a session leader with the pty as controlling
  terminal: `pre_exec` calls `libc::setsid()` then `ioctl(0, TIOCSCTTY, 0)` when
  `get_controlling_tty()` (src/unix.rs:238-271). Descendants are in the child's session.

## (b) Intended semantics of Child::kill / maintainer guidance

Pinned 0.9.0 source, `impl ChildKiller for std::process::Child` (src/lib.rs:341-372):
- Unix: `libc::kill(child_pid, SIGHUP)` to the DIRECT child pid only (not killpg, no
  -pid). SIGHUP is chosen because "the default behavior of a process receiving this
    signal is to killed unless it configured a signal handler".
- Then a grace loop: 5 attempts x 50ms polling `try_wait()` ("we give the process a bit
  of a grace period ... befre we proceed with the full on kill").
- If still alive: falls through to `std::process::Child::kill` (SIGKILL, direct pid).
- `ProcessSignaller::kill` (the `clone_killer` path, src/lib.rs:325-333) is weaker:
  SIGHUP to pid only, no grace loop, no fallback — never guarantees death.
- Windows: TerminateProcess with exit code 127 (src/lib.rs:303-313); post-0.9.0 fix
  #7709 corrected inverted return handling.

I.e. `Child::kill` is explicitly per-process, NOT group-kill. Orphaned descendants are
handled by the kernel: when the last master fd closes, the controlling terminal hangs
up and the kernel delivers SIGHUP to the foreground process group of the session
(vhangup semantics). SIGHUP-blocking processes (su, runuser, nohup-style daemons) can
linger — this is the documented gap.

Maintainer guidance (wez, issue #7898 "Do not send `\n` + EOF when unix pty is
dropped", https://github.com/wezterm/wezterm/issues/7898, comment 2026-07-06):
- On the `UnixMasterWriter::drop` hack that writes `\n` + VEOF into the pty:
  "I agree that this is nasty. If we remove it, it will regress those users of
  portable-pty, but this is an incomplete hack that assumes something about the shell
  attached, which is not necessarily true, and sometimes harmful. Does removing it
  break wezterm's pane closure? There's chance that it might cause something to
  linger. If that works fine, then my vote is just to remove that code..."
- Referenced https://github.com/wezterm/wezterm/discussions/2392#discussioncomment-3380751
  (why master EOF/hangup works: "The reason that EOF isn't signalled is because the
  master end of the pty is used for both read and write, so dropping just the write
  side doesn't close the underlying handle: you need it open to continue reading
  from it.")

Known orphan/harm issues: #5101 "WezTerm killing tmux window when closed" (EOF kills
tmux), #4317 "closing tab or application closes tmux window on mac", #5994,
#7898 itself; fix PR #8226 "pty: hang up the pty when closing a pane instead of
sending newline + EOF" (open, NOT merged as of 2026-10-07; main still contains the
`UnixMasterWriter::drop` `\n`+VEOF hack at pty/src/unix.rs:393-407). PR #8226 body:
local pane readers hold their own master handle, so the kernel hangup only happens
when the pane's reader thread drops it too; without that, `su`/`runuser` (SIGHUP
blocked) linger. wez's stance: kernel SIGHUP-on-hangup is the intended cleanup
mechanism; the EOF hack is legacy and harmful.

## (c) Version-bump path (engine pins 0.9.0)

- crates.io: portable-pty 0.9.0 (published 2025-02-11 by wez) is the NEWEST and
  MAX version. There is no >0.9.0 release. Nothing to bump to.
- Post-0.9.0 commits touching pty/ in wezterm monorepo (GitHub API, since
  2025-02-11): "pty: Fix typo psuedo => pseudo (#7178)", "pty: Replace winapi with
  windows-sys (#8073)", "filedescriptor: Replace winapi with windows-sys (#8026)",
  "pty: windows: fix kill() (#7709)" (Windows-only TerminateProcess return-code fix),
  plus an unrelated default_prog split fixup. NO change to Unix spawn topology
  (setsid/TIOCSCTTY unchanged), NO group-kill API, NO preexec hook API added.
- Conclusion: a version bump gains nothing for descendant cleanup. If group-kill is
  wanted, the consumer must do it (killpg) or pin the monorepo rev with #8226 once
  merged.

## (d) Consumer examples

1. Zed (crates/terminal/src/pty_info.rs + terminal.rs, main branch):
   - `ProcessIdGetter::pid()` = `libc::tcgetpgrp(master_fd)` (zed calls it "the
     foreground process group"; 0 => none; fallback to spawned child pid).
     Note: this duplicates portable-pty's own `MasterPty::process_group_leader()`
     because zed holds a duplicated master fd, not the MasterPty trait object.
   - `kill_current_process()` = `libc::killpg(pgid, SIGKILL)` — kills the foreground
     process group (the command running in the shell), NOT the shell.
   - `terminate_child_process()` = `libc::killpg(pgid, SIGTERM)` (graceful close).
   - `kill_child_process()` = `Child::kill()` (portable-pty SIGHUP path on the shell).
   - `Terminal::kill_active_task()` (terminal.rs:3119-3143): killpg foreground group
     FIRST, then kill the shell child, so the terminal exits and task completion
     fires. Unit test at terminal.rs:6108+ verifies kill of `sleep 60` child.
   - URLs: https://github.com/zed-industries/zed/blob/main/crates/terminal/src/pty_info.rs
     https://github.com/zed-industries/zed/blob/main/crates/terminal/src/terminal.rs
2. Wezterm itself (the reference consumer): pane close = drop the pane's handles; the
   kernel hangs up the pty when the last master fd closes, SIGHUP-ing the session;
   the `\n`+EOF hack is being removed in open PR #8226 (see (b)).

## Concrete recommendation for the Jetty engine (src/session.rs)

1. On session kill: `let pgid = master.process_group_leader()` (or
   `tcgetpgrp(dup_master_fd)` like zed) then `libc::killpg(pgid, SIGKILL)`, then
   `child.kill()` (SIGHUP + grace + SIGKILL fallback for the shell itself). Order:
   foreground group first, then shell (matches zed).
2. Closing the session must drop the reader + writer + master handles so the kernel
   hangs up the tty (this, not Child::kill, is what reaps SIGHUP-blocking
   descendants... except SIGHUP-blockers like su/runuser, which only a killpg or
   closing-the-last-fd hangup catches).
3. Beware the 0.9.0 `UnixMasterWriter::drop` `\n`+VEOF hack: dropping the writer
   alone can inject a newline into the running command (running half-typed input!)
   and EOF can kill tmux-like multiplexers. Drop the whole pty (or keep writer until
   reader ends), don't just drop the writer early. Upstream fix (#8226) is not
   released.
4. `clone_killer()` returns the weak ProcessSignaller (SIGHUP only, no fallback);
   prefer keeping the real Child for kill.
5. No upstream version bump helps; group-kill must be implemented by Jetty.

## Validation

- Reproduced all code claims against the pinned 0.9.0 source in the local cargo
  registry (file:line refs above), not just docs.rs.
- Verified wezterm main's pty/src/unix.rs still has the EOF drop hack (PR #8226 not
  merged) via raw fetch, and monorepo pty/ commit list via GitHub API.
- Verified crates.io version list: 0.9.0 is max_stable_version (no >0.9.0 exists).
- Cross-checked zed's kill path by fetching both terminal.rs and pty_info.rs from
  raw.githubusercontent and grepping locally.
- Issue titles/URLs/bodies and wez's comment re-parsed from the GitHub API JSON (not
  scraped HTML) for accuracy.
- Web search budget: ~10 fetches total, within the 8-12 limit.

## Open questions

- Whether helix uses killpg for its pty (not checked — zed + wezterm suffice per task).
- Whether wezterm's mux pane kill calls Child::kill anywhere (pane close path relies
  on hangup; direct calls not traced).
- Exact SIGHUP-on-last-close behavior for non-controlling-tty spawns
  (`get_controlling_tty()` false) — descendants would NOT get kernel SIGHUP then.
- Whether PR #8226's reader-drain approach will be merged as-is (open as of today).

## what_i_did_not_check

- did not read gpui-terminal (vendored) kill path: vendor/gpui-terminal does not
  exist in this working tree yet (only AGENTS.md, Cargo.toml, src/, target/ present;
  vendoring apparently not yet done).
- did not check Windows/macOS kill paths in consumers (Linux-only scope).
- did not run any code; evidence is source + API reads only.
- did not check docs.rs rendering of portable-pty for extra doc guidance beyond the
  crate source doc comments (same text, lower value).
- did not verify whozim/other small consumers or the `CommandBuilder::get_controlling_tty`
  default value (assumed true for default openpty+spawn_command usage; jetty's
  engine.rs uses default CommandBuilder).

## URLs (key evidence)

- https://github.com/wezterm/wezterm/issues/7898 (wez guidance, EOF hack)
- https://github.com/wezterm/wezterm/pull/8226 (hang-up fix, open)
- https://github.com/wezterm/wezterm/issues/5101, #4317 (EOF kills tmux)
- https://github.com/wezterm/wezterm/discussions/2392 (EOF/hangup mechanics)
- https://github.com/wezterm/wezterm/pull/7709 (windows kill fix, post-0.9.0)
- https://crates.io/api/v1/crates/portable-pty (0.9.0 = newest, 2025-02-11)
- https://github.com/zed-industries/zed/blob/main/crates/terminal/src/pty_info.rs (killpg)
- https://github.com/zed-industries/zed/blob/main/crates/terminal/src/terminal.rs (kill order)
- Pinned local source: ~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/portable-pty-0.9.0/src/{lib.rs,unix.rs}
