#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! Prune: find and clean global caches of dev package managers.
//!
//!   devsweep              open the window
//!   devsweep --tray       run the background tray (auto-clean timer)
//!   devsweep --tray --dry-run   same, but scheduled runs only log their plan
//!   devsweep --debug [ids...] [--top N] [-v]
//!                         print everything the scan finds plus the auto-clean
//!                         dry-run plan. Read-only; never deletes anything.
//!   devsweep --page settings|history   open the window on a page
//!   devsweep --screenshot out.png [--state settings|history|dark|light|select|confirm|<id>]
//!                         render the window after scanning, save a PNG, exit.

mod app;
mod debug;
mod icon;
mod platform;
mod png;
mod theme;
mod tray;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--debug") {
        platform::attach_console();
        debug::run(&args);
    } else if args.iter().any(|a| a == "--tray") {
        tray::set_dry_run(args.iter().any(|a| a == "--dry-run"));
        tray::run();
    } else {
        let after = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();
        app::run(after("--screenshot").map(|path| app::Shot { path, state: after("--state").unwrap_or_default() }), after("--page"));
    }
}
