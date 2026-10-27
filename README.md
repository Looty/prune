# Prune

A small native Windows app (Rust + egui) that finds the global caches of your dev package managers, shows what's in them, and cleans what you pick, by hand or on a schedule.

Supported: **npm, pnpm, Yarn, Bun, pip, uv, Cargo, Go, NuGet**. Only managers that exist on the machine show up; one that's installed with an empty cache shows as a slim "cache is empty" row.

**Download:** grab `prune-<version>-windows-x64.zip` from [Releases](https://github.com/Looty/prune/releases/latest), unzip, and run `prune.exe`. No installer; Windows 10/11 x64.

Everything Prune removes is a download cache. Projects keep working, and the package manager downloads a package again the next time a project needs it. Prune never touches `node_modules`, virtual environments or installed tools (`~/.cargo/bin`, global npm installs).

## What it shows

- One group per package manager, with its cache folders, size on disk and how much is reclaimable.
- Every cached package: versions, size, **reclaimable** size, and when it was **last fetched**.
- "extra" rows for non-package data: logs, orphaned blobs, build caches, index metadata, temp files.

**Reclaimable** leaves out files that are hard-linked into projects (pnpm, uv and Bun do this) and content shared with packages you didn't select, because deleting those frees no space.

## Cleaning

- Tick packages, a whole manager in its group header, or **Select all** above the groups (with a search, it selects only the matches). Then **Clean up**. You confirm before anything is deleted.
- pnpm entries are removed from `index.db` together with their store files. uv packages are removed with `uv cache clean <pkg>`, which respects uv's locks. Everything else is deleted directly, and read-only Go module files are handled.

## Auto-clean (tray)

Like Mem Reduct, Prune keeps a tiny tray process (`prune --tray`, a few MB of RAM, no GPU) that runs the timer, even when the window is closed.

1. In the list, turn on the **Auto** switch for packages or whole managers.
2. In **Settings**, turn on auto-clean and choose **every N minutes/hours/days** and **only packages not fetched in the last N days**.
3. Optionally choose **Start with Windows**, which adds an HKCU Run entry for the tray.

To test a schedule without deleting anything, run `prune --tray --dry-run`: scheduled runs then only write their plan to `history.log`.

Each run scans only the managers that have rules, deletes the matching packages that are old enough, and appends a line to `history.log`.

Tray icon:
- **Left-click:** opens the window.
- **Right-click:** a menu with Open Prune, Settings, Auto-clean (on/off), Run auto-clean now, the next run time, and Exit.
- **Hover:** shows how much can be freed and when the next auto-clean runs.

`prune --page settings` opens the window on a page. If the window is already open, it switches to that page.

## Memory

The window's own data is about 17 MB; the rest of what Task Manager shows (~60–90 MB) is the graphics driver. When the window goes to the background or is minimized, Prune hands that memory back to Windows. Close the window and only the tray (~3 MB) stays.

## Debug mode

Prints everything the scan finds plus the auto-clean plan as a dry run. It never deletes anything.

```bash
prune --debug            # all managers, top 20 packages each
prune --debug npm uv -v  # only these, with versions and every path that would be deleted
prune --debug --top 0    # every package
```

The report is also saved to `%APPDATA%\Prune\debug-report.txt`. During development the same report is available without the GUI crate:

```bash
cargo run -p devsweep-core --example scan -- pnpm --top 30 -v --plan pnpm:react
```

`prune --screenshot out.png [--state settings|history|dark|light|select|confirm|cleaning|scanning|<manager id>]` renders the window, saves a PNG and exits. It's used to check the UI without clicking; `cleaning` only shows the progress dialog and deletes nothing.

## Files

`%APPDATA%\Prune\`: `settings.json`, `state.json` (last auto run, bytes freed), `history.log`, `debug-report.txt`. Set `PRUNE_HOME` to use another folder. A `%APPDATA%\DevSweep` folder from before the rename is moved here on first start, and an old "DevSweep" Start-with-Windows entry is replaced.

## Build

```bash
cargo build --release
```

The binary is `target\release\prune.exe`, with its icon and name embedded (`build.rs`, needs the Windows SDK's `rc.exe`; without it the build still works, just without the exe icon).

## Layout

- `core/` (crate `devsweep-core`): detection, scanning, cleanup planning and execution (no UI).
  - `providers/*.rs`: one file per package manager.
  - `clean.rs`: `plan` (pure) and `execute` (the only code that deletes).
- `src/`: window (`app.rs`), tray (`tray.rs`), theme, icon (`icon.rs`, drawn in code), Win32 helpers, debug output.
