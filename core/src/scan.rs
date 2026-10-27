use crate::fsutil;
use crate::model::{Package, Provider, ProviderReport, ScanCtx};
use crate::providers;
use rayon::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Instant;

static VERIFY: AtomicBool = AtomicBool::new(false);

/// Debug mode: also walk every root independently to measure the real disk
/// size, so "unaccounted" bytes show up. Off by default (it doubles the I/O):
/// then `disk_size` is the sum of what providers attributed.
pub fn set_verify(on: bool) {
    VERIFY.store(on, Ordering::Relaxed);
}

/// Scan one provider. Returns None when the tool is neither on PATH nor has a cache.
pub fn scan_one(p: &dyn Provider) -> Option<ProviderReport> {
    let t = Instant::now();
    let cli = p.cli_names().iter().find_map(|n| fsutil::which(n));
    let roots = p.roots();
    if roots.is_empty() && cli.is_none() {
        return None;
    }
    let ctx = ScanCtx { roots: &roots, warnings: Mutex::new(Vec::new()) };
    let mut packages = if roots.is_empty() { Vec::new() } else { p.scan(&ctx) };
    packages.retain(|pk| pk.size() > 0 || !pk.versions.is_empty());
    sort_packages(&mut packages);
    let attributed: u64 = packages.iter().map(|p| p.size()).sum();
    let disk_size = if VERIFY.load(Ordering::Relaxed) {
        roots.par_iter().map(|r| fsutil::dir_stats_par(r, false).bytes).sum::<u64>()
    } else {
        attributed
    };
    Some(ProviderReport {
        id: p.id(),
        name: p.name(),
        cli,
        roots: roots.clone(),
        disk_size,
        packages,
        scan_ms: t.elapsed().as_millis(),
        warnings: ctx.warnings.into_inner().unwrap_or_default(),
    })
}

pub fn sort_packages(packages: &mut [Package]) {
    packages.sort_by(|a, b| b.size().cmp(&a.size()).then_with(|| a.name.cmp(&b.name)));
}

/// Scan every provider in parallel, one thread each (so a huge cache never
/// holds up small ones), with their inner work on the shared rayon pool.
/// `on_report` is called as each provider finishes, so a UI can fill in progressively.
pub fn scan_all_with(filter: Option<&[String]>, on_report: impl Fn(ProviderReport) + Sync) {
    scan_all_progress(filter, on_report, |_| {});
}

/// Like `scan_all_with`, plus `on_done(id)` for every provider that finishes,
/// including ones that aren't installed (they produce no report).
pub fn scan_all_progress(filter: Option<&[String]>, on_report: impl Fn(ProviderReport) + Sync, on_done: impl Fn(&'static str) + Sync) {
    let all = providers::all();
    let on_report = &on_report;
    let on_done = &on_done;
    // Each provider gets its own small pool: a huge cache (uv) can't starve
    // the others by flooding a shared queue. Pools end with the scan.
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let per = (threads / 2).max(2);
    std::thread::scope(|s| {
        for p in all.iter().filter(|p| filter.is_none_or(|f| f.iter().any(|id| id == p.id()))) {
            s.spawn(move || {
                let run = || scan_one(p.as_ref());
                let r = match rayon::ThreadPoolBuilder::new().num_threads(per).build() {
                    Ok(pool) => pool.install(run),
                    Err(_) => run(),
                };
                if let Some(r) = r {
                    on_report(r);
                }
                on_done(p.id());
            });
        }
    });
}

/// Blocking scan returning reports in a stable order.
pub fn scan_all(filter: Option<&[String]>) -> Vec<ProviderReport> {
    let out = Mutex::new(Vec::new());
    scan_all_with(filter, |r| out.lock().unwrap().push(r));
    let mut v = out.into_inner().unwrap();
    let order: Vec<&str> = providers::all().iter().map(|p| p.id()).collect();
    v.sort_by_key(|r| order.iter().position(|id| *id == r.id).unwrap_or(usize::MAX));
    v
}
