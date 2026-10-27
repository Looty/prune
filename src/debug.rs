//! `--debug`: print scan data and the auto-clean dry-run plan. Read-only.

use prune_core::settings::{data_dir, settings_path, Settings, State};
use prune_core::{clean, report, scan, timefmt};
use std::fmt::Write;

pub fn run(args: &[String]) {
    let mut ids = Vec::new();
    let mut top = 20;
    let mut verbose = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--debug" => {}
            "--top" => top = it.next().and_then(|v| v.parse().ok()).unwrap_or(20),
            "-v" | "--verbose" => verbose = true,
            other => ids.push(other.to_string()),
        }
    }

    // Independent disk walk so "unaccounted" bytes are visible.
    scan::set_verify(true);
    let t = std::time::Instant::now();
    let reports = scan::scan_all(if ids.is_empty() { None } else { Some(&ids) });
    let mut out = report::render(&reports, &report::Opts { top, verbose });
    let _ = writeln!(out, "total scan wall time: {} ms\n", t.elapsed().as_millis());

    let s = Settings::load();
    let st = State::load();
    let _ = writeln!(out, "settings: {}", settings_path().display());
    let _ = writeln!(
        out,
        "auto-clean: {} every {} {}, older than {} days, {} rules",
        if s.auto_clean { "ON" } else { "off" },
        s.interval_value,
        s.interval_unit.label(),
        s.min_age_days,
        s.rules.len()
    );
    for r in &s.rules {
        let _ = writeln!(out, "  rule {}:{}", r.provider, r.package.as_deref().unwrap_or("*"));
    }
    let _ = writeln!(
        out,
        "last auto run: {}  last freed: {}  total freed: {}",
        st.last_auto_run.map(|u| timefmt::date(Some(timefmt::from_unix(u)))).unwrap_or_else(|| "never".into()),
        prune_core::fsutil::human_bytes(st.last_freed),
        prune_core::fsutil::human_bytes(st.total_freed)
    );
    let sel = clean::auto_selection(&reports, &s);
    out.push_str(&clean::describe(&clean::plan(&sel)));

    print!("{out}");
    let path = data_dir().join("debug-report.txt");
    let _ = std::fs::create_dir_all(data_dir());
    if std::fs::write(&path, &out).is_ok() {
        println!("\nreport saved to {}", path.display());
    }
}
