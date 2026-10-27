//! Plain-text debug report of everything a scan found.

use crate::fsutil::human_bytes as hb;
use crate::model::{Kind, ProviderReport, Target};
use crate::timefmt;
use std::fmt::Write;

pub struct Opts {
    /// Packages shown per provider (0 = all).
    pub top: usize,
    /// Show versions and delete targets.
    pub verbose: bool,
}

pub fn render(reports: &[ProviderReport], o: &Opts) -> String {
    let mut s = String::new();
    let disk: u64 = reports.iter().map(|r| r.disk_size).sum();
    let recl: u64 = reports.iter().map(|r| r.reclaimable()).sum();
    let _ = writeln!(s, "Prune debug report  {}", timefmt::now_iso());
    let _ = writeln!(s, "{} providers, {} on disk, {} reclaimable\n", reports.len(), hb(disk), hb(recl));

    for r in reports {
        let pk = r.packages.iter().filter(|p| p.kind == Kind::Package).count();
        let other = r.packages.len() - pk;
        let attributed = r.package_size();
        let _ = writeln!(s, "=== {} [{}]  scan {} ms", r.name, r.id, r.scan_ms);
        let _ = writeln!(s, "    cli: {}", r.cli.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "(not on PATH)".into()));
        for root in &r.roots {
            let _ = writeln!(s, "    root: {}", root.display());
        }
        let _ = writeln!(
            s,
            "    disk {}  attributed {}  unaccounted {}  reclaimable {}  packages {}  other {}",
            hb(r.disk_size),
            hb(attributed),
            hb(r.disk_size.saturating_sub(attributed)),
            hb(r.reclaimable()),
            pk,
            other
        );
        if attributed > r.disk_size + r.disk_size / 20 + 1024 * 1024 {
            let _ = writeln!(s, "    WARN attributed exceeds disk size (double counting?)");
        }
        for w in &r.warnings {
            let _ = writeln!(s, "    WARN {w}");
        }
        let n = if o.top == 0 { r.packages.len() } else { o.top.min(r.packages.len()) };
        if n > 0 {
            let _ = writeln!(s, "    {:<44} {:>5} {:>10} {:>10} {:>11} {:>14}", "package", "vers", "size", "reclaim", "fetched", "age");
        }
        for p in r.packages.iter().take(n) {
            let tag = if p.kind == Kind::Other { "* " } else { "  " };
            let _ = writeln!(
                s,
                "  {tag}{:<44} {:>5} {:>10} {:>10} {:>11} {:>14}{}",
                trunc(&p.name, 44),
                p.versions.len(),
                hb(p.size()),
                hb(p.reclaimable()),
                timefmt::date(p.fetched()),
                timefmt::relative(p.fetched()),
                p.note.as_ref().map(|n| format!("  ({n})")).unwrap_or_default()
            );
            if o.verbose {
                for v in &p.versions {
                    let _ = writeln!(
                        s,
                        "        v {:<30} {:>10} {:>10} {}",
                        trunc(&v.version, 30),
                        hb(v.size),
                        hb(v.reclaimable),
                        timefmt::date(v.fetched)
                    );
                    for t in &v.targets {
                        let _ = writeln!(s, "            {}", target_str(t));
                    }
                }
            }
        }
        if r.packages.len() > n {
            let _ = writeln!(s, "    ... {} more", r.packages.len() - n);
        }
        let _ = writeln!(s);
    }
    s
}

fn target_str(t: &Target) -> String {
    match t {
        Target::Path(p) => format!("path   {}", p.display()),
        Target::Shared { path, size, refs } => format!("shared {} ({}, {} refs)", path.display(), hb(*size), refs),
        Target::PnpmIndexRow { key, .. } => format!("pnpm-row {}", key.replace('\t', " | ")),
        Target::UvClean { package, .. } => format!("uv-clean {package}"),
    }
}

fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let t: String = s.chars().take(n - 1).collect();
        format!("{t}…")
    }
}
