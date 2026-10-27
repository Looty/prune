//! pnpm content-addressable store. v11 keeps package indexes as rows in
//! `index.db` (SQLite); older stores (v3, v10) keep one JSON index per package.
//! Files under `files/` are shared between packages and usually hard-linked
//! into projects. Read-only: the database is opened with SQLITE_OPEN_READ_ONLY.

use crate::fsutil;
use crate::model::{Kind, Package, Provider, ScanCtx, Target, Version};
use crate::providers::npm::{self, assign, list_files, other_files, Acc, FileInfo};
use crate::providers::util;
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub struct Pnpm;

impl Provider for Pnpm {
    fn id(&self) -> &'static str {
        "pnpm"
    }
    fn name(&self) -> &'static str {
        "pnpm"
    }
    fn cli_names(&self) -> &'static [&'static str] {
        &["pnpm"]
    }
    fn roots(&self) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = Vec::new();
        for c in candidate_store_dirs() {
            // A configured path may be the versioned dir itself.
            let c = if is_store_version(&c) { c.parent().map(Path::to_path_buf).unwrap_or(c) } else { c };
            if !out.contains(&c) && fsutil::subdirs(&c).iter().any(|d| is_store_version(d)) {
                out.push(c);
            }
        }
        out
    }
    fn scan(&self, ctx: &ScanCtx) -> Vec<Package> {
        ctx.roots.iter().flat_map(|r| scan_root(r, ctx)).collect()
    }
}

/// Configured store-dir values first, then known defaults.
fn candidate_store_dirs() -> Vec<PathBuf> {
    let mut v = Vec::new();
    for var in ["npm_config_store_dir", "PNPM_STORE_DIR", "pnpm_config_store_dir"] {
        if let Some(p) = fsutil::env_path(var) {
            v.push(p);
        }
    }
    let lad = fsutil::local_app_data();
    let rcs = [
        lad.join("pnpm").join("config").join("rc"),
        fsutil::app_data().join("pnpm").join("config").join("rc"),
        fsutil::home().join(".npmrc"),
    ];
    for rc in rcs {
        if let Some(s) = fs::read_to_string(&rc).ok().and_then(|t| npm::npmrc_value(&t, "store-dir")) {
            v.push(npm::expand_path(&s));
        }
    }
    v.push(lad.join("pnpm").join("store"));
    v.push(lad.join("pnpm-store"));
    v.push(fsutil::home().join(".pnpm-store"));
    if let Some(drive) = fsutil::home().components().next() {
        let mut p = PathBuf::from(drive.as_os_str());
        p.push("\\.pnpm-store");
        v.push(p);
    }
    v
}

fn is_version_dir_name(n: &str) -> bool {
    n.len() > 1 && n.starts_with('v') && n[1..].bytes().all(|b| b.is_ascii_digit())
}

/// `vN` directory that looks like a pnpm store (v11 `index.db`, or the
/// `files/` content dir that every store version has).
fn is_store_version(d: &Path) -> bool {
    is_version_dir_name(&util::file_name(d)) && (d.join("index.db").is_file() || d.join("files").is_dir())
}

// ---------------------------------------------------------------------------
// Parsing.

/// Split an index.db key "sha512-...\t@scope/name@1.2.3" into (name, version).
fn parse_row_key(key: &str) -> (String, String) {
    let spec = key.split_once('\t').map(|x| x.1).unwrap_or(key);
    let (n, v) = util::split_at_version(spec);
    if n.is_empty() {
        (spec.to_string(), String::new())
    } else {
        (n, v)
    }
}

fn is_hex(b: u8) -> bool {
    b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
}

/// Per-file sha512 digests inside a msgpackr record: runs of exactly 128
/// lowercase hex chars. A longer run is accepted when it is preceded by the
/// msgpack str8 length byte 0x80 (128), i.e. the next value is hex-looking.
fn extract_digests(data: &[u8]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < data.len() {
        if !is_hex(data[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < data.len() && is_hex(data[i]) {
            i += 1;
        }
        let run = i - start;
        if run == 128 || (run > 128 && start > 0 && data[start - 1] == 0x80) {
            let d = String::from_utf8_lossy(&data[start..start + 128]).into_owned();
            if !out.contains(&d) {
                out.push(d);
            }
        }
    }
    out
}

/// Every `integrity` string inside a legacy JSON index (files + sideEffects).
fn collect_integrities(v: &serde_json::Value, out: &mut Vec<String>) {
    match v {
        serde_json::Value::Object(m) => {
            for (k, x) in m {
                if k == "integrity" {
                    if let Some(s) = x.as_str() {
                        out.push(s.to_string());
                    }
                } else {
                    collect_integrities(x, out);
                }
            }
        }
        serde_json::Value::Array(a) => a.iter().for_each(|x| collect_integrities(x, out)),
        _ => {}
    }
}

/// Name/version from a v10 index file name: `<hash>-<name with + for />@<ver>.json`.
fn parse_index_file_name(file: &str) -> Option<(String, String)> {
    let stem = file.strip_suffix(".json")?;
    let (_, spec) = stem.split_once('-')?;
    let (n, v) = util::split_at_version(&spec.replace('+', "/"));
    if n.is_empty() || v.is_empty() {
        None
    } else {
        Some((n, v))
    }
}

/// Store path(s) for a hex digest: plain and executable variants that exist.
fn content_candidates(files_dir: &Path, hex: &str) -> [PathBuf; 2] {
    let base = files_dir.join(&hex[..2]);
    [base.join(&hex[2..]), base.join(format!("{}-exec", &hex[2..]))]
}

// ---------------------------------------------------------------------------
// Scan.

/// One package version in the store, before content is attributed.
struct Spec {
    name: String,
    version: String,
    /// Targets owned outright that carry no bytes of their own (index rows).
    head: Vec<Target>,
    /// Files owned outright (legacy JSON index files).
    own: Vec<FileInfo>,
    /// Hex digests of the content files it references.
    digests: Vec<String>,
}

fn scan_root(root: &Path, ctx: &ScanCtx) -> Vec<Package> {
    let mut out = Vec::new();
    let mut misc: Vec<PathBuf> = Vec::new();
    for e in fs::read_dir(root).into_iter().flatten().flatten() {
        let p = e.path();
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir && is_store_version(&p) {
            out.extend(scan_version_dir(&p, ctx));
        } else {
            misc.push(p);
        }
    }
    out.extend(npm::size_only("Other store files", "unrecognized files next to the store; not deleted", &misc));
    out
}

fn scan_version_dir(dir: &Path, ctx: &ScanCtx) -> Vec<Package> {
    let files_dir = dir.join("files");
    let db = dir.join("index.db");
    let v11 = db.is_file();
    let label = util::file_name(dir);

    let (specs, all_files) = rayon::join(
        || if v11 { read_db_specs(&db, ctx) } else { read_legacy_specs(dir, ctx) },
        || {
            list_files(&files_dir)
                .into_par_iter()
                .map(|f| {
                    let l = fsutil::link_count(&f.path);
                    (f, l)
                })
                .collect::<Vec<_>>()
        },
    );
    let mut out = Vec::new();

    let Some(specs) = specs else {
        // Index unreadable: report the bytes, but offer nothing for deletion.
        let size: u64 = all_files.iter().map(|(f, _)| f.len).sum();
        let mut p = Package::new(format!("Store files ({label}, index unreadable)"), Kind::Other)
            .with_note("could not read the store index; use `pnpm store prune`");
        p.versions.push(Version { version: "-".into(), size, reclaimable: 0, fetched: None, targets: Vec::new() });
        out.push(p);
        out.extend(version_dir_extras(dir));
        return out;
    };

    // Legacy JSON index files live inside files/ for v3: they are owned by
    // their spec, not content.
    let owned: HashSet<&Path> = specs.iter().flat_map(|s| s.own.iter().map(|f| f.path.as_path())).collect();
    let content: Vec<&(FileInfo, u32)> = all_files.iter().filter(|(f, _)| !owned.contains(f.path.as_path())).collect();
    let by_path: HashMap<&Path, usize> = content.iter().enumerate().map(|(i, (f, _))| (f.path.as_path(), i)).collect();

    let mut owners: Vec<Vec<usize>> = vec![Vec::new(); content.len()];
    let mut accs: Vec<Acc> = Vec::with_capacity(specs.len());
    for (si, s) in specs.iter().enumerate() {
        let mut acc = Acc { targets: s.head.clone(), ..Default::default() };
        for f in &s.own {
            acc.own(f.path.clone(), f.len, 1);
            acc.fetched = fsutil::max_time(acc.fetched, f.created);
        }
        for d in &s.digests {
            if d.len() < 3 {
                continue;
            }
            for c in content_candidates(&files_dir, d) {
                if let Some(&ci) = by_path.get(c.as_path()) {
                    // Specs are visited in order, so checking the last owner dedups.
                    if owners[ci].last() != Some(&si) {
                        owners[ci].push(si);
                    }
                }
            }
        }
        accs.push(acc);
    }
    let mut unreferenced: Vec<(FileInfo, u32)> = Vec::new();
    for (ci, (f, links)) in content.iter().enumerate() {
        let o = &owners[ci];
        if o.is_empty() {
            unreferenced.push((f.clone(), *links));
            continue;
        }
        assign(&f.path, f.len, *links, o, &mut accs);
        for &si in o {
            accs[si].fetched = fsutil::max_time(accs[si].fetched, f.created);
        }
    }

    let items: Vec<(String, Version)> = specs
        .into_iter()
        .zip(accs)
        .map(|(s, a)| {
            let v = if s.version.is_empty() { "-".to_string() } else { s.version };
            (s.name, a.into_version(v))
        })
        .collect();
    out.extend(util::group(items, Kind::Package));
    out.extend(other_files(
        &format!("Unreferenced store files ({label})"),
        "content no package index points to; freed by `pnpm store prune`",
        &unreferenced,
    ));
    out.extend(version_dir_extras(dir));
    out
}

/// Everything in a `vN` dir besides `files/` content and index files.
fn version_dir_extras(dir: &Path) -> Vec<Package> {
    let label = util::file_name(dir);
    let mut out = Vec::new();
    let mut idx: Vec<PathBuf> = Vec::new();
    let mut tmp: Vec<PathBuf> = Vec::new();
    let mut misc: Vec<PathBuf> = Vec::new();
    for e in fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().to_ascii_lowercase();
        match name.as_str() {
            "files" | "index" => {}
            // Symlinks to projects using the store: no bytes, never offered.
            "projects" => {}
            "tmp" => tmp.push(e.path()),
            n if n.starts_with("index.db") => idx.push(e.path()),
            _ => misc.push(e.path()),
        }
    }
    if !idx.is_empty() {
        // Deleting the database would orphan the whole store: size only, no targets.
        let size: u64 = idx.iter().map(|p| npm::tree_bytes(p)).sum();
        let fetched = idx.iter().filter_map(|p| fsutil::fetched_time_of(p)).max();
        let mut p = Package::new(format!("Store index ({label})"), Kind::Other)
            .with_note("pnpm's package index database; shrinks when packages are removed");
        p.versions.push(Version { version: "-".into(), size, reclaimable: 0, fetched, targets: Vec::new() });
        out.push(p);
    }
    out.extend(util::other(&format!("Temporary files ({label})"), "leftovers from interrupted installs; safe to delete", tmp, true));
    // Unknown store files: count them, but do not offer to delete.
    out.extend(npm::size_only(&format!("Other store files ({label})"), "unrecognized files in the store", &misc));
    out
}

/// Rows of a v11 `index.db`. None when the database cannot be read.
fn read_db_specs(db: &Path, ctx: &ScanCtx) -> Option<Vec<Spec>> {
    use rusqlite::{Connection, OpenFlags};
    let rows: Vec<(String, Vec<u8>)> = (|| -> rusqlite::Result<Vec<(String, Vec<u8>)>> {
        // Plain read-only open (not immutable=1) so rows still in the WAL are seen.
        let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        let mut st = conn.prepare("SELECT key, data FROM package_index")?;
        let it = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?)))?;
        it.collect()
    })()
    .map_err(|e| ctx.warn(format!("pnpm: cannot read {}: {e}", db.display())))
    .ok()?;
    Some(
        rows.into_par_iter()
            .map(|(key, data)| {
                let (name, version) = parse_row_key(&key);
                Spec {
                    name,
                    version,
                    head: vec![Target::PnpmIndexRow { db: db.to_path_buf(), key }],
                    own: Vec::new(),
                    digests: extract_digests(&data),
                }
            })
            .collect(),
    )
}

/// JSON index files of a v3 / v10 store.
fn read_legacy_specs(dir: &Path, ctx: &ScanCtx) -> Option<Vec<Spec>> {
    let mut index_files: Vec<FileInfo> =
        list_files(&dir.join("index")).into_iter().filter(|f| f.path.extension().is_some_and(|e| e == "json")).collect();
    index_files.extend(list_files(&dir.join("files")).into_iter().filter(|f| util::file_name(&f.path).ends_with("-index.json")));
    let bad = std::sync::atomic::AtomicUsize::new(0);
    let specs = index_files
        .into_par_iter()
        .filter_map(|f| {
            let Some(v) = fs::read(&f.path).ok().and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok()) else {
                bad.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return None;
            };
            let mut ints = Vec::new();
            collect_integrities(&v, &mut ints);
            let mut digests: Vec<String> = ints.iter().filter_map(|i| npm::integrity_hex(i)).map(|(_, h)| h).collect();
            digests.sort();
            digests.dedup();
            let from_json = v
                .get("name")
                .and_then(|n| n.as_str())
                .map(|n| (n.to_string(), v.get("version").and_then(|x| x.as_str()).unwrap_or("").to_string()));
            let (name, version) = from_json
                .or_else(|| parse_index_file_name(&util::file_name(&f.path)))
                .unwrap_or_else(|| ("(unnamed package)".into(), util::file_name(&f.path)));
            Some(Spec { name, version, head: Vec::new(), own: vec![f], digests })
        })
        .collect();
    let bad = bad.into_inner();
    if bad > 0 {
        ctx.warn(format!("pnpm: {bad} unreadable index files in {}", dir.display()));
    }
    Some(specs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_keys() {
        assert_eq!(parse_row_key("sha512-abc==\t@tailwindcss/node@4.3.3"), ("@tailwindcss/node".into(), "4.3.3".into()));
        assert_eq!(parse_row_key("sha512-x\tget-tsconfig@4.14.3"), ("get-tsconfig".into(), "4.14.3".into()));
        assert_eq!(parse_row_key("weird"), ("weird".into(), String::new()));
    }

    #[test]
    fn digests_from_msgpack() {
        let d1 = "a".repeat(64) + &"0123456789".repeat(6) + "beef";
        let d2 = "f".repeat(128);
        assert_eq!(d1.len(), 128);
        let mut data = vec![0xd9, 0x80];
        data.extend_from_slice(d1.as_bytes());
        data.extend_from_slice(&[0xa4, b'm', b'o', b'd', b'e', 0xcd, 0x01, 0xa4, 0xd9, 0x80]);
        data.extend_from_slice(d2.as_bytes());
        // A positive fixint that happens to be an ASCII hex digit right after.
        data.push(b'5');
        // Too short and too long (not preceded by 0x80) runs are ignored.
        data.extend_from_slice(b"\x01abcdef\x02");
        data.extend_from_slice("0".repeat(130).as_bytes());
        // Duplicates collapse.
        data.extend_from_slice(&[0xd9, 0x80]);
        data.extend_from_slice(d1.as_bytes());
        data.push(0xc0);
        assert_eq!(extract_digests(&data), vec![d1, d2]);
    }

    #[test]
    fn legacy_json() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"name":"a","version":"1.0.0","files":{"index.js":{"integrity":"sha512-AAH+/w==","mode":420,"size":4}},
               "sideEffects":{"k":{"added":{"x.node":{"integrity":"sha512-AAE=","mode":493}}}}}"#,
        )
        .unwrap();
        let mut ints = Vec::new();
        collect_integrities(&v, &mut ints);
        assert_eq!(ints, vec!["sha512-AAH+/w==".to_string(), "sha512-AAE=".to_string()]);
        assert_eq!(parse_index_file_name("0a1b2c-@types+node@20.1.0.json"), Some(("@types/node".into(), "20.1.0".into())));
        assert_eq!(parse_index_file_name("0a1b2c-index.json"), None);
    }

    /// A fake v11 store in a temp dir: shared files are split, never double counted.
    #[test]
    fn scan_fake_v11_store() {
        let dir = std::env::temp_dir().join(format!("devsweep-pnpm-test-{}", std::process::id())).join("v11");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let shared = "a".repeat(128);
        let own = "b".repeat(128);
        let exec = "c".repeat(128);
        let w = |hex: &str, suffix: &str, data: &[u8]| {
            let p = dir.join("files").join(&hex[..2]).join(format!("{}{suffix}", &hex[2..]));
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, data).unwrap();
        };
        w(&shared, "", b"shared-data");
        w(&own, "", b"own");
        w(&exec, "-exec", b"#!/bin/sh");
        w(&"d".repeat(128), "", b"unreferenced");
        let blob = |digests: &[&str]| {
            let mut b = vec![0x80u8 | 2];
            for d in digests {
                b.extend_from_slice(&[0xd9, 0x80]);
                b.extend_from_slice(d.as_bytes());
                b.extend_from_slice(&[0xcd, 0x01, 0xa4]);
            }
            b
        };
        {
            let c = rusqlite::Connection::open(dir.join("index.db")).unwrap();
            c.execute("CREATE TABLE package_index (key TEXT PRIMARY KEY, data BLOB NOT NULL) WITHOUT ROWID", []).unwrap();
            c.execute("INSERT INTO package_index VALUES (?1, ?2)", rusqlite::params!["sha512-x\tfoo@1.0.0", blob(&[&shared, &own])])
                .unwrap();
            c.execute("INSERT INTO package_index VALUES (?1, ?2)", rusqlite::params!["sha512-y\t@s/bar@2.0.0", blob(&[&shared, &exec])])
                .unwrap();
        }
        let total: u64 = list_files(&dir).iter().map(|f| f.len).sum();
        let ctx = ScanCtx { roots: &[], warnings: std::sync::Mutex::new(Vec::new()) };
        let pkgs = scan_version_dir(&dir, &ctx);
        let _ = fs::remove_dir_all(dir.parent().unwrap());

        let sum: u64 = pkgs.iter().map(|p| p.size()).sum();
        assert_eq!(sum, total);
        let foo = pkgs.iter().find(|p| p.name == "foo").unwrap();
        let bar = pkgs.iter().find(|p| p.name == "@s/bar").unwrap();
        assert_eq!(bar.versions[0].version, "2.0.0");
        let t = &foo.versions[0].targets;
        assert!(matches!(&t[0], Target::PnpmIndexRow { key, .. } if key == "sha512-x\tfoo@1.0.0"));
        assert!(t.iter().any(|x| matches!(x, Target::Shared { refs: 2, size: 11, .. })));
        assert_eq!(foo.versions[0].reclaimable, 3); // only its own file
        assert_eq!(bar.versions[0].reclaimable, 9); // the -exec file
        let unref = pkgs.iter().find(|p| p.name.starts_with("Unreferenced")).unwrap();
        assert_eq!(unref.size(), 12);
        let idx = pkgs.iter().find(|p| p.name.starts_with("Store index")).unwrap();
        assert!(idx.versions[0].targets.is_empty());
    }

    #[test]
    fn candidates() {
        let [a, b] = content_candidates(Path::new("files"), "abcdef");
        assert_eq!(a, Path::new("files").join("ab").join("cdef"));
        assert_eq!(b, Path::new("files").join("ab").join("cdef-exec"));
        assert!(is_version_dir_name("v11") && !is_version_dir_name("v") && !is_version_dir_name("vx"));
    }
}
