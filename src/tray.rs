//! Background tray process (like Mem Reduct): a tiny Win32 message loop with
//! no window and no GPU context. It owns the auto-clean timer.

use crate::{icon, platform};
use devsweep_core::fsutil::human_bytes;
use devsweep_core::settings::{settings_path, Settings, State};
use devsweep_core::{clean, scan, timefmt};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

const TICK_MS: u32 = 15_000;

/// `--tray --dry-run`: run the real timer, scan and selection, but only log
/// the plan to history.log instead of deleting (for testing the schedule).
static DRY_RUN: AtomicBool = AtomicBool::new(false);

pub fn set_dry_run(on: bool) {
    DRY_RUN.store(on, Ordering::SeqCst);
}

#[derive(Default)]
struct Shared {
    running: AtomicBool,
    /// Set by the worker when a run finishes, read by the UI thread.
    finished: Mutex<Option<String>>,
}

pub fn run() {
    let Some(_instance) = platform::single_instance(platform::TRAY_MUTEX) else { return };
    platform::migrate_autostart();

    let mut settings = Settings::load();
    let mut settings_mtime = mtime();
    let started = timefmt::now_unix();

    let menu = Menu::new();
    let open = MenuItem::new("Open Prune", true, None);
    let open_settings = MenuItem::new("Settings", true, None);
    let auto = CheckMenuItem::new("Auto-clean", true, settings.auto_clean, None);
    let run_now = MenuItem::new("Run auto-clean now", true, None);
    let info = MenuItem::new(next_run_text(&settings, started), false, None);
    let exit = MenuItem::new("Exit", true, None);
    let _ = menu.append_items(&[
        &open,
        &open_settings,
        &PredefinedMenuItem::separator(),
        &auto,
        &run_now,
        &info,
        &PredefinedMenuItem::separator(),
        &exit,
    ]);

    let Ok(icon) = Icon::from_rgba(icon::rgba(32), 32, 32) else { return };
    let Ok(tray) = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_tooltip(tooltip(&settings, started))
        .with_icon(icon)
        .build()
    else {
        return;
    };

    let shared = Arc::new(Shared::default());
    trim_memory();

    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            DispatchMessageW, GetMessageW, PostQuitMessage, SetTimer, TranslateMessage, MSG, WM_TIMER,
        };
        SetTimer(std::ptr::null_mut(), 0, TICK_MS, None);
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);

            while let Ok(ev) = TrayIconEvent::receiver().try_recv() {
                match ev {
                    TrayIconEvent::DoubleClick { button: MouseButton::Left, .. } => platform::open_window(),
                    TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } => platform::open_window(),
                    _ => {}
                }
            }
            while let Ok(ev) = MenuEvent::receiver().try_recv() {
                if ev.id == open.id() {
                    platform::open_window();
                } else if ev.id == open_settings.id() {
                    platform::open_window_page("settings");
                } else if ev.id == auto.id() {
                    settings = Settings::load();
                    settings.auto_clean = auto.is_checked();
                    let _ = settings.save();
                    settings_mtime = mtime();
                    let _ = tray.set_tooltip(Some(tooltip(&settings, started)));
                } else if ev.id == run_now.id() {
                    start_run(&shared, settings.clone(), "manual");
                } else if ev.id == exit.id() {
                    PostQuitMessage(0);
                }
            }

            if msg.message == WM_TIMER {
                // Pick up changes made in the window.
                let m = mtime();
                if m != settings_mtime {
                    settings_mtime = m;
                    settings = Settings::load();
                    auto.set_checked(settings.auto_clean);
                }
                if settings.auto_clean && !shared.running.load(Ordering::SeqCst) {
                    let last = State::load().last_auto_run.unwrap_or(started);
                    if timefmt::now_unix() >= last + settings.interval_secs() {
                        start_run(&shared, settings.clone(), "scheduled");
                    }
                }
                if let Some(note) = shared.finished.lock().ok().and_then(|mut f| f.take()) {
                    let _ = tray.set_tooltip(Some(format!("{}\n{note}", tooltip(&settings, started))));
                    trim_memory();
                } else {
                    let _ = tray.set_tooltip(Some(tooltip(&settings, started)));
                }
                info.set_text(next_run_text(&settings, started));
            }
        }
    }
    drop(tray);
}

fn mtime() -> Option<SystemTime> {
    std::fs::metadata(settings_path()).and_then(|m| m.modified()).ok()
}

fn time_left(s: &Settings, st: &State, started: u64) -> String {
    let next = st.last_auto_run.unwrap_or(started) + s.interval_secs();
    let left = next.saturating_sub(timefmt::now_unix());
    if left < 3600 {
        format!("{} min", left / 60 + 1)
    } else if left < 86400 {
        format!("{} h", left / 3600)
    } else {
        format!("{} days", left / 86400)
    }
}

/// Greyed-out info line in the menu.
fn next_run_text(s: &Settings, started: u64) -> String {
    if !s.auto_clean {
        return "Auto-clean is off".into();
    }
    let unit = s.interval_unit.label();
    let every = if s.interval_value == 1 { unit.trim_end_matches('s').to_string() } else { format!("{} {unit}", s.interval_value) };
    format!("Next run in {} · every {every}, older than {} days", time_left(s, &State::load(), started), s.min_age_days)
}

fn tooltip(s: &Settings, started: u64) -> String {
    let st = State::load();
    let mut t = match st.last_reclaimable {
        Some(b) => format!("Prune · {} can be freed", human_bytes(b)),
        None => "Prune".to_string(),
    };
    if s.auto_clean {
        t.push_str(&format!("\nNext auto-clean in {}", time_left(s, &st, started)));
    } else {
        t.push_str("\nAuto-clean off");
    }
    t
}

/// Scan, select by rules + age, delete. Runs on a worker thread.
fn start_run(shared: &Arc<Shared>, settings: Settings, why: &'static str) {
    if shared.running.swap(true, Ordering::SeqCst) {
        return;
    }
    let shared = shared.clone();
    std::thread::spawn(move || {
        let mut ids: Vec<String> = settings.rules.iter().map(|r| r.provider.clone()).collect();
        ids.sort();
        ids.dedup();
        let mut state = State::load();
        let note = if ids.is_empty() {
            "No auto-clean rules set".to_string()
        } else {
            let reports = scan::scan_all(Some(&ids));
            let sel = clean::auto_selection(&reports, &settings);
            let plan = clean::plan(&sel);
            if DRY_RUN.load(Ordering::SeqCst) {
                devsweep_core::settings::log_history(&format!("auto-clean ({why}) DRY RUN\n{}", clean::describe(&plan)));
                format!("Dry run: would free {}", human_bytes(plan.bytes))
            } else if plan.is_empty() {
                devsweep_core::settings::log_history(&format!("auto-clean ({why}): nothing to clean"));
                state.last_freed = 0;
                "Last run: nothing to clean".to_string()
            } else {
                let out = clean::execute(&plan, |_, _| {});
                state.last_freed = out.freed_estimate;
                state.total_freed += out.freed_estimate;
                state.last_error = out.errors.first().cloned();
                format!("Last run freed {}", human_bytes(out.freed_estimate))
            }
        };
        state.last_auto_run = Some(timefmt::now_unix());
        let _ = state.save();
        if let Ok(mut f) = shared.finished.lock() {
            *f = Some(note);
        }
        shared.running.store(false, Ordering::SeqCst);
    });
}

use platform::trim_memory;
