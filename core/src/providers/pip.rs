//! pip's cache: `%LOCALAPPDATA%\pip\cache` (or `PIP_CACHE_DIR`, or `cache-dir`
//! in pip.ini).
//!
//! * `http-v2/` (pip >= 23.3): `<sha224>` holds the serialized response headers
//!   (no URL) and `<sha224>.body` the payload. Wheel bodies are zips whose
//!   `<name>-<version>.dist-info/` entry names the package; PEP 658 metadata
//!   bodies carry `Name:`/`Version:` headers. Sdists (gzip) and index pages
//!   carry no recoverable name and go to "Other" entries.
//! * `http/` (older pip): one file per response, headers and body together.
//! * `wheels/`: wheels pip built locally from sdists; the file name says it all.
//! * `selfcheck/`: pip's "new version available" timestamps.
//!
//! Read-only: files are only opened for reading.

use crate::fsutil;
use crate::model::{Kind, Package, Provider, ScanCtx, Target, Version};
use crate::providers::util;
use rayon::prelude::*;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub struct Pip;

impl Provider for Pip {
    fn id(&self) -> &'static str {
        "pip"
    }
    fn name(&self) -> &'static str {
        "pip"
    }
    fn cli_names(&self) -> &'static [&'static str] {
        &["pip", "pip3"]
    }
    fn roots(&self) -> Vec<PathBuf> {
        let default = fsutil::local_app_data().join("pip").join("cache");
        let configured = fsutil::env_path("PIP_CACHE_DIR").or_else(config_cache_dir);
        fsutil::existing(configured.into_iter().chain(Some(default)))
    }
    fn scan(&self, ctx: &ScanCtx) -> Vec<Package> {
        let mut out: Vec<Package> = Vec::new();
        for root in ctx.roots {
            merge_packages(&mut out, scan_root(root, ctx));
        }
        out
    }
}

/// `cache-dir` from the first pip config file that sets it, in pip's
/// precedence order (PIP_CONFIG_FILE, user, legacy user, site-wide).
fn config_cache_dir() -> Option<PathBuf> {
    let mut files: Vec<PathBuf> = Vec::new();
    files.extend(fsutil::env_path("PIP_CONFIG_FILE"));
    files.push(fsutil::app_data().join("pip").join("pip.ini"));
    files.push(fsutil::home().join("pip").join("pip.ini"));
    if let Some(pd) = fsutil::env_path("PROGRAMDATA") {
        files.push(pd.join("pip").join("pip.ini"));
    }
    files.iter().find_map(|f| {
        let text = fs::read_to_string(f).ok()?;
        ini_cache_dir(&text).map(|v| expand_path(&v))
    })
}

/// `cache-dir` (or `cache_dir`) from pip.ini text; `[global]` wins over other sections.
pub(crate) fn ini_cache_dir(text: &str) -> Option<String> {
    let mut section = String::new();
    let mut fallback: Option<String> = None;
    for line in text.lines() {
        let line = line.trim().trim_start_matches('\u{feff}');
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') {
            section = line.trim_matches(|c| c == '[' || c == ']').trim().to_ascii_lowercase();
            continue;
        }
        let Some(i) = line.find(['=', ':']) else { continue };
        // A Windows path value contains ':' too: only split on the first
        // separator if the key part looks like a key.
        let (k, v) = (line[..i].trim(), line[i + 1..].trim());
        if k.to_ascii_lowercase().replace('_', "-") != "cache-dir" || v.is_empty() {
            continue;
        }
        let v = v.trim_matches(|c| c == '"' || c == '\'').to_string();
        if section == "global" {
            return Some(v);
        }
        fallback.get_or_insert(v);
    }
    fallback
}

/// Expand a leading `~` and `%VAR%` references.
pub(crate) fn expand_path(s: &str) -> PathBuf {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('%') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        match after.find('%') {
            Some(j) if j > 0 => {
                let var = &after[..j];
                match std::env::var(var) {
                    Ok(val) => out.push_str(&val),
                    Err(_) => {
                        out.push('%');
                        out.push_str(var);
                        out.push('%');
                    }
                }
                rest = &after[j + 1..];
            }
            _ => {
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    if out == "~" {
        return fsutil::home();
    }
    if let Some(r) = out.strip_prefix("~/").or_else(|| out.strip_prefix("~\\")) {
        return fsutil::home().join(r);
    }
    PathBuf::from(out)
}

/// PEP 503 normalized name: lowercase, runs of `-`, `_`, `.` become one `-`.
pub(crate) fn canonical_name(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut sep = false;
    for c in s.trim().chars() {
        if c == '-' || c == '_' || c == '.' {
            sep = true;
        } else {
            if sep && !out.is_empty() {
                out.push('-');
            }
            sep = false;
            out.extend(c.to_lowercase());
        }
    }
    out
}

/// `foo_bar-1.2.0.dist-info` -> ("foo-bar", "1.2.0").
pub(crate) fn parse_dist_info(dir: &str) -> Option<(String, String)> {
    let stem = dir.strip_suffix(".dist-info").or_else(|| {
        let n = dir.len().checked_sub(".dist-info".len())?;
        dir.get(n..).filter(|s| s.eq_ignore_ascii_case(".dist-info")).map(|_| &dir[..n])
    })?;
    let (name, ver) = stem.rsplit_once('-')?;
    if name.is_empty() || ver.is_empty() {
        return None;
    }
    Some((canonical_name(name), ver.to_string()))
}

/// `foo_bar-1.0-py3-none-any.whl` (or the stem) -> ("foo-bar", "1.0").
pub(crate) fn parse_wheel_filename(file: &str) -> Option<(String, String)> {
    let stem = file.strip_suffix(".whl").unwrap_or(file);
    let parts: Vec<&str> = stem.split('-').collect();
    if parts.len() < 5 || parts.len() > 6 || parts[0].is_empty() || parts[1].is_empty() {
        return None;
    }
    if !parts[1].starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    Some((canonical_name(parts[0]), parts[1].to_string()))
}

/// Name and version from a core-metadata (METADATA / PKG-INFO) document.
pub(crate) fn parse_metadata(text: &str) -> Option<(String, String)> {
    let mut name = None;
    let mut version = None;
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            break; // end of headers, the description follows
        }
        if let Some(v) = header(line, "name") {
            name.get_or_insert(v);
        } else if let Some(v) = header(line, "version") {
            version.get_or_insert(v);
        }
    }
    match (name, version) {
        (Some(n), Some(v)) if !n.is_empty() && !v.is_empty() => Some((canonical_name(&n), v)),
        _ => None,
    }
}

fn header(line: &str, key: &str) -> Option<String> {
    let (k, v) = line.split_once(':')?;
    k.trim().eq_ignore_ascii_case(key).then(|| v.trim().to_string())
}

/// Name and version of a wheel from its central directory (no decompression).
fn wheel_identity(path: &Path) -> Result<(String, String), String> {
    let f = fs::File::open(path).map_err(|e| e.to_string())?;
    let zip = zip::ZipArchive::new(BufReader::new(f)).map_err(|e| e.to_string())?;
    let mut fallback = None;
    for raw in zip.file_names_raw() {
        let name = String::from_utf8_lossy(raw);
        let first = name.split(['/', '\\']).next().unwrap_or("");
        if let Some(id) = parse_dist_info(first) {
            return Ok(id);
        }
        // Some wheels only ship `<name>-<ver>.dist-info/...` inside a data dir;
        // keep the first nested hit as a fallback.
        if fallback.is_none() {
            fallback = name.split(['/', '\\']).find_map(parse_dist_info);
        }
    }
    fallback.ok_or_else(|| "no .dist-info entry".to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyKind {
    Zip,
    Metadata,
    Gzip,
    Index,
    Unknown,
}

fn sniff(head: &[u8]) -> BodyKind {
    let t = trim_ascii_start(head);
    if head.starts_with(b"PK\x03\x04") {
        BodyKind::Zip
    } else if head.starts_with(&[0x1f, 0x8b]) {
        BodyKind::Gzip
    } else if starts_with_ci(t, b"metadata-version:") {
        BodyKind::Metadata
    } else if starts_with_ci(t, b"<!doctype") || starts_with_ci(t, b"<html") || t.starts_with(b"{") {
        BodyKind::Index
    } else {
        BodyKind::Unknown
    }
}

fn trim_ascii_start(b: &[u8]) -> &[u8] {
    let b = b.strip_prefix(b"\xef\xbb\xbf").unwrap_or(b);
    let i = b.iter().position(|c| !c.is_ascii_whitespace()).unwrap_or(b.len());
    &b[i..]
}

fn starts_with_ci(b: &[u8], p: &[u8]) -> bool {
    b.len() >= p.len() && b[..p.len()].eq_ignore_ascii_case(p)
}

fn read_head(path: &Path, n: usize) -> Vec<u8> {
    let mut buf = Vec::with_capacity(n);
    if let Ok(f) = fs::File::open(path) {
        let _ = f.take(n as u64).read_to_end(&mut buf);
    }
    buf
}

/// One cached response (or loose file): its paths and what we learned.
struct Entry {
    paths: Vec<PathBuf>,
    /// Main file (body), used for sniffing and the fetch date.
    main: PathBuf,
    class: Class,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Class {
    Wheel(String, String),
    Metadata(String, String),
    Other(&'static str),
}

const SDISTS: &str = "Source distributions";
const INDEX: &str = "Index pages & metadata";
const UNIDENTIFIED: &str = "Unidentified responses";
const INCOMPLETE: &str = "Incomplete cache entries";
const LEGACY: &str = "Legacy HTTP cache";
const SELFCHECK: &str = "Self-check state";
const UNKNOWN: &str = "Other pip cache data";

fn other_note(name: &str) -> &'static str {
    match name {
        SDISTS => "downloaded .tar.gz sdists; pip's cache does not record their names",
        INDEX => "PyPI index pages, and metadata of versions with no cached wheel; re-fetched on demand",
        UNIDENTIFIED => "cached HTTP responses of unknown type",
        INCOMPLETE => "header file without body or body without headers; safe to delete",
        LEGACY => "http/ cache from pip < 23.3, no longer read by current pip",
        SELFCHECK => "pip's version-check timestamps, tiny",
        _ => "unrecognized files in the pip cache",
    }
}

fn classify_body(path: &Path, ctx: &ScanCtx) -> Class {
    match sniff(&read_head(path, 64)) {
        BodyKind::Zip => match wheel_identity(path) {
            Ok((n, v)) => Class::Wheel(n, v),
            Err(e) => {
                ctx.warn(format!("pip: unreadable wheel {}: {e}", path.display()));
                Class::Other(UNIDENTIFIED)
            }
        },
        BodyKind::Metadata => {
            let head = read_head(path, 16 * 1024);
            match parse_metadata(&String::from_utf8_lossy(&head)) {
                Some((n, v)) => Class::Metadata(n, v),
                None => Class::Other(INDEX),
            }
        }
        BodyKind::Gzip => Class::Other(SDISTS),
        BodyKind::Index => Class::Other(INDEX),
        BodyKind::Unknown => Class::Other(UNIDENTIFIED),
    }
}

/// All files under `dir`, walking sub-directories in parallel.
fn files_par(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = fs::read_dir(dir) else { return Vec::new() };
    let entries: Vec<_> = rd.flatten().collect();
    entries
        .par_iter()
        .flat_map(|e| {
            let Ok(ft) = e.file_type() else { return Vec::new() };
            if ft.is_symlink() {
                Vec::new()
            } else if ft.is_dir() {
                files_par(&e.path())
            } else {
                vec![e.path()]
            }
        })
        .collect()
}

/// http-v2: pair `<hash>` with `<hash>.body` and classify the bodies.
fn scan_http_v2(dir: &Path, ctx: &ScanCtx) -> Vec<Entry> {
    let mut pairs: BTreeMap<String, (Option<PathBuf>, Option<PathBuf>)> = BTreeMap::new();
    for f in files_par(dir) {
        let name = util::file_name(&f);
        match name.strip_suffix(".body") {
            Some(stem) => pairs.entry(stem.to_string()).or_default().1 = Some(f),
            None => pairs.entry(name).or_default().0 = Some(f),
        }
    }
    let pairs: Vec<_> = pairs.into_values().collect();
    pairs
        .into_par_iter()
        .map(|(meta, body)| match (meta, body) {
            (Some(m), Some(b)) => Entry { class: classify_body(&b, ctx), main: b.clone(), paths: vec![b, m] },
            (m, b) => {
                let p = b.or(m).expect("pair has at least one file");
                Entry { paths: vec![p.clone()], main: p, class: Class::Other(INCOMPLETE) }
            }
        })
        .collect()
}

/// Legacy http/: one file per response; a wheel response still ends with the
/// zip central directory, so the zip reader can identify it.
fn scan_http_legacy(dir: &Path) -> Vec<Entry> {
    files_par(dir)
        .into_par_iter()
        .map(|f| {
            let class = match wheel_identity(&f) {
                Ok((n, v)) => Class::Wheel(n, v),
                Err(_) => Class::Other(LEGACY),
            };
            Entry { paths: vec![f.clone()], main: f, class }
        })
        .collect()
}

/// wheels/: `<a>/<b>/<c>/<d>/<rest>/<name>-<ver>-<tags>.whl` plus small
/// sidecar files (origin.json) that follow the wheel they sit next to.
fn scan_wheels(dir: &Path) -> Vec<Entry> {
    let mut by_dir: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
    for f in files_par(dir) {
        by_dir.entry(f.parent().map(Path::to_path_buf).unwrap_or_default()).or_default().push(f);
    }
    let mut out = Vec::new();
    for (_, mut files) in by_dir {
        files.sort();
        let (whls, side): (Vec<PathBuf>, Vec<PathBuf>) =
            files.into_iter().partition(|f| util::file_name(f).to_ascii_lowercase().ends_with(".whl"));
        let mut side = Some(side);
        for w in whls {
            let class = match parse_wheel_filename(&util::file_name(&w)) {
                Some((n, v)) => Class::Wheel(n, v),
                None => Class::Other(UNKNOWN),
            };
            let mut paths = vec![w.clone()];
            paths.extend(side.take().unwrap_or_default());
            out.push(Entry { paths, main: w, class });
        }
        if let Some(rest) = side.filter(|s| !s.is_empty()) {
            for f in rest {
                out.push(Entry { paths: vec![f.clone()], main: f, class: Class::Other(UNKNOWN) });
            }
        }
    }
    out
}

fn scan_root(root: &Path, ctx: &ScanCtx) -> Vec<Package> {
    let Ok(rd) = fs::read_dir(root) else { return Vec::new() };
    let tops: Vec<_> = rd.flatten().collect();
    let entries: Vec<Entry> = tops
        .par_iter()
        .flat_map(|e| {
            let p = e.path();
            let is_dir = e.file_type().map(|t| t.is_dir() && !t.is_symlink()).unwrap_or(false);
            match (util::file_name(&p).to_ascii_lowercase().as_str(), is_dir) {
                ("http-v2", true) => scan_http_v2(&p, ctx),
                ("http", true) => scan_http_legacy(&p),
                ("wheels", true) => scan_wheels(&p),
                ("selfcheck", _) => vec![Entry { paths: vec![p.clone()], main: p, class: Class::Other(SELFCHECK) }],
                _ => vec![Entry { paths: vec![p.clone()], main: p, class: Class::Other(UNKNOWN) }],
            }
        })
        .collect();
    build_packages(entries)
}

#[derive(Default)]
struct Acc {
    size: u64,
    reclaimable: u64,
    fetched: Option<SystemTime>,
    targets: Vec<Target>,
}

impl Acc {
    fn add(&mut self, e: &Entry) {
        for p in &e.paths {
            let s = fsutil::dir_stats(p, true);
            self.size += s.bytes;
            self.reclaimable += s.reclaimable;
            self.targets.push(Target::Path(p.clone()));
        }
        self.fetched = fsutil::max_time(self.fetched, fsutil::fetched_time_of(&e.main));
    }
    fn into_version(self, version: String) -> Version {
        Version { version, size: self.size, reclaimable: self.reclaimable, fetched: self.fetched, targets: self.targets }
    }
}

fn build_packages(entries: Vec<Entry>) -> Vec<Package> {
    let wheels: std::collections::HashSet<(String, String)> = entries
        .iter()
        .filter_map(|e| match &e.class {
            Class::Wheel(n, v) => Some((n.clone(), v.clone())),
            _ => None,
        })
        .collect();

    // Stats per entry in parallel, then fold serially.
    // (package name + version if known, kind label, stats)
    type Resolved = (Option<(String, String)>, &'static str, Acc);
    let resolved: Vec<Resolved> = entries
        .par_iter()
        .map(|e| {
            let mut acc = Acc::default();
            acc.add(e);
            match &e.class {
                Class::Wheel(n, v) => (Some((n.clone(), v.clone())), "", acc),
                // Metadata joins its wheel; without one it is just index data.
                Class::Metadata(n, v) if wheels.contains(&(n.clone(), v.clone())) => (Some((n.clone(), v.clone())), "", acc),
                Class::Metadata(..) => (None, INDEX, acc),
                Class::Other(o) => (None, *o, acc),
            }
        })
        .collect();

    let mut pkgs: BTreeMap<(String, String), Acc> = BTreeMap::new();
    let mut others: BTreeMap<&'static str, Acc> = BTreeMap::new();
    for (key, other, acc) in resolved {
        let slot = match key {
            Some(k) => pkgs.entry(k).or_default(),
            None => others.entry(other).or_default(),
        };
        slot.size += acc.size;
        slot.reclaimable += acc.reclaimable;
        slot.fetched = fsutil::max_time(slot.fetched, acc.fetched);
        slot.targets.extend(acc.targets);
    }

    let items: Vec<(String, Version)> = pkgs.into_iter().map(|((n, v), acc)| (n, acc.into_version(v))).collect();
    let mut out = util::group(items, Kind::Package);
    for (name, acc) in others {
        let mut p = Package::new(name, Kind::Other).with_note(other_note(name));
        p.versions.push(acc.into_version("-".into()));
        out.push(p);
    }
    out
}

/// Merge packages from another root into `out` by (name, kind).
pub(crate) fn merge_packages(out: &mut Vec<Package>, more: Vec<Package>) {
    let mut idx: HashMap<(String, Kind), usize> = out.iter().enumerate().map(|(i, p)| ((p.name.clone(), p.kind), i)).collect();
    for p in more {
        match idx.get(&(p.name.clone(), p.kind)) {
            Some(&i) => out[i].versions.extend(p.versions),
            None => {
                idx.insert((p.name.clone(), p.kind), out.len());
                out.push(p);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pip_canonical_names() {
        assert_eq!(canonical_name("Foo.Bar__baz"), "foo-bar-baz");
        assert_eq!(canonical_name("typing_extensions"), "typing-extensions");
        assert_eq!(canonical_name("Django"), "django");
        assert_eq!(canonical_name("a-_-.b"), "a-b");
        assert_eq!(canonical_name("zope.interface"), "zope-interface");
    }

    #[test]
    fn pip_dist_info() {
        assert_eq!(parse_dist_info("opensearch_protobufs-1.2.0.dist-info"), Some(("opensearch-protobufs".into(), "1.2.0".into())));
        assert_eq!(parse_dist_info("numpy-2.5.4.dist-info"), Some(("numpy".into(), "2.5.4".into())));
        assert_eq!(parse_dist_info("Foo-1.0.DIST-INFO"), Some(("foo".into(), "1.0".into())));
        assert_eq!(parse_dist_info("numpy"), None);
        assert_eq!(parse_dist_info("-1.0.dist-info"), None);
    }

    #[test]
    fn pip_wheel_filenames() {
        assert_eq!(parse_wheel_filename("typing_extensions-4.12.2-py3-none-any.whl"), Some(("typing-extensions".into(), "4.12.2".into())));
        assert_eq!(parse_wheel_filename("hermes_agent-0.19.1-0.editable-py3-none-any"), Some(("hermes-agent".into(), "0.19.1".into())));
        assert_eq!(parse_wheel_filename("numpy-2.5.4-cp312-cp312-win_amd64.whl").unwrap().1, "2.5.4");
        assert_eq!(parse_wheel_filename("not-a-wheel.whl"), None);
    }

    #[test]
    fn pip_metadata_crlf() {
        let t = "Metadata-Version: 2.4\r\nName: Yarl_Lib\r\nVersion: 1.25.1\r\nSummary: x\r\n\r\nName: nope\r\n";
        assert_eq!(parse_metadata(t), Some(("yarl-lib".into(), "1.25.1".into())));
        assert_eq!(parse_metadata("Metadata-Version: 2.1\nSummary: x\n"), None);
    }

    #[test]
    fn pip_sniffing() {
        assert_eq!(sniff(b"PK\x03\x04rest"), BodyKind::Zip);
        assert_eq!(sniff(&[0x1f, 0x8b, 8, 0]), BodyKind::Gzip);
        assert_eq!(sniff(b"Metadata-Version: 2.4\n"), BodyKind::Metadata);
        assert_eq!(sniff(b"  <!DOCTYPE html>"), BodyKind::Index);
        assert_eq!(sniff(b"{\"meta\":{}}"), BodyKind::Index);
        assert_eq!(sniff(b"\x00\x01"), BodyKind::Unknown);
    }

    #[test]
    fn pip_ini_parsing() {
        let t = "[install]\ncache-dir = D:\\other\n[global]\n# comment\ncache_dir = C:\\pip cache\\x\n";
        assert_eq!(ini_cache_dir(t).as_deref(), Some("C:\\pip cache\\x"));
        assert_eq!(ini_cache_dir("[install]\ncache-dir: \"D:\\c\"\n").as_deref(), Some("D:\\c"));
        assert_eq!(ini_cache_dir("[global]\ntimeout = 60\n"), None);
    }

    #[test]
    fn pip_expand() {
        std::env::set_var("DEVSWEEP_PIP_TEST_VAR", "X:\\base");
        assert_eq!(expand_path("%DEVSWEEP_PIP_TEST_VAR%\\pip"), PathBuf::from("X:\\base\\pip"));
        assert_eq!(expand_path("C:\\100%"), PathBuf::from("C:\\100%"));
        assert_eq!(expand_path("~\\c"), fsutil::home().join("c"));
    }
}
