# CLAUDE.md

Guidance for AI assistants (and humans) working in this repository.

## What this project is

**Velo** is a lightweight, fast file manager for Windows, built with **Rust + [Slint](https://slint.dev/)**.
It is intentionally a *foundation* (土台), not a finished product: the priority is **fast startup,
fast directory listing, and low memory use** over visual polish. New features are meant to be layered
on top of a deliberately robust base.

Current scope:
- Lists the home directory on startup (📁 / 📄 type, name, size, modified time per row).
- Double-click a folder to enter it; "↑ 上へ" button to go to the parent.
- Current path shown in a read-only address bar.
- Directory reads happen on a background thread so the UI never blocks.
- External file add/remove/rename is watched and reflected after debouncing.

Deliberately *out of scope for now* (extension points): icons/thumbnails, context menus,
copy/move/delete, search, sort UI, tabs, multi-pane, bookmarks, settings.

## Language & conventions

> **Important:** This codebase is written in Japanese. All code comments, the `README.md`,
> UI strings (`ui/app.slint`), and **git commit messages** are in Japanese. Match this when
> editing — write new comments and commit messages in Japanese, keep UI text in Japanese, and
> preserve the existing explanatory "junior-developer notes" comment style.

- Maintain **zero clippy warnings** — `cargo clippy` is treated as a gate.
- Doc comments (`///`) explain *why*, not just *what*. Keep that altitude.

## Build, run, and check

Target platform is **Windows (x86_64-pc-windows-msvc)**.

```sh
cargo run              # dev run
cargo build --release  # release build (LTO, see [profile.release] in Cargo.toml)
cargo clippy           # static checks — keep warnings at zero
cargo fmt              # formatting
```

`build.rs` compiles `ui/app.slint` into Rust via `slint-build`; `main.rs` pulls the generated
code in with `slint::include_modules!()` (this generates `MainWindow` and `FileRow`).

Note: the remote/CI environment here is Linux, so a full `cargo run` of this Slint GUI app may not
be runnable headlessly. Prefer `cargo build` / `cargo clippy` / `cargo fmt --check` to validate
changes. There is no CI workflow and no `rust-toolchain`/`rustfmt`/`clippy` config file yet.

## Architecture — the load-bearing rules

The spine of the design: **the source of truth for state lives in Rust; Slint is display-only.**
Follow these four rules — they exist to prevent "which side owns this state?" bugs as features grow.

1. **Rust owns state.** "Which directory, showing what" lives in `AppState`
   (`src/app_state.rs`), held in a UI-thread `thread_local!` named `APP` in `src/main.rs`.
   Slint properties only render a formatted view of that state.

2. **No heavy I/O on the UI thread.** `read_dir` + per-file `metadata` is centralized in
   `fs::lister::list_dir` (`src/fs/lister.rs`) and **always** called from a worker thread.

3. **Worker → UI updates go only through `Weak::upgrade_in_event_loop`.** Worker threads
   return `Send` data only and never touch UI properties/models directly. Application happens in
   `main.rs`: `load_dir` (spawns worker) → `apply_listing` (runs on UI thread).

4. **The list is supplied via a virtualized `ListView` + `VecModel<FileRow>`.** The model is
   created once and connected to the `ListView`; updates swap *only the model contents* with
   `VecModel::set_vec` — never rebuild the whole property.

### Data flow

```text
user action (double-click / go up)        [UI thread]
        ▼
   navigate_to ──→ updates current_dir (last navigation wins)
        ▼
   load_dir ──→ std::thread::spawn ──────────────┐  [worker thread]
                                                  ▼
                       fs::lister::list_dir reads entries
                       to_rows formats display strings
                                                  │
            weak.upgrade_in_event_loop ←──────────┘  [back onto UI thread]
                                                  ▼
                       apply_listing: swap model + update path
                       (re-installs the watcher only on user navigation)
```

### "Last navigation wins" guard

Rapidly clicking folders can let a stale read arrive after a newer one. To prevent display drift,
`navigate_to` updates `current_dir` immediately, and `apply_listing` only applies a result when
`current_dir == the loaded path` (otherwise it discards the stale result).

### File watching

`fs::watcher::watch_dir` (`src/fs/watcher.rs`) watches the current directory **non-recursively**
using `notify` + `notify-debouncer-full`, with a **300ms debounce** (`DEBOUNCE` const). On a
confirmed change it triggers a re-read ("something changed" — individual events aren't inspected in
this foundation). The watcher callback runs on the debouncer's own thread, so it too hops to the UI
thread via `upgrade_in_event_loop`. The `DirWatcher` is kept alive in `AppState.watcher`; dropping
it stops watching. On user navigation the watcher is re-installed on the new dir; on watch-triggered
reloads it is left in place (the `install_watcher` flag in `load_dir`/`apply_listing` controls this).

## Module map

```text
src/
  main.rs        Entry point. Wires UI ↔ backend, owns the thread↔UI bridge and APP thread_local.
  app_state.rs   AppState: current path, current entries, active watcher (UI-thread only; not Send/Sync).
  fs/
    mod.rs       Re-exports; FS logic has no UI dependencies.
    entry.rs     FileEntry: domain model holding RAW data (u64 size, SystemTime) — no formatting.
    lister.rs    list_dir: worker-thread read_dir → sorted Vec<FileEntry>.
    watcher.rs   watch_dir: notify + debouncer; notifies via callback.
ui/
  app.slint      MainWindow + virtualized list; defines the FileRow struct.
build.rs         Compiles ui/app.slint with slint-build.
```

## Key design choices worth preserving

- **Domain vs. display split.** `FileEntry` keeps raw values; the display `FileRow` (Slint struct)
  holds pre-formatted strings. Formatting (`format_size`, `format_time` in `main.rs`) is done on the
  **worker thread** to keep UI-thread work minimal. This split also keeps future sort/filter (on raw
  values) and icon/thumbnail loading (async + LRU cache per path) cheap to add later.
- **Fixed sort for now:** folders first, then name-ascending case-insensitive
  (`sort_by_cached_key` in `lister.rs`). A sort UI is deferred.
- **Resilient listing:** a single failed `metadata`/entry is skipped rather than aborting the whole
  listing (handles permission-denied files gracefully).
- **Release profile** is tuned for small/fast binaries: `lto = true`, `codegen-units = 1`,
  `panic = "abort"`, `strip = true` (see `Cargo.toml`).

## Guiding principle

> Get a working foundation first. Optimize only after measuring.
