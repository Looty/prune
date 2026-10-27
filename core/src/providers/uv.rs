//! uv's cache: `%LOCALAPPDATA%\uv\cache` (or `UV_CACHE_DIR`, or `cache-dir`
//! in uv.toml).
//!
//! Layout: versioned buckets `<family>-v<N>`.
//! * `archive-v0/<id>/` unpacked wheels (`<name>-<ver>.dist-info/` names them)
//!   and cached virtual environments (`pyvenv.cfg`). Their files are hard-linked
//!   into venvs, so `reclaimable` uses link counts.
//! * `wheels-vN/{pypi,<kind>/<digest>}/<name>/<ver>-<tags>[.http|.msgpack|.lock]`
//!   pointer files naming `archive-v0/<id>` (the `.http` holds the bare id).
//! * `sdists-vN/{pypi,index/<digest>}/<name>/<ver>/...` built sdists, plus
//!   `{editable,url,path,git}/<digest>/...` for non-registry sources.
//! * `simple-vN/{pypi,index/<digest>}/<name>.rkyv` index responses.
//! * `environments-vN` (pointers to venv archives), `interpreter-vN`,
//!   `builds-vN`, `python-vN`, `git-vN`, `flat-index-vN`.
//!
//! Several uv releases can share one cache, each using its own bucket
//! versions. The uv on PATH (which `uv cache clean` runs) is probed by reading
//! its binary for bucket names; its buckets get `Target::UvClean`, buckets of
//! other live uv versions get plain path targets, and buckets no uv has
//! touched since a newer sibling appeared are "outdated".
//!
//! Read-only: files are only opened for reading, no process is spawned.

use super::pip::{canonical_name, expand_path, merge_packages, parse_dist_info, parse_wheel_filename};
use crate::fsutil;
use crate::model::{Kind, Package, Provider, ScanCtx, Target, Version};
use crate::providers::util;
use rayon::prelude::*;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub struct Uv;

impl Provider for Uv {
    fn id(&self) -> &'static str {
        "uv"
    }
    fn name(&self) -> &'static str {
        "uv"
    }
    fn cli_names(&self) -> &'static [&'static str] {
        &["uv"]
    }
    fn roots(&self) -> Vec<PathBuf> {
        let default = fsutil::local_app_data().join("uv").join("cache");
        let configured = fsutil::env_path("UV_CACHE_DIR").or_else(config_cache_dir);
        fsutil::existing(configured.into_iter().chain(Some(default)))
    }
    fn scan(&self, ctx: &ScanCtx) -> Vec<Package> {
        let exe = fsutil::which("uv");
        let probed = exe.as_deref().map(probe_exe).unwrap_or_default();
        let mut out = Vec::new();
        for root in ctx.roots {
            merge_packages(&mut out, scan_root(root, exe.as_deref(), &probed, ctx));
        }
        out
    }
}

// ---------------------------------------------------------------- config

fn config_cache_dir() -> Option<PathBuf> {
    let mut files: Vec<PathBuf> = Vec::new();
    files.extend(fsutil::env_path("UV_CONFIG_FILE"));
    files.push(fsutil::app_data().join("uv").join("uv.toml"));
    if let Some(pd) = fsutil::env_path("PROGRAMDATA") {
        files.push(pd.join("uv").join("uv.toml"));
    }
    files.iter().find_map(|f| {
        let text = fs::read_to_string(f).ok()?;
        toml_cache_dir(&text).map(|v| expand_path(&v))
    })
}

/// Top-level `cache-dir = "..."` from uv.toml text.
pub(crate) fn toml_cache_dir(text: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim().trim_start_matches('\u{feff}');
        if line.starts_with('[') {
            break; // only top-level keys
        }
        if line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        if k.trim().trim_matches(|c| c == '"' || c == '\'') == "cache-dir" {
            return toml_string(v);
        }
    }
    None
}

/// A single-line TOML basic ("...") or literal ('...') string.
fn toml_string(v: &str) -> Option<String> {
    let v = v.trim();
    if let Some(rest) = v.strip_prefix('\'') {
        return rest.find('\'').map(|i| rest[..i].to_string());
    }
    let rest = v.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                'u' => out.push(char::from_u32(u32::from_str_radix(&chars.by_ref().take(4).collect::<String>(), 16).ok()?)?),
                'U' => out.push(char::from_u32(u32::from_str_radix(&chars.by_ref().take(8).collect::<String>(), 16).ok()?)?),
                other => out.push(other), // \\ and \"
            },
            c => out.push(c),
        }
    }
    None
}

// ---------------------------------------------------------------- buckets

/// Families uv names its buckets after; longest first for suffix matching.
const FAMILIES: &[&str] =
    &["environments", "interpreter", "flat-index", "archive", "sdists", "simple", "wheels", "builds", "python", "git"];

/// "flat-index-v4" -> ("flat-index", 4).
pub(crate) fn parse_bucket(name: &str) -> Option<(String, u32)> {
    let (fam, n) = name.rsplit_once("-v")?;
    if fam.is_empty() || n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((fam.to_string(), n.parse().ok()?))
}

/// Highest bucket version per family named in a uv binary.
fn probe_exe(exe: &Path) -> HashMap<&'static str, u32> {
    fs::read(exe).map(|b| probe_bytes(&b)).unwrap_or_default()
}

pub(crate) fn probe_bytes(b: &[u8]) -> HashMap<&'static str, u32> {
    const CH: usize = 1 << 20;
    let chunks = b.len().div_ceil(CH);
    (0..chunks)
        .into_par_iter()
        .map(|c| {
            let mut m: HashMap<&'static str, u32> = HashMap::new();
            let end = ((c + 1) * CH).min(b.len());
            let mut i = c * CH;
            while i < end {
                if b[i] == b'-' && i + 2 < b.len() && b[i + 1] == b'v' && b[i + 2].is_ascii_digit() {
                    let mut j = i + 2;
                    let mut n: u32 = 0;
                    while j < b.len() && b[j].is_ascii_digit() && n < 10_000 {
                        n = n * 10 + (b[j] - b'0') as u32;
                        j += 1;
                    }
                    // Rust packs string literals back to back ("wheels-v6sdists-v9"),
                    // so the next byte may be anything; just bound the number.
                    if j - (i + 2) <= 3 && n < 1000 {
                        if let Some(f) = FAMILIES.iter().find(|f| i >= f.len() && &b[i - f.len()..i] == f.as_bytes()) {
                            let e = m.entry(f).or_insert(n);
                            *e = (*e).max(n);
                        }
                    }
                }
                i += 1;
            }
            m
        })
        .reduce(HashMap::new, |mut a, b| {
            for (k, v) in b {
                let e = a.entry(k).or_insert(v);
                *e = (*e).max(v);
            }
            a
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    /// Used by the uv on PATH: `uv cache clean` handles it.
    Current,
    /// Used by another (newer or older) uv sharing the cache.
    Live,
    /// Older version that nothing wrote to since a newer one appeared.
    Outdated,
}

struct Bucket {
    name: String,
    family: String,
    ver: u32,
    path: PathBuf,
    class: Class,
    /// Version the uv on PATH (or the newest present) uses for this family.
    current: u32,
}

/// Assign classes: `current` per family from the probe, else the newest present.
/// An older bucket is outdated only if a newer sibling was written more recently.
/// `newest`: newest file mtime per bucket index.
fn classify(buckets: &mut [Bucket], probed: &HashMap<&'static str, u32>, newest: &HashMap<usize, SystemTime>) {
    let mut fams: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, b) in buckets.iter().enumerate() {
        fams.entry(b.family.clone()).or_default().push(i);
    }
    for (fam, idx) in fams {
        let max = idx.iter().map(|&i| buckets[i].ver).max().unwrap_or(0);
        let cur = probed.get(fam.as_str()).copied().unwrap_or(max);
        for &i in &idx {
            let b = &buckets[i];
            let class = if b.ver == cur {
                Class::Current
            } else {
                let mine = newest.get(&i).copied();
                let newer_sibling = idx.iter().filter(|&&j| buckets[j].ver > b.ver).filter_map(|j| newest.get(j).copied()).max();
                match (mine, newer_sibling) {
                    (Some(m), Some(s)) if m < s => Class::Outdated,
                    (None, Some(_)) => Class::Outdated,
                    _ => Class::Live,
                }
            };
            buckets[i].class = class;
            buckets[i].current = cur;
        }
    }
}

// ---------------------------------------------------------------- units

#[derive(Debug, Clone, PartialEq, Eq)]
enum Owner {
    /// Package content. `direct`: delete by path even in the current bucket.
    Pkg { name: String, version: String, direct: bool },
    /// simple-vN index response for a package.
    Index { name: String },
    /// A whole non-package bucket (environments, interpreter...).
    Whole,
    /// Lock / marker files that uv keeps.
    Bookkeeping,
    /// Unrecognized file or directory inside a bucket.
    Loose,
}

struct Unit {
    bucket: usize,
    path: PathBuf,
    owner: Owner,
    size: u64,
    reclaimable: u64,
    fetched: Option<SystemTime>,
    newest: Option<SystemTime>,
    /// Indices into the archive list.
    refs: Vec<usize>,
}

fn unit(bucket: usize, path: PathBuf, owner: Owner) -> Unit {
    Unit { bucket, path, owner, size: 0, reclaimable: 0, fetched: None, newest: None, refs: Vec::new() }
}

fn is_bookkeeping(name: &str) -> bool {
    matches!(name, ".lock" | ".git" | ".gitignore" | "CACHEDIR.TAG")
}

fn entries(dir: &Path) -> Vec<(PathBuf, bool)> {
    let Ok(rd) = fs::read_dir(dir) else { return Vec::new() };
    let mut v: Vec<(PathBuf, bool)> = rd
        .flatten()
        .filter_map(|e| {
            let ft = e.file_type().ok()?;
            (!ft.is_symlink()).then(|| (e.path(), ft.is_dir()))
        })
        .collect();
    v.sort();
    v
}

/// Version from a wheels-bucket file name: `2.5.4-cp312-cp312-win_amd64.http`.
pub(crate) fn wheel_entry_version(file: &str) -> String {
    let mut stem = file;
    for ext in [".lock", ".http", ".msgpack", ".rev", ".whl"] {
        if let Some(s) = stem.strip_suffix(ext) {
            stem = s;
            break;
        }
    }
    if stem.starts_with(|c: char| c.is_ascii_digit()) {
        return stem.split('-').next().unwrap_or(stem).to_string();
    }
    parse_wheel_filename(stem).map(|(_, v)| v).unwrap_or_else(|| "?".into())
}

/// Name dirs of a wheels/sdists/simple bucket: `pypi/<x>` and `<kind>/<digest>/<x>`.
fn registry_children(b: usize, dir: &Path, out: &mut Vec<Unit>, f: &mut dyn FnMut(&Path, bool, &mut Vec<Unit>)) {
    for (kind, is_dir) in entries(dir) {
        let kname = util::file_name(&kind);
        if !is_dir {
            let o = if is_bookkeeping(&kname) { Owner::Bookkeeping } else { Owner::Loose };
            out.push(unit(b, kind, o));
        } else if kname == "pypi" {
            for (x, d) in entries(&kind) {
                f(&x, d, out);
            }
        } else {
            for (digest, d) in entries(&kind) {
                if !d {
                    out.push(unit(b, digest, Owner::Loose));
                    continue;
                }
                for (x, d) in entries(&digest) {
                    f(&x, d, out);
                }
            }
        }
    }
}

fn walk_wheels(b: usize, dir: &Path, out: &mut Vec<Unit>) {
    registry_children(b, dir, out, &mut |x, is_dir, out| {
        if !is_dir {
            out.push(unit(b, x.to_path_buf(), Owner::Loose));
            return;
        }
        let name = canonical_name(&util::file_name(x));
        for (f, _) in entries(x) {
            let version = wheel_entry_version(&util::file_name(&f));
            out.push(unit(b, f, Owner::Pkg { name: name.clone(), version, direct: false }));
        }
    });
}

fn walk_simple(b: usize, dir: &Path, out: &mut Vec<Unit>) {
    registry_children(b, dir, out, &mut |x, is_dir, out| {
        let fname = util::file_name(x);
        let stem = fname.rsplit_once('.').map(|(s, _)| s).unwrap_or(&fname);
        if is_dir || stem.is_empty() {
            out.push(unit(b, x.to_path_buf(), Owner::Loose));
        } else {
            out.push(unit(b, x.to_path_buf(), Owner::Index { name: canonical_name(stem) }));
        }
    });
}

/// Find a built wheel (file or pointer named like one) under `dir`.
fn find_wheel_identity(dir: &Path, depth: u32) -> Option<(String, String)> {
    let list = entries(dir);
    for (p, is_dir) in &list {
        if !is_dir {
            let n = util::file_name(p);
            if n.ends_with(".lock") {
                continue;
            }
            if let Some(id) = parse_wheel_filename(&n) {
                return Some(id);
            }
        } else if let Some(id) = parse_dist_info(&util::file_name(p)) {
            return Some(id);
        }
    }
    if depth == 0 {
        return None;
    }
    list.iter().filter(|(_, d)| *d).find_map(|(p, _)| find_wheel_identity(p, depth - 1))
}

fn walk_sdists(b: usize, dir: &Path, out: &mut Vec<Unit>) {
    for (kind, is_dir) in entries(dir) {
        let kname = util::file_name(&kind);
        if !is_dir {
            let o = if is_bookkeeping(&kname) { Owner::Bookkeeping } else { Owner::Loose };
            out.push(unit(b, kind, o));
            continue;
        }
        let registry = kname == "pypi" || kname == "index";
        let name_dirs: Vec<(PathBuf, bool)> = if kname == "pypi" {
            entries(&kind)
        } else if registry {
            entries(&kind).into_iter().flat_map(|(d, is_d)| if is_d { entries(&d) } else { vec![(d, false)] }).collect()
        } else {
            Vec::new()
        };
        if registry {
            // <name>/<version>/...
            for (nd, is_d) in name_dirs {
                if !is_d {
                    out.push(unit(b, nd, Owner::Loose));
                    continue;
                }
                let name = canonical_name(&util::file_name(&nd));
                let list = entries(&nd);
                let first_ver = list.iter().find(|(_, d)| *d).map(|(p, _)| util::file_name(p));
                for (v, is_v) in list {
                    let version = if is_v { util::file_name(&v) } else { first_ver.clone().unwrap_or_else(|| "-".into()) };
                    out.push(unit(b, v, Owner::Pkg { name: name.clone(), version, direct: false }));
                }
            }
            continue;
        }
        // editable / url / path / git: <digest>/<revision>/...; identify by the built wheel.
        for (digest, is_d) in entries(&kind) {
            if !is_d {
                out.push(unit(b, digest, Owner::Loose));
                continue;
            }
            let list = entries(&digest);
            let ids: Vec<Option<(String, String)>> = list.iter().map(|(p, d)| if *d { find_wheel_identity(p, 3) } else { None }).collect();
            let first = ids.iter().flatten().next().cloned();
            for ((p, _), id) in list.into_iter().zip(ids) {
                match id.or_else(|| first.clone()) {
                    Some((name, version)) => out.push(unit(b, p, Owner::Pkg { name, version, direct: true })),
                    None => out.push(unit(b, p, Owner::Loose)),
                }
            }
        }
    }
}

// ---------------------------------------------------------------- archives

#[derive(Debug, Clone, PartialEq, Eq)]
enum ArchiveKind {
    Wheel(String, String),
    Venv,
    Unknown,
}

struct Archive {
    path: PathBuf,
    kind: ArchiveKind,
    size: u64,
    reclaimable: u64,
    fetched: Option<SystemTime>,
}

fn archive_kind(dir: &Path) -> ArchiveKind {
    let list = entries(dir);
    if list.iter().any(|(p, d)| !d && util::file_name(p) == "pyvenv.cfg") {
        return ArchiveKind::Venv;
    }
    list.iter()
        .filter(|(_, d)| *d)
        .find_map(|(p, _)| parse_dist_info(&util::file_name(p)))
        .map(|(n, v)| ArchiveKind::Wheel(n, v))
        .unwrap_or(ArchiveKind::Unknown)
}

/// Archive ids by byte string, plus the distinct id lengths.
pub(crate) struct IdSet {
    map: HashMap<Vec<u8>, usize>,
    lens: Vec<usize>,
}

impl IdSet {
    pub(crate) fn new<'a>(ids: impl IntoIterator<Item = &'a str>) -> Self {
        let map: HashMap<Vec<u8>, usize> = ids.into_iter().enumerate().map(|(i, s)| (s.as_bytes().to_vec(), i)).collect();
        let mut lens: Vec<usize> = map.keys().map(Vec::len).collect();
        lens.sort_unstable();
        lens.dedup();
        IdSet { map, lens }
    }

    /// Archive ids mentioned in `data`: `archive-v0/<id>` text or a bare id in
    /// msgpack. Checks every run of id characters and each id-sized window.
    pub(crate) fn find(&self, data: &[u8], out: &mut Vec<usize>) {
        let is_id = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'-';
        for run in data.split(|&c| !is_id(c)) {
            for &l in &self.lens {
                if run.len() < l {
                    continue;
                }
                for w in run.windows(l) {
                    if let Some(&i) = self.map.get(w) {
                        if !out.contains(&i) {
                            out.push(i);
                        }
                    }
                }
            }
        }
    }
}

const SMALL: u64 = 64 * 1024;

// ---------------------------------------------------------------- link counts

/// Like `fsutil::dir_stats(path, true)` (parallel over sub-directories), but
/// reads hard-link counts by name where Windows supports it: opening each of
/// the ~10^5 archive files with `CreateFileW` costs milliseconds per file.
fn stats_links(path: &Path) -> fsutil::Stats {
    let Ok(rd) = fs::read_dir(path) else {
        let mut s = fsutil::Stats::default();
        if let Ok(m) = fs::symlink_metadata(path) {
            if m.is_file() {
                s.bytes = m.len();
                s.files = 1;
                s.reclaimable = if links::count(path) <= 1 { m.len() } else { 0 };
                s.newest_mtime = m.modified().ok();
            }
        }
        return s;
    };
    let list: Vec<_> = rd.flatten().collect();
    list.par_iter()
        .map(|e| {
            let mut s = fsutil::Stats::default();
            let Ok(ft) = e.file_type() else { return s };
            if ft.is_symlink() {
                return s;
            }
            if ft.is_dir() {
                return stats_links(&e.path());
            }
            if let Ok(m) = e.metadata() {
                let p = e.path();
                s.bytes = m.len();
                s.files = 1;
                if links::count(&p) <= 1 {
                    s.reclaimable = m.len();
                }
                s.newest_mtime = m.modified().ok();
            }
            s
        })
        .reduce(fsutil::Stats::default, |mut a, b| {
            a.bytes += b.bytes;
            a.files += b.files;
            a.reclaimable += b.reclaimable;
            a.newest_mtime = fsutil::max_time(a.newest_mtime, b.newest_mtime);
            a
        })
}

#[cfg(windows)]
mod links {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::sync::OnceLock;

    type ByName = unsafe extern "system" fn(*const u16, i32, *mut c_void, u32) -> i32;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetModuleHandleW(name: *const u16) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
    }

    /// `GetFileInformationByName` (Windows 11 24H2+), looked up at run time so
    /// older Windows still loads the binary.
    fn by_name() -> Option<ByName> {
        static F: OnceLock<Option<ByName>> = OnceLock::new();
        *F.get_or_init(|| unsafe {
            let k32: Vec<u16> = "kernel32.dll".encode_utf16().chain(Some(0)).collect();
            let m = GetModuleHandleW(k32.as_ptr());
            if m.is_null() {
                return None;
            }
            let f = GetProcAddress(m, c"GetFileInformationByName".as_ptr() as *const u8);
            (!f.is_null()).then(|| std::mem::transmute::<*mut c_void, ByName>(f))
        })
    }

    pub fn count(path: &Path) -> u32 {
        const FILE_STAT_BASIC_BY_NAME_INFO: i32 = 3;
        if let Some(f) = by_name() {
            let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            // FILE_STAT_BASIC_INFORMATION is 104 bytes; NumberOfLinks is the
            // u32 at offset 64 (after seven 8-byte fields, attributes, tag).
            let mut buf = [0u64; 13];
            let ok = unsafe { f(wide.as_ptr(), FILE_STAT_BASIC_BY_NAME_INFO, buf.as_mut_ptr().cast(), 104) };
            if ok != 0 {
                return ((buf[8] & 0xffff_ffff) as u32).max(1);
            }
        }
        crate::fsutil::link_count(path)
    }
}

#[cfg(not(windows))]
mod links {
    pub fn count(path: &std::path::Path) -> u32 {
        crate::fsutil::link_count(path)
    }
}

/// Archive references held by small files under `path`.
fn refs_under(path: &Path, ids: &IdSet, out: &mut Vec<usize>) {
    let Ok(meta) = fs::symlink_metadata(path) else { return };
    if meta.is_dir() {
        for (p, _) in entries(path) {
            refs_under(&p, ids, out);
        }
    } else if meta.is_file() && meta.len() <= SMALL && !util::file_name(path).ends_with(".lock") {
        if let Ok(data) = fs::read(path) {
            ids.find(&data, out);
        }
    }
}

// ---------------------------------------------------------------- assembly

#[derive(Default)]
struct Acc {
    size: u64,
    reclaimable: u64,
    fetched: Option<SystemTime>,
    paths: Vec<PathBuf>,
    uv_clean: bool,
}

impl Acc {
    fn add(&mut self, size: u64, recl: u64, fetched: Option<SystemTime>, path: Option<&Path>) {
        self.size += size;
        self.reclaimable += recl;
        self.fetched = fsutil::max_time(self.fetched, fetched);
        if let Some(p) = path {
            self.paths.push(p.to_path_buf());
        }
    }
    fn version(self, label: String, uv: Option<(&Path, &str)>) -> Version {
        let mut targets = Vec::new();
        if self.uv_clean {
            if let Some((cache_dir, package)) = uv {
                targets.push(Target::UvClean { cache_dir: cache_dir.to_path_buf(), package: package.to_string() });
            }
        }
        targets.extend(self.paths.into_iter().map(Target::Path));
        Version { version: label, size: self.size, reclaimable: self.reclaimable, fetched: self.fetched, targets }
    }
}

const UNREFERENCED: &str = "Unreferenced archives";
const OUTDATED: &str = "Outdated cache versions";
const INDEX_ONLY: &str = "Index cache (nothing else cached)";
const UNATTRIBUTED: &str = "Unattributed cache entries";
const BOOKKEEPING: &str = "Cache bookkeeping";
const UNKNOWN_ARCHIVES: &str = "Unidentified archives";

fn family_other(family: &str) -> (String, &'static str) {
    match family {
        "environments" => {
            ("Cached environments".into(), "uvx / `uv run --with` environments; files are hard links into archives, so little is freed")
        }
        "interpreter" => ("Interpreter metadata".into(), "cached probes of installed Pythons; rebuilt on demand"),
        "builds" => ("Build environments".into(), "isolated environments for building sdists; safe to delete"),
        "python" => ("Python downloads".into(), "downloaded Python distribution archives"),
        "git" => ("Git checkouts".into(), "git dependencies fetched by uv"),
        "flat-index" => ("Flat index cache".into(), "--find-links listings; re-fetched on demand"),
        "simple" | "wheels" | "sdists" | "archive" => (format!("{family} cache"), "uv cache data"),
        other => (format!("{other} cache"), "unrecognized uv cache bucket"),
    }
}

fn bucket_label(b: &Bucket) -> String {
    match b.class {
        Class::Live if b.ver > b.current => format!("{} (newer uv)", b.name),
        Class::Live => format!("{} (older uv)", b.name),
        _ => b.name.clone(),
    }
}

fn scan_root(root: &Path, exe: Option<&Path>, probed: &HashMap<&'static str, u32>, ctx: &ScanCtx) -> Vec<Package> {
    // Top level: buckets, bookkeeping files, unknown entries.
    let mut buckets: Vec<Bucket> = Vec::new();
    let mut root_units: Vec<(PathBuf, Owner)> = Vec::new();
    for (p, is_dir) in entries(root) {
        let name = util::file_name(&p);
        match (is_dir, parse_bucket(&name)) {
            (true, Some((family, ver))) => buckets.push(Bucket { name, family, ver, path: p, class: Class::Current, current: ver }),
            _ if is_bookkeeping(&name) => root_units.push((p, Owner::Bookkeeping)),
            _ => root_units.push((p, Owner::Loose)),
        }
    }

    // Archive ids (fast listing) so pointer scanning can start right away.
    let mut archive_paths: Vec<PathBuf> = Vec::new();
    let mut units: Vec<Unit> = Vec::new();
    for (bi, b) in buckets.iter().enumerate().filter(|(_, b)| b.family == "archive") {
        for (p, is_dir) in entries(&b.path) {
            if is_dir {
                archive_paths.push(p);
            } else {
                let o = if is_bookkeeping(&util::file_name(&p)) { Owner::Bookkeeping } else { Owner::Loose };
                let s = stats_links(&p);
                let mut u = unit(bi, p, o);
                (u.size, u.reclaimable, u.newest) = (s.bytes, s.reclaimable, s.newest_mtime);
                u.fetched = fsutil::fetched_time_of(&u.path);
                units.push(u);
            }
        }
    }
    let names: Vec<String> = archive_paths.iter().map(|p| util::file_name(p)).collect();
    let ids = IdSet::new(names.iter().map(String::as_str));

    let (archives, mut walked) = rayon::join(
        || {
            archive_paths
                .par_iter()
                .map(|p| {
                    let s = stats_links(p);
                    Archive {
                        path: p.clone(),
                        kind: archive_kind(p),
                        size: s.bytes,
                        reclaimable: s.reclaimable,
                        fetched: fsutil::fetched_time_of(p),
                    }
                })
                .collect::<Vec<_>>()
        },
        || {
            let mut units: Vec<Unit> = buckets
                .par_iter()
                .enumerate()
                .filter(|(_, b)| b.family != "archive")
                .flat_map(|(bi, b)| {
                    let mut out = Vec::new();
                    match b.family.as_str() {
                        "wheels" => walk_wheels(bi, &b.path, &mut out),
                        "sdists" => walk_sdists(bi, &b.path, &mut out),
                        "simple" => walk_simple(bi, &b.path, &mut out),
                        _ => out.push(unit(bi, b.path.clone(), Owner::Whole)),
                    }
                    out
                })
                .collect();
            units.par_iter_mut().for_each(|u| {
                let s = stats_links(&u.path);
                u.size = s.bytes;
                u.reclaimable = s.reclaimable;
                u.newest = s.newest_mtime;
                u.fetched = fsutil::fetched_time_of(&u.path);
                if matches!(buckets[u.bucket].family.as_str(), "wheels" | "sdists" | "environments") {
                    refs_under(&u.path, &ids, &mut u.refs);
                }
            });
            units
        },
    );
    units.append(&mut walked);
    let mut newest: HashMap<usize, SystemTime> = HashMap::new();
    for u in &units {
        if let Some(t) = u.newest {
            let e = newest.entry(u.bucket).or_insert(t);
            *e = (*e).max(t);
        }
    }
    classify(&mut buckets, probed, &newest);
    for (p, o) in root_units {
        let s = stats_links(&p);
        // Root-level units belong to no bucket; use a sentinel index.
        units.push(Unit {
            bucket: usize::MAX,
            fetched: fsutil::fetched_time_of(&p),
            size: s.bytes,
            reclaimable: s.reclaimable,
            newest: s.newest_mtime,
            path: p,
            owner: o,
            refs: Vec::new(),
        });
    }

    let can_clean = exe.is_some();
    let class_of = |bi: usize| if bi == usize::MAX { Class::Current } else { buckets[bi].class };
    let cleanable = |u: &Unit| {
        can_clean
            && class_of(u.bucket) == Class::Current
            && matches!(&u.owner, Owner::Pkg { direct: false, .. })
            && matches!(buckets[u.bucket].family.as_str(), "wheels" | "sdists")
    };

    // Who holds each archive.
    let mut holders: Vec<Vec<usize>> = vec![Vec::new(); archives.len()];
    for (ui, u) in units.iter().enumerate() {
        for &a in &u.refs {
            holders[a].push(ui);
        }
    }

    let mut pkgs: BTreeMap<(String, String), Acc> = BTreeMap::new();
    let mut index: BTreeMap<String, Acc> = BTreeMap::new();
    let mut index_cur: HashMap<String, Vec<PathBuf>> = HashMap::new();
    let mut whole: BTreeMap<usize, Acc> = BTreeMap::new(); // non-package buckets
    let mut outdated: BTreeMap<String, Acc> = BTreeMap::new();
    let mut unref: Vec<(String, Acc)> = Vec::new();
    let mut unknown_arch = Acc::default();
    let mut unattributed = Acc::default();
    let mut bookkeeping = Acc::default();

    for (ai, a) in archives.iter().enumerate() {
        let live: Vec<&Unit> = holders[ai].iter().map(|&u| &units[u]).filter(|u| class_of(u.bucket) != Class::Outdated).collect();
        if live.is_empty() {
            let label = match &a.kind {
                ArchiveKind::Wheel(n, v) => format!("{n} {v}"),
                ArchiveKind::Venv => format!("environment {}", util::file_name(&a.path)),
                ArchiveKind::Unknown => util::file_name(&a.path),
            };
            if holders[ai].is_empty() {
                let mut acc = Acc::default();
                acc.add(a.size, a.reclaimable, a.fetched, Some(&a.path));
                unref.push((label, acc));
            } else {
                outdated.entry("archives used only by outdated buckets".into()).or_default().add(
                    a.size,
                    a.reclaimable,
                    a.fetched,
                    Some(&a.path),
                );
            }
            continue;
        }
        if let Some(env) = live.iter().find(|u| buckets[u.bucket].family == "environments") {
            whole.entry(env.bucket).or_default().add(a.size, a.reclaimable, a.fetched, Some(&a.path));
            continue;
        }
        let owner = live.iter().find_map(|u| match &u.owner {
            Owner::Pkg { name, version, .. } => Some((name.clone(), version.clone())),
            _ => None,
        });
        let id = match (&a.kind, owner) {
            (ArchiveKind::Wheel(n, v), _) => Some((n.clone(), v.clone())),
            (_, o) => o,
        };
        match id {
            Some(key) => {
                let acc = pkgs.entry(key).or_default();
                if live.iter().any(|u| cleanable(u)) {
                    acc.uv_clean = true;
                    acc.add(a.size, a.reclaimable, a.fetched, None);
                } else {
                    acc.add(a.size, a.reclaimable, a.fetched, Some(&a.path));
                }
            }
            None => unknown_arch.add(a.size, a.reclaimable, a.fetched, Some(&a.path)),
        }
    }

    for u in &units {
        let class = class_of(u.bucket);
        if class == Class::Outdated {
            continue; // covered by the whole bucket below
        }
        match &u.owner {
            Owner::Pkg { name, version, .. } => {
                let acc = pkgs.entry((name.clone(), version.clone())).or_default();
                if cleanable(u) {
                    acc.uv_clean = true;
                    acc.add(u.size, u.reclaimable, u.fetched, None);
                } else {
                    acc.add(u.size, u.reclaimable, u.fetched, Some(&u.path));
                }
            }
            Owner::Index { name } => {
                let acc = index.entry(name.clone()).or_default();
                if can_clean && class == Class::Current {
                    acc.uv_clean = true;
                    acc.add(u.size, u.reclaimable, u.fetched, None);
                    // Kept aside: deleted by path if the package has nothing else cached.
                    index_cur.entry(name.clone()).or_default().push(u.path.clone());
                } else {
                    acc.add(u.size, u.reclaimable, u.fetched, Some(&u.path));
                }
            }
            Owner::Whole => whole.entry(u.bucket).or_default().add(u.size, u.reclaimable, u.fetched, Some(&u.path)),
            // Bookkeeping files are kept: counted, never targeted.
            Owner::Bookkeeping => bookkeeping.add(u.size, 0, u.fetched, None),
            Owner::Loose => unattributed.add(u.size, u.reclaimable, u.fetched, Some(&u.path)),
        }
    }
    // Outdated buckets go as a whole; their units (walked above) give the sizes.
    for (bi, b) in buckets.iter().enumerate().filter(|(_, b)| b.class == Class::Outdated) {
        let acc = outdated.entry(b.name.clone()).or_default();
        for u in units.iter().filter(|u| u.bucket == bi) {
            acc.add(u.size, u.reclaimable, u.fetched, None);
        }
        acc.paths.push(b.path.clone());
    }

    // Packages.
    let names_with_content: HashSet<String> = pkgs.keys().map(|(n, _)| n.clone()).collect();
    let mut items: Vec<(String, Version)> = Vec::new();
    for ((name, ver), acc) in pkgs {
        let v = acc.version(ver, Some((root, &name)));
        items.push((name, v));
    }
    let mut index_only = Vec::new();
    for (name, mut acc) in index {
        if names_with_content.contains(&name) {
            let v = acc.version("index".into(), Some((root, &name)));
            items.push((name, v));
        } else {
            acc.uv_clean = false;
            acc.paths.extend(index_cur.remove(&name).unwrap_or_default());
            index_only.push(acc.version(name, None));
        }
    }
    let mut out = util::group(items, Kind::Package);

    // Others.
    let mut push_other = |name: String, note: &str, versions: Vec<Version>| {
        if versions.is_empty() {
            return;
        }
        let mut p = Package::new(name, Kind::Other).with_note(note);
        p.versions = versions;
        out.push(p);
    };
    push_other(
        UNREFERENCED.into(),
        "not referenced by any wheel, sdist or environment entry; what `uv cache prune` frees",
        unref.into_iter().map(|(l, a)| a.version(l, None)).collect(),
    );
    push_other(
        OUTDATED.into(),
        "outdated cache version, left by an older uv; `uv cache prune` removes it",
        outdated.into_iter().map(|(l, a)| a.version(l, None)).collect(),
    );
    push_other(INDEX_ONLY.into(), "simple-API responses for packages with no wheels cached; re-fetched on demand", index_only);
    let mut fam_groups: BTreeMap<String, (&'static str, Vec<Version>)> = BTreeMap::new();
    for (bi, acc) in whole {
        let b = &buckets[bi];
        let (name, note) = family_other(&b.family);
        fam_groups.entry(name).or_insert((note, Vec::new())).1.push(acc.version(bucket_label(b), None));
    }
    for (name, (note, versions)) in fam_groups {
        push_other(name, note, versions);
    }
    if unknown_arch.size > 0 || !unknown_arch.paths.is_empty() {
        push_other(
            UNKNOWN_ARCHIVES.into(),
            "referenced archives without a .dist-info or pyvenv.cfg",
            vec![unknown_arch.version("-".into(), None)],
        );
    }
    if unattributed.size > 0 || !unattributed.paths.is_empty() {
        push_other(UNATTRIBUTED.into(), "unrecognized files in uv's cache layout", vec![unattributed.version("-".into(), None)]);
    }
    if bookkeeping.size > 0 {
        push_other(BOOKKEEPING.into(), "uv lock and marker files; kept", vec![bookkeeping.version("-".into(), None)]);
    }

    // Tell the user when several uv versions share this cache.
    let mut live: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for b in buckets.iter().filter(|b| b.class != Class::Outdated) {
        live.entry(b.family.as_str()).or_default().push(b.name.as_str());
    }
    let shared: Vec<String> = live.values().filter(|v| v.len() > 1).map(|v| v.join("+")).collect();
    if !shared.is_empty() {
        let current: Vec<&str> = buckets
            .iter()
            .filter(|b| b.class == Class::Current && matches!(b.family.as_str(), "wheels" | "sdists" | "simple"))
            .map(|b| b.name.as_str())
            .collect();
        ctx.warn(format!(
            "uv: several uv versions share {} ({}); `uv cache clean` via {} handles {}, other buckets are deleted by path",
            root.display(),
            shared.join(", "),
            exe.map(|e| e.display().to_string()).unwrap_or_else(|| "(no uv on PATH)".into()),
            current.join(", ")
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uv_link_counts_by_name() {
        let dir = std::env::temp_dir().join(format!("devsweep-uv-links-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let a = dir.join("a.txt");
        let b = dir.join("b.txt");
        fs::write(&a, b"hello").unwrap();
        assert_eq!(links::count(&a), 1);
        if fs::hard_link(&a, &b).is_ok() {
            assert_eq!(links::count(&a), 2);
            assert_eq!(links::count(&b), 2);
            let s = stats_links(&dir);
            assert_eq!((s.bytes, s.files, s.reclaimable), (10, 2, 0));
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn uv_bucket_names() {
        assert_eq!(parse_bucket("wheels-v7"), Some(("wheels".into(), 7)));
        assert_eq!(parse_bucket("flat-index-v4"), Some(("flat-index".into(), 4)));
        assert_eq!(parse_bucket("archive-v0"), Some(("archive".into(), 0)));
        assert_eq!(parse_bucket("CACHEDIR.TAG"), None);
        assert_eq!(parse_bucket("wheels-vx"), None);
    }

    #[test]
    fn uv_probe_binary_strings() {
        let bin = b"\x00wheels-v1\x00xwheels-v6sdists-v9\x00simple-v24simple-v12flat-index-v4\x00cpython-v1 foo-v3 wheels-v12345";
        let m = probe_bytes(bin);
        assert_eq!(m.get("wheels"), Some(&6), "wheels-v12345 is not a plausible bucket");
        assert_eq!(m.get("sdists"), Some(&9));
        assert_eq!(m.get("simple"), Some(&24));
        assert_eq!(m.get("flat-index"), Some(&4));
        assert_eq!(m.get("python"), Some(&1));
        assert_eq!(m.len(), 5);
    }

    #[test]
    fn uv_archive_ref_tokenizer() {
        let ids = IdSet::new(["iJSADXNmA3AYJe9e", "-H9lY1-hthZVuI5yethL4", "unusedXXXXXXXXXX"]);
        let mut out = Vec::new();
        ids.find(b"archive-v0/iJSADXNmA3AYJe9e", &mut out);
        assert_eq!(out, vec![0]);
        // msgpack: 0xb0 str16 marker, bare id, then 0x91.
        let mut out = Vec::new();
        ids.find(b"\x95\xb0iJSADXNmA3AYJe9e\x91\x81", &mut out);
        assert_eq!(out, vec![0]);
        // Windows separator and an id glued to other id characters.
        let mut out = Vec::new();
        ids.find(b"archive-v0\\-H9lY1-hthZVuI5yethL4\r\n7sqxwCiJSADXNmA3AYJe9ehttps", &mut out);
        out.sort();
        assert_eq!(out, vec![0, 1]);
        let mut out = Vec::new();
        ids.find(b"nothing to see archive-v0/short", &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn uv_wheel_entry_versions() {
        assert_eq!(wheel_entry_version("2.5.4-cp312-cp312-win_amd64"), "2.5.4");
        assert_eq!(wheel_entry_version("2.5.4-cp312-cp312-win_amd64.http"), "2.5.4");
        assert_eq!(wheel_entry_version("1.13.0-py3-none-any.msgpack"), "1.13.0");
        assert_eq!(wheel_entry_version("foo_bar-0.1-py3-none-any.lock"), "0.1");
        assert_eq!(wheel_entry_version("weird"), "?");
    }

    #[test]
    fn uv_toml_parsing() {
        assert_eq!(toml_cache_dir("cache-dir = \"C:\\\\uv cache\\\\x\"\n").as_deref(), Some("C:\\uv cache\\x"));
        assert_eq!(toml_cache_dir("# c\ncache-dir = 'D:\\raw\\path' # note\n").as_deref(), Some("D:\\raw\\path"));
        assert_eq!(toml_cache_dir("[pip]\ncache-dir = \"nope\"\n"), None);
        assert_eq!(toml_cache_dir("\"cache-dir\" = \"\\u0041:\\\\c\"").as_deref(), Some("A:\\c"));
    }
}
