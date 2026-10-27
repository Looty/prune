//! Turning a selection into concrete delete actions, and running them.
//! `plan` is pure. `execute` is the only code in the app that deletes anything.

use crate::model::{Package, ProviderReport, Target};
use crate::settings::Settings;
use crate::timefmt;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

#[derive(Debug, Clone)]
pub enum Action {
    Delete(PathBuf),
    PnpmRows { db: PathBuf, keys: Vec<String> },
    UvClean { cache_dir: PathBuf, packages: Vec<String> },
}

#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub actions: Vec<Action>,
    /// Estimated bytes freed.
    pub bytes: u64,
    /// (provider id, package name) included.
    pub packages: Vec<(String, String)>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }
}

/// Build the actions needed to remove the given packages (all versions).
pub fn plan(items: &[(&str, &Package)]) -> Plan {
    let mut out = Plan::default();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut deletes: Vec<PathBuf> = Vec::new();
    let mut shared: HashMap<PathBuf, (u64, u32, u32)> = HashMap::new(); // size, refs, selected
    let mut pnpm: BTreeMap<PathBuf, Vec<String>> = BTreeMap::new();
    let mut uv: BTreeMap<PathBuf, Vec<String>> = BTreeMap::new();

    for (provider, pkg) in items {
        out.packages.push((provider.to_string(), pkg.name.clone()));
        for v in &pkg.versions {
            out.bytes += v.reclaimable;
            for t in &v.targets {
                match t {
                    Target::Path(p) => {
                        if seen.insert(p.clone()) {
                            deletes.push(p.clone());
                        }
                    }
                    Target::Shared { path, size, refs } => {
                        let e = shared.entry(path.clone()).or_insert((*size, *refs, 0));
                        e.2 += 1;
                    }
                    Target::PnpmIndexRow { db, key } => pnpm.entry(db.clone()).or_default().push(key.clone()),
                    Target::UvClean { cache_dir, package } => {
                        let list = uv.entry(cache_dir.clone()).or_default();
                        if !list.contains(package) {
                            list.push(package.clone());
                        }
                    }
                }
            }
        }
    }

    // Index rows first so the store never points at missing files.
    for (db, keys) in pnpm {
        out.actions.push(Action::PnpmRows { db, keys });
    }
    for p in deletes {
        out.actions.push(Action::Delete(p));
    }
    let mut shared: Vec<_> = shared.into_iter().filter(|(_, (_, refs, sel))| sel >= refs).collect();
    shared.sort_by(|a, b| a.0.cmp(&b.0));
    for (path, (size, _, _)) in shared {
        if seen.insert(path.clone()) {
            out.bytes += size;
            out.actions.push(Action::Delete(path));
        }
    }
    // uv last, in small batches: it can be held up by a running `uv run`,
    // and batches keep the progress moving.
    for (cache_dir, packages) in uv {
        for chunk in packages.chunks(UV_BATCH) {
            out.actions.push(Action::UvClean { cache_dir: cache_dir.clone(), packages: chunk.to_vec() });
        }
    }
    out
}

const UV_BATCH: usize = 25;

/// Packages the auto-clean rules would remove right now.
pub fn auto_selection<'a>(reports: &'a [ProviderReport], s: &Settings) -> Vec<(&'a str, &'a Package)> {
    let cutoff = SystemTime::now() - Duration::from_secs(s.min_age_days as u64 * 86400);
    let mut out = Vec::new();
    for r in reports {
        for p in &r.packages {
            if !s.covers(r.id, &p.name) {
                continue;
            }
            // Never auto-delete something without a known fetch date.
            let Some(f) = p.fetched() else { continue };
            if s.min_age_days == 0 || f <= cutoff {
                out.push((r.id, p));
            }
        }
    }
    out
}

#[derive(Debug, Default, Clone)]
pub struct Outcome {
    pub done: usize,
    pub errors: Vec<String>,
    pub freed_estimate: u64,
    pub ms: u128,
}

/// Run a plan. `progress(done, total)` is called after each action.
pub fn execute(plan: &Plan, progress: impl Fn(usize, usize)) -> Outcome {
    let t = std::time::Instant::now();
    let mut o = Outcome { freed_estimate: plan.bytes, ..Default::default() };
    let total = plan.actions.len();
    // uv caches found busy: their remaining batches are skipped, not waited on.
    let mut uv_busy: HashSet<PathBuf> = HashSet::new();
    for (i, a) in plan.actions.iter().enumerate() {
        let r: Result<(), String> = match a {
            Action::Delete(p) => crate::fsutil::remove_force(p).map_err(|e| format!("{}: {e}", p.display())),
            Action::PnpmRows { db, keys } => delete_pnpm_rows(db, keys),
            Action::UvClean { cache_dir, .. } if uv_busy.contains(cache_dir) => {
                progress(i + 1, total);
                continue;
            }
            Action::UvClean { cache_dir, packages } => match uv_clean(cache_dir, packages) {
                Err(UvError::Busy) => {
                    uv_busy.insert(cache_dir.clone());
                    Err(UV_BUSY_MSG.to_string())
                }
                Err(UvError::Other(e)) => Err(e),
                Ok(()) => Ok(()),
            },
        };
        match r {
            Ok(()) => o.done += 1,
            Err(e) => o.errors.push(e),
        }
        progress(i + 1, total);
    }
    o.ms = t.elapsed().as_millis();
    crate::settings::log_history(&format!(
        "cleaned {} packages, ~{} freed, {} errors",
        plan.packages.len(),
        crate::fsutil::human_bytes(plan.bytes),
        o.errors.len()
    ));
    o
}

fn delete_pnpm_rows(db: &std::path::Path, keys: &[String]) -> Result<(), String> {
    let mut conn = rusqlite::Connection::open(db).map_err(|e| format!("pnpm index: {e}"))?;
    conn.busy_timeout(Duration::from_secs(10)).ok();
    let tx = conn.transaction().map_err(|e| format!("pnpm index: {e}"))?;
    for k in keys {
        tx.execute("DELETE FROM package_index WHERE key = ?1", [k]).map_err(|e| format!("pnpm index: {e}"))?;
    }
    tx.commit().map_err(|e| format!("pnpm index: {e}"))
}

pub const UV_BUSY_MSG: &str =
    "uv skipped: its cache is in use by a running uv process (for example a `uv run` server). Close it and clean uv again.";

enum UvError {
    /// Another uv process holds the cache lock; uv would wait for it forever.
    Busy,
    Other(String),
}

/// `uv cache clean <packages>`. uv waits for other uv processes to release the
/// cache; when it says so on stderr we stop it instead of hanging.
fn uv_clean(cache_dir: &std::path::Path, packages: &[String]) -> Result<(), UvError> {
    use std::io::Read;
    use std::process::Stdio;
    use std::sync::{Arc, Mutex};

    let uv = crate::fsutil::which("uv").ok_or(UvError::Other("uv not found on PATH".into()))?;
    let mut cmd = std::process::Command::new(uv);
    cmd.arg("cache")
        .arg("clean")
        .args(packages)
        .env("UV_CACHE_DIR", cache_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = cmd.spawn().map_err(|e| UvError::Other(format!("uv: {e}")))?;

    // Collect stderr on a thread so we can watch it while uv runs.
    let err = Arc::new(Mutex::new(String::new()));
    let reader = child.stderr.take().map(|mut s| {
        let err = err.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            while let Ok(n) = s.read(&mut buf) {
                if n == 0 {
                    break;
                }
                if let Ok(mut e) = err.lock() {
                    e.push_str(&String::from_utf8_lossy(&buf[..n]));
                }
            }
        })
    });

    let started = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {}
            Err(e) => return Err(UvError::Other(format!("uv: {e}"))),
        }
        let text = err.lock().map(|e| e.clone()).unwrap_or_default();
        let busy = is_uv_busy(&text);
        if busy || started.elapsed() > Duration::from_secs(600) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(if busy { UvError::Busy } else { UvError::Other("uv cache clean timed out after 10 min".into()) });
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    if let Some(r) = reader {
        let _ = r.join();
    }
    if status.success() {
        Ok(())
    } else {
        let text = err.lock().map(|e| e.trim().to_string()).unwrap_or_default();
        Err(UvError::Other(format!("uv cache clean: {text}")))
    }
}

/// uv's "Cache is currently in-use, waiting for other uv processes to finish".
fn is_uv_busy(stderr: &str) -> bool {
    let s = stderr.to_lowercase();
    s.contains("in-use") || s.contains("waiting for other uv processes")
}

/// Human-readable plan, for debug output. Never executes anything.
pub fn describe(plan: &Plan) -> String {
    let mut s = format!(
        "PLAN (dry run): {} packages, {} actions, ~{} would be freed\n",
        plan.packages.len(),
        plan.actions.len(),
        crate::fsutil::human_bytes(plan.bytes)
    );
    for (prov, name) in &plan.packages {
        s.push_str(&format!("  pkg  {prov}:{name}\n"));
    }
    for a in &plan.actions {
        match a {
            Action::Delete(p) => s.push_str(&format!("  rm   {}\n", p.display())),
            Action::PnpmRows { db, keys } => s.push_str(&format!("  sql  {} rows from {}\n", keys.len(), db.display())),
            Action::UvClean { cache_dir, packages } => {
                s.push_str(&format!("  uv   cache clean {} (UV_CACHE_DIR={})\n", packages.join(" "), cache_dir.display()))
            }
        }
    }
    s
}

pub fn now_unix() -> u64 {
    timefmt::now_unix()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Kind, Version};
    use crate::settings::Rule;

    fn ver(targets: Vec<Target>, recl: u64, age_days: u64) -> Version {
        Version {
            version: "1".into(),
            size: recl,
            reclaimable: recl,
            fetched: Some(SystemTime::now() - Duration::from_secs(age_days * 86400)),
            targets,
        }
    }

    fn pkg(name: &str, v: Version) -> Package {
        Package { name: name.into(), kind: Kind::Package, note: None, versions: vec![v] }
    }

    #[test]
    fn shared_needs_all_owners() {
        let shared = |refs| Target::Shared { path: "s".into(), size: 10, refs };
        let a = pkg("a", ver(vec![Target::Path("a".into()), shared(2)], 5, 0));
        let b = pkg("b", ver(vec![Target::Path("b".into()), shared(2)], 5, 0));
        let one = plan(&[("x", &a)]);
        assert_eq!(one.actions.len(), 1);
        assert_eq!(one.bytes, 5);
        let both = plan(&[("x", &a), ("x", &b)]);
        assert_eq!(both.actions.len(), 3);
        assert_eq!(both.bytes, 20);
    }

    #[test]
    fn index_rows_first_uv_last_and_dedupe() {
        let t = vec![
            Target::Path("f".into()),
            Target::PnpmIndexRow { db: "db".into(), key: "k".into() },
            Target::UvClean { cache_dir: "c".into(), package: "numpy".into() },
            Target::UvClean { cache_dir: "c".into(), package: "numpy".into() },
            Target::Path("f".into()),
        ];
        let p = pkg("a", ver(t, 1, 0));
        let pl = plan(&[("x", &p)]);
        assert!(matches!(pl.actions[0], Action::PnpmRows { .. }));
        assert!(matches!(pl.actions[1], Action::Delete(_)));
        match &pl.actions[2] {
            Action::UvClean { packages, .. } => assert_eq!(packages, &vec!["numpy".to_string()]),
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(pl.actions.len(), 3);
    }

    #[test]
    fn uv_is_batched() {
        let t: Vec<Target> = (0..60).map(|i| Target::UvClean { cache_dir: "c".into(), package: format!("p{i}") }).collect();
        let p = pkg("a", ver(t, 1, 0));
        let pl = plan(&[("x", &p)]);
        let sizes: Vec<usize> = pl
            .actions
            .iter()
            .map(|a| match a {
                Action::UvClean { packages, .. } => packages.len(),
                _ => 0,
            })
            .collect();
        assert_eq!(sizes, vec![25, 25, 10]);
    }

    #[test]
    fn detects_uv_waiting_for_lock() {
        assert!(is_uv_busy("Cache is currently in-use, waiting for other uv processes to finish (use `--force` to override)\n"));
        assert!(!is_uv_busy("Removed 12 files (3.4MiB)\n"));
    }

    #[test]
    fn auto_selection_respects_rules_and_age() {
        let old = pkg("old", ver(vec![Target::Path("o".into())], 1, 90));
        let new = pkg("new", ver(vec![Target::Path("n".into())], 1, 2));
        let mut undated = pkg("undated", ver(vec![], 1, 0));
        undated.versions[0].fetched = None;
        let r = ProviderReport {
            id: "npm",
            name: "npm",
            cli: None,
            roots: vec![],
            disk_size: 0,
            packages: vec![old, new, undated],
            scan_ms: 0,
            warnings: vec![],
        };
        let reports = vec![r];
        let mut s = Settings { min_age_days: 30, ..Default::default() };
        assert!(auto_selection(&reports, &s).is_empty());
        s.rules.push(Rule { provider: "npm".into(), package: None });
        let names: Vec<_> = auto_selection(&reports, &s).iter().map(|(_, p)| p.name.clone()).collect();
        assert_eq!(names, vec!["old"]);
        s.min_age_days = 0;
        assert_eq!(auto_selection(&reports, &s).len(), 2);
        s.rules = vec![Rule { provider: "npm".into(), package: Some("new".into()) }];
        let names: Vec<_> = auto_selection(&reports, &s).iter().map(|(_, p)| p.name.clone()).collect();
        assert_eq!(names, vec!["new"]);
    }
}
