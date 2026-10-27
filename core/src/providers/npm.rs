//! npm cache: `_cacache` (index-v5 buckets pointing at content-v2 blobs),
//! plus `_npx` installs, `_logs` and loose files. Read-only.

use crate::fsutil;
use crate::model::{Kind, Package, Provider, ScanCtx, Target, Version};
use crate::providers::util;
use base64::Engine;
use rayon::prelude::*;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub struct Npm;

impl Provider for Npm {
    fn id(&self) -> &'static str {
        "npm"
    }
    fn name(&self) -> &'static str {
        "npm"
    }
    fn cli_names(&self) -> &'static [&'static str] {
        &["npm"]
    }
    fn roots(&self) -> Vec<PathBuf> {
        fsutil::existing([cache_dir()])
    }
    fn scan(&self, ctx: &ScanCtx) -> Vec<Package> {
        ctx.roots.iter().flat_map(|r| scan_root(r, ctx)).collect()
    }
}

/// npm's cache directory: env, then `cache=` in npmrc files, then the default.
pub fn cache_dir() -> PathBuf {
    for var in ["npm_config_cache", "NPM_CONFIG_CACHE"] {
        if let Some(p) = fsutil::env_path(var) {
            return p;
        }
    }
    let rcs = [fsutil::home().join(".npmrc"), fsutil::app_data().join("npm").join("etc").join("npmrc")];
    for rc in rcs {
        if let Some(v) = fs::read_to_string(&rc).ok().and_then(|t| npmrc_value(&t, "cache")) {
            return expand_path(&v);
        }
    }
    fsutil::local_app_data().join("npm-cache")
}

// ---------------------------------------------------------------------------
// Helpers shared with the pnpm / yarn providers.

/// Last value of `key` in an npmrc/ini style file (`key=value`, `key = "value"`).
pub(crate) fn npmrc_value(text: &str, key: &str) -> Option<String> {
    let mut found = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        if k.trim() != key {
            continue;
        }
        let v = v.trim().trim_matches('"').trim_matches('\'').trim();
        if !v.is_empty() {
            found = Some(v.to_string());
        }
    }
    found
}

/// Expand `${VAR}`, `%VAR%` and a leading `~` in a configured path.
pub(crate) fn expand_path(v: &str) -> PathBuf {
    let mut s = String::new();
    let mut rest = v;
    while let Some(i) = rest.find("${") {
        s.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        match after.find('}') {
            Some(j) => {
                s.push_str(&std::env::var(&after[..j]).unwrap_or_default());
                rest = &after[j + 1..];
            }
            None => {
                s.push_str(&rest[i..]);
                rest = "";
            }
        }
    }
    s.push_str(rest);
    // %VAR% (Windows style); unknown names are kept literally.
    let mut out = String::new();
    let mut rest = s.as_str();
    while let Some(i) = rest.find('%') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        match after.find('%') {
            Some(j) if j > 0 && std::env::var(&after[..j]).is_ok() => {
                out.push_str(&std::env::var(&after[..j]).unwrap_or_default());
                rest = &after[j + 1..];
            }
            _ => {
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    if let Some(r) = out.strip_prefix("~/").or_else(|| out.strip_prefix("~\\")) {
        return fsutil::home().join(r);
    }
    PathBuf::from(out)
}

/// A file found by `list_files`.
#[derive(Debug, Clone)]
pub(crate) struct FileInfo {
    pub path: PathBuf,
    pub len: u64,
    pub created: Option<SystemTime>,
}

pub(crate) use crate::fsutil::{read_dir_fast, RawEntry};

/// Every regular file under `dir`, walked in parallel. Symlinks/junctions skipped.
pub(crate) fn list_files(dir: &Path) -> Vec<FileInfo> {
    read_dir_fast(dir)
        .into_par_iter()
        .flat_map_iter(|e| {
            if e.is_link {
                Vec::new()
            } else if e.is_dir {
                list_files(&e.path)
            } else {
                vec![FileInfo { path: e.path, len: e.len, created: e.created }]
            }
        })
        .collect()
}

/// Total bytes of regular files under `path` (a file or directory), in parallel.
pub(crate) fn tree_bytes(path: &Path) -> u64 {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_file() => m.len(),
        Ok(m) if m.is_dir() => read_dir_fast(path)
            .into_par_iter()
            .map(|e| {
                if e.is_link {
                    0
                } else if e.is_dir {
                    tree_bytes(&e.path)
                } else {
                    e.len
                }
            })
            .sum(),
        _ => 0,
    }
}

/// Per-version accumulator used while distributing content to owners.
#[derive(Debug, Default, Clone)]
pub(crate) struct Acc {
    pub size: u64,
    pub reclaimable: u64,
    pub fetched: Option<SystemTime>,
    pub targets: Vec<Target>,
}

impl Acc {
    pub fn into_version(self, version: impl Into<String>) -> Version {
        Version { version: version.into(), size: self.size, reclaimable: self.reclaimable, fetched: self.fetched, targets: self.targets }
    }
    /// Add a file owned only by this version.
    pub fn own(&mut self, path: PathBuf, len: u64, links: u32) {
        self.size += len;
        if links <= 1 {
            self.reclaimable += len;
        }
        self.targets.push(Target::Path(path));
    }
}

/// Assign one file to its distinct `owners` (indices into `accs`). A single
/// owner gets a `Path`; several get a `Shared` target each, and the on-disk
/// size is split between them so totals never exceed the disk usage. Shared
/// bytes are never counted as reclaimable for a single version.
pub(crate) fn assign(path: &Path, len: u64, links: u32, owners: &[usize], accs: &mut [Acc]) {
    let refs = owners.len() as u64;
    match refs {
        0 => {}
        1 => accs[owners[0]].own(path.to_path_buf(), len, links),
        _ => {
            let freeable = if links <= 1 { len } else { 0 };
            for (i, &o) in owners.iter().enumerate() {
                let a = &mut accs[o];
                a.size += len / refs + if i == 0 { len % refs } else { 0 };
                a.targets.push(Target::Shared { path: path.to_path_buf(), size: freeable, refs: refs as u32 });
            }
        }
    }
}

/// An `Other` entry made of individual files.
pub(crate) fn other_files(name: &str, note: &str, files: &[(FileInfo, u32)]) -> Option<Package> {
    if files.is_empty() {
        return None;
    }
    let mut acc = Acc::default();
    for (f, links) in files {
        acc.own(f.path.clone(), f.len, *links);
        acc.fetched = fsutil::max_time(acc.fetched, f.created);
    }
    let mut p = Package::new(name, Kind::Other).with_note(note);
    p.versions.push(acc.into_version("-"));
    Some(p)
}

fn other_paths(name: &str, note: &str, paths: Vec<PathBuf>, links: bool) -> Option<Package> {
    util::other(name, note, paths, links)
}

/// An `Other` entry for unrecognized paths: counted so nothing is unaccounted,
/// but with no delete targets (the cache dir may be user-configured and shared).
pub(crate) fn size_only(name: &str, note: &str, paths: &[PathBuf]) -> Option<Package> {
    if paths.is_empty() {
        return None;
    }
    let size: u64 = paths.par_iter().map(|p| tree_bytes(p)).sum();
    let fetched = paths.iter().filter_map(|p| fsutil::fetched_time_of(p)).max();
    let mut p = Package::new(name, Kind::Other).with_note(note);
    p.versions.push(Version { version: "-".into(), size, reclaimable: 0, fetched, targets: Vec::new() });
    Some(p)
}

pub(crate) fn ms_time(ms: f64) -> Option<SystemTime> {
    if ms.is_finite() && ms > 0.0 {
        Some(UNIX_EPOCH + Duration::from_millis(ms as u64))
    } else {
        None
    }
}

/// Decode `%xx` escapes (lossy on invalid UTF-8).
pub(crate) fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok());
            if let Some(v) = hex {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Lowercase hex of the first hash in an SRI integrity string, with its algorithm.
/// "sha512-AAAA== sha1-BBB" -> ("sha512", "0000...").
pub(crate) fn integrity_hex(integrity: &str) -> Option<(String, String)> {
    let first = integrity.split_whitespace().next()?;
    let (algo, rest) = first.split_once('-')?;
    let b64 = rest.split('?').next()?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(b64.trim_end_matches('=')))
        .ok()?;
    if bytes.is_empty() {
        return None;
    }
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Some((algo.to_ascii_lowercase(), hex))
}

// ---------------------------------------------------------------------------
// Index parsing.

/// One live index entry.
#[derive(Debug, Clone, PartialEq)]
struct Entry {
    key: String,
    integrity: String,
    time: f64,
}

#[derive(Deserialize)]
struct Line {
    key: String,
    integrity: Option<String>,
    time: Option<f64>,
}

/// Parse a bucket file. Later lines supersede earlier ones for the same key;
/// a null integrity deletes the key. Returns (live entries, any line parsed).
fn parse_bucket(text: &str) -> (Vec<Entry>, bool) {
    let mut order: Vec<String> = Vec::new();
    let mut map: HashMap<String, Option<Entry>> = HashMap::new();
    for line in text.split('\n') {
        let line = line.trim_end_matches('\r');
        let Some((_hash, json)) = line.split_once('\t') else { continue };
        let Ok(l) = serde_json::from_str::<Line>(json) else { continue };
        if !map.contains_key(&l.key) {
            order.push(l.key.clone());
        }
        let e = l.integrity.filter(|i| !i.is_empty()).map(|integrity| Entry { key: l.key.clone(), integrity, time: l.time.unwrap_or(0.0) });
        map.insert(l.key, e);
    }
    let any = !order.is_empty();
    let live = order.into_iter().filter_map(|k| map.remove(&k).flatten()).collect();
    (live, any)
}

/// What an index key refers to.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Group {
    /// (package name, version or "metadata")
    Pkg(String, String),
    Advisory,
    Unknown,
}

fn classify_key(key: &str) -> Group {
    if let Some(url) = key.strip_prefix("make-fetch-happen:request-cache:") {
        return match classify_url(url) {
            Some((n, v)) => Group::Pkg(n, v),
            None => Group::Unknown,
        };
    }
    if key.starts_with("security-advisory:") {
        return Group::Advisory;
    }
    if let Some(rest) = key.strip_prefix("pacote:") {
        let (kind, spec) = rest.split_once(':').unwrap_or(("", rest));
        if spec.contains("://") {
            return match classify_url(spec) {
                Some((n, v)) => Group::Pkg(n, v),
                None => Group::Unknown,
            };
        }
        let (name, version) = util::split_at_version(spec);
        if name.is_empty() {
            return Group::Unknown;
        }
        let version = if kind.contains("tarball") {
            if version.is_empty() {
                "tarball".to_string()
            } else {
                version
            }
        } else {
            "metadata".to_string()
        };
        return Group::Pkg(name, version);
    }
    Group::Unknown
}

/// Package name and version from a registry URL. Tarballs look like
/// `<registry>/<name>/-/<basename>-<version>.tgz`; anything else is treated as
/// the package's metadata document (packument).
fn classify_url(url: &str) -> Option<(String, String)> {
    let rest = url.split_once("://").map(|x| x.1).unwrap_or(url);
    let rest = rest.split(['?', '#']).next().unwrap_or("");
    let path = rest.split_once('/').map(|x| x.1).unwrap_or("");
    let path = percent_decode(path);
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let name_of = |segs: &[&str]| -> Option<String> {
        let last = *segs.last()?;
        if segs.len() >= 2 && segs[segs.len() - 2].starts_with('@') {
            Some(format!("{}/{}", segs[segs.len() - 2], last))
        } else {
            Some(last.to_string())
        }
    };
    if let Some(i) = segs.iter().position(|s| *s == "-") {
        if i + 1 >= segs.len() {
            return None;
        }
        let name = name_of(&segs[..i])?;
        let file = segs[segs.len() - 1];
        let stem = file.strip_suffix(".tgz").or_else(|| file.strip_suffix(".tar.gz")).unwrap_or(file);
        let base = name.rsplit('/').next().unwrap_or(&name);
        let version = match stem.strip_prefix(&format!("{base}-")) {
            Some(v) if !v.is_empty() => v.to_string(),
            _ => {
                let (_, v) = util::split_dash_version(stem);
                if v.is_empty() {
                    stem.to_string()
                } else {
                    v
                }
            }
        };
        return Some((name, version));
    }
    let last = *segs.last()?;
    if let Some(stem) = last.strip_suffix(".tgz") {
        let (n, v) = util::split_dash_version(stem);
        return Some((n, if v.is_empty() { "tarball".into() } else { v }));
    }
    Some((name_of(&segs)?, "metadata".into()))
}

/// Name for an npx install from its package.json.
fn npx_label(json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let mut names: Vec<String> = v.get("dependencies").and_then(|d| d.as_object()).map(|o| o.keys().cloned().collect()).unwrap_or_default();
    if names.is_empty() {
        names = v
            .pointer("/_npx/packages")
            .and_then(|p| p.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default();
    }
    if names.is_empty() {
        None
    } else {
        Some(names.join(", "))
    }
}

// ---------------------------------------------------------------------------
// Scan.

struct Bucket {
    file: FileInfo,
    live: Vec<Entry>,
}

fn scan_root(root: &Path, ctx: &ScanCtx) -> Vec<Package> {
    let cacache = root.join("_cacache");
    let index_dir = cacache.join("index-v5");
    let content_dir = cacache.join("content-v2");

    let ((buckets, content), npx) = rayon::join(
        || {
            rayon::join(
                || {
                    list_files(&index_dir)
                        .into_par_iter()
                        .map(|file| {
                            let text = fs::read(&file.path).map(|b| String::from_utf8_lossy(&b).into_owned());
                            let live = text.map(|t| parse_bucket(&t).0).unwrap_or_default();
                            Bucket { file, live }
                        })
                        .collect::<Vec<_>>()
                },
                || {
                    list_files(&content_dir)
                        .into_par_iter()
                        .map(|f| {
                            let l = fsutil::link_count(&f.path);
                            (f, l)
                        })
                        .collect::<Vec<_>>()
                },
            )
        },
        || scan_npx(&root.join("_npx")),
    );

    let mut out = Vec::new();

    // Groups (distinct versions) and their owners.
    let mut gidx: HashMap<Group, usize> = HashMap::new();
    let mut groups: Vec<Group> = Vec::new();
    let mut accs: Vec<Acc> = Vec::new();
    let content_by_path: HashMap<&Path, usize> = content.iter().enumerate().map(|(i, (f, _))| (f.path.as_path(), i)).collect();
    let mut content_owners: Vec<Vec<usize>> = vec![Vec::new(); content.len()];
    let mut leftovers: Vec<(FileInfo, u32)> = Vec::new();
    let mut missing = 0usize;
    let mut bucket_owners: Vec<(usize, Vec<usize>)> = Vec::new();

    for (bi, b) in buckets.iter().enumerate() {
        if b.live.is_empty() {
            leftovers.push((b.file.clone(), 1));
            continue;
        }
        let mut owners: Vec<usize> = Vec::new();
        for e in &b.live {
            let g = classify_key(&e.key);
            let gi = *gidx.entry(g.clone()).or_insert_with(|| {
                groups.push(g);
                accs.push(Acc::default());
                groups.len() - 1
            });
            accs[gi].fetched = fsutil::max_time(accs[gi].fetched, ms_time(e.time));
            if !owners.contains(&gi) {
                owners.push(gi);
            }
            match integrity_hex(&e.integrity) {
                Some((algo, hex)) if hex.len() > 4 => {
                    let p = content_dir.join(&algo).join(&hex[0..2]).join(&hex[2..4]).join(&hex[4..]);
                    match content_by_path.get(p.as_path()) {
                        Some(&ci) => {
                            if !content_owners[ci].contains(&gi) {
                                content_owners[ci].push(gi);
                            }
                        }
                        None => missing += 1,
                    }
                }
                _ => missing += 1,
            }
        }
        bucket_owners.push((bi, owners));
    }
    if missing > 0 {
        ctx.warn(format!("npm: {missing} index entries point at missing content (`npm cache verify` fixes this)"));
    }

    for (bi, owners) in &bucket_owners {
        let f = &buckets[*bi].file;
        assign(&f.path, f.len, 1, owners, &mut accs);
    }
    let mut orphans: Vec<(FileInfo, u32)> = Vec::new();
    for (ci, (f, links)) in content.iter().enumerate() {
        let owners = &mut content_owners[ci];
        if owners.is_empty() {
            orphans.push((f.clone(), *links));
        } else {
            owners.sort_unstable();
            assign(&f.path, f.len, *links, owners, &mut accs);
        }
    }

    let mut items: Vec<(String, Version)> = Vec::new();
    let mut advisory: Option<Acc> = None;
    let mut unknown: Option<Acc> = None;
    for (g, acc) in groups.into_iter().zip(accs) {
        match g {
            Group::Pkg(name, version) => items.push((name, acc.into_version(version))),
            Group::Advisory => advisory = Some(acc),
            Group::Unknown => unknown = Some(acc),
        }
    }
    out.extend(util::group(items, Kind::Package));
    if let Some(a) = advisory {
        let mut p =
            Package::new("Audit advisory cache", Kind::Other).with_note("cached `npm audit` advisory lookups; re-fetched when needed");
        p.versions.push(a.into_version("-"));
        out.push(p);
    }
    if let Some(a) = unknown {
        let mut p =
            Package::new("Other registry responses", Kind::Other).with_note("cached `npm search` results and other non-package requests");
        p.versions.push(a.into_version("-"));
        out.push(p);
    }
    out.extend(other_files("Orphaned content", "blobs no index entry points to; what `npm cache verify` removes", &orphans));
    out.extend(other_files(
        "Stale index buckets",
        "index files whose entries were all deleted; `npm cache verify` removes them",
        &leftovers,
    ));
    out.extend(npx);

    // Everything else under the root, so every byte is accounted for.
    let mut misc: Vec<PathBuf> = Vec::new();
    let mut tmp: Vec<PathBuf> = Vec::new();
    let mut logs: Vec<PathBuf> = Vec::new();
    for e in fs::read_dir(root).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().to_ascii_lowercase();
        match name.as_str() {
            "_npx" => {
                // Loose files directly in _npx (dirs are handled by scan_npx).
                for f in fs::read_dir(e.path()).into_iter().flatten().flatten() {
                    if f.file_type().map(|t| !t.is_dir()).unwrap_or(false) {
                        misc.push(f.path());
                    }
                }
            }
            "_logs" => logs.push(e.path()),
            "_cacache" => {
                for c in fs::read_dir(e.path()).into_iter().flatten().flatten() {
                    match c.file_name().to_string_lossy().to_ascii_lowercase().as_str() {
                        "index-v5" | "content-v2" => {}
                        "tmp" => tmp.push(c.path()),
                        _ => misc.push(c.path()),
                    }
                }
            }
            _ => misc.push(e.path()),
        }
    }
    out.extend(other_paths("Logs", "npm debug logs; safe to delete", logs, false));
    out.extend(other_paths("Temporary files", "leftovers from interrupted downloads; safe to delete", tmp, false));
    out.extend(size_only("Other npm files", "state files and unrecognized entries; not deleted automatically", &misc));
    out
}

/// One `Other` package per distinct npx install name, one version per dir.
fn scan_npx(dir: &Path) -> Vec<Package> {
    let dirs = fsutil::subdirs(dir);
    let found: Vec<(String, Version)> = dirs
        .par_iter()
        .map(|d| {
            // No package.json means npx was interrupted mid-install.
            let label = fs::read_to_string(d.join("package.json"))
                .ok()
                .and_then(|t| npx_label(&t))
                .unwrap_or_else(|| "(incomplete install)".into());
            // npx installs are big node_modules trees: walk them in parallel.
            // npm copies files (no hard links), so skip link counting.
            let bytes = tree_bytes(d);
            let v = Version {
                version: util::file_name(d),
                size: bytes,
                reclaimable: bytes,
                fetched: fsutil::fetched_time_of(d),
                targets: vec![Target::Path(d.clone())],
            };
            (format!("npx: {label}"), v)
        })
        .collect();
    let mut map: BTreeMap<String, Vec<Version>> = BTreeMap::new();
    for (n, v) in found {
        map.entry(n).or_default().push(v);
    }
    map.into_iter()
        .map(|(name, versions)| {
            let mut p = Package::new(name, Kind::Other).with_note("temporary npx install; re-created on next npx run");
            p.versions = versions;
            p
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tarball_urls() {
        assert_eq!(
            classify_key("make-fetch-happen:request-cache:https://registry.npmjs.org/@babel/core/-/core-7.29.0.tgz"),
            Group::Pkg("@babel/core".into(), "7.29.0".into())
        );
        assert_eq!(
            classify_url("https://registry.npmjs.org/devtools-protocol/-/devtools-protocol-0.0.1608973.tgz"),
            Some(("devtools-protocol".into(), "0.0.1608973".into()))
        );
        assert_eq!(
            classify_url("https://registry.npmjs.org/@types%2fnode/-/node-20.1.0-beta.1.tgz"),
            Some(("@types/node".into(), "20.1.0-beta.1".into()))
        );
        assert_eq!(
            classify_url("https://npm.example.com/api/npm/repo/left-pad/-/left-pad-1.3.0.tgz?x=1"),
            Some(("left-pad".into(), "1.3.0".into()))
        );
    }

    #[test]
    fn packument_urls() {
        assert_eq!(
            classify_url("https://registry.npmjs.org/@rolldown%2fbinding-linux-x64-gnu"),
            Some(("@rolldown/binding-linux-x64-gnu".into(), "metadata".into()))
        );
        assert_eq!(classify_url("https://registry.npmjs.org/reusify"), Some(("reusify".into(), "metadata".into())));
        assert_eq!(classify_url("https://registry.npmjs.org/-/npm/v1/security/advisories/bulk"), None);
    }

    #[test]
    fn other_keys() {
        assert_eq!(classify_key("pacote:tarball:@nous-research/ui@0.18.2"), Group::Pkg("@nous-research/ui".into(), "0.18.2".into()));
        assert_eq!(classify_key("security-advisory:undici:abc=="), Group::Advisory);
        assert_eq!(classify_key("something-else"), Group::Unknown);
    }

    #[test]
    fn bucket_supersede_and_delete() {
        let text = "\nh1\t{\"key\":\"a\",\"integrity\":\"sha512-AAAA\",\"time\":1}\n\
                    h2\t{\"key\":\"b\",\"integrity\":\"sha512-BBBB\",\"time\":2}\n\
                    h3\t{\"key\":\"a\",\"integrity\":\"sha512-CCCC\",\"time\":3}\n\
                    h4\t{\"key\":\"b\",\"integrity\":null,\"time\":4}\n\
                    garbage line\n";
        let (live, any) = parse_bucket(text);
        assert!(any);
        assert_eq!(live, vec![Entry { key: "a".into(), integrity: "sha512-CCCC".into(), time: 3.0 }]);
        let (live, any) = parse_bucket("h\t{\"key\":\"x\",\"integrity\":null}\n");
        assert!(live.is_empty() && any);
    }

    #[test]
    fn integrity_to_hex() {
        // base64 of bytes 0x00 0x01 0xfe 0xff
        assert_eq!(integrity_hex("sha512-AAH+/w=="), Some(("sha512".into(), "0001feff".into())));
        assert_eq!(integrity_hex("sha1-AAH+/w== sha512-zzzz"), Some(("sha1".into(), "0001feff".into())));
        assert_eq!(integrity_hex("sha512-AAH+/w==?foo"), Some(("sha512".into(), "0001feff".into())));
        assert_eq!(integrity_hex("bogus"), None);
    }

    #[test]
    fn npmrc_and_npx() {
        let rc = "; comment\nregistry=https://x\ncache = \"D:\\npm cache\"\n#cache=nope\n";
        assert_eq!(npmrc_value(rc, "cache"), Some("D:\\npm cache".into()));
        assert_eq!(npmrc_value(rc, "store-dir"), None);
        assert_eq!(npx_label(r#"{"dependencies":{"codex-pets":"^0.3.0"},"_npx":{"packages":["codex-pets"]}}"#), Some("codex-pets".into()));
        assert_eq!(npx_label(r#"{"_npx":{"packages":["a","b"]}}"#), Some("a, b".into()));
        assert_eq!(percent_decode("@a%2Fb%zz%4"), "@a/b%zz%4");
        std::env::set_var("DEVSWEEP_TEST_DIR", "C:\\x");
        assert_eq!(expand_path("${DEVSWEEP_TEST_DIR}\\a"), PathBuf::from("C:\\x\\a"));
        assert_eq!(expand_path("%DEVSWEEP_TEST_DIR%\\b%"), PathBuf::from("C:\\x\\b%"));
        assert_eq!(expand_path("50%off%NOPE_UNSET_VAR%"), PathBuf::from("50%off%NOPE_UNSET_VAR%"));
    }

    /// A fake cache in a temp dir: every byte is attributed exactly once.
    #[test]
    fn scan_fake_cache() {
        let root = std::env::temp_dir().join(format!("devsweep-npm-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let w = |rel: &str, data: &[u8]| {
            let p = root.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, data).unwrap();
        };
        let line =
            |key: &str, integ: &str| format!("\nh\t{{\"key\":\"{key}\",\"integrity\":\"{integ}\",\"time\":1700000000000,\"size\":5}}");
        // bytes ab cd ef 01 02 -> content-v2/sha512/ab/cd/ef0102
        let integ = "sha512-q83vAQI=";
        w(
            "_cacache/index-v5/11/22/aaaa",
            line("make-fetch-happen:request-cache:https://registry.npmjs.org/left-pad/-/left-pad-1.3.0.tgz", integ).as_bytes(),
        );
        w("_cacache/index-v5/33/44/bbbb", line("pacote:tarball:left-pad@1.3.0", integ).as_bytes());
        w("_cacache/index-v5/55/66/cccc", b"\nh\t{\"key\":\"gone\",\"integrity\":null}");
        w("_cacache/content-v2/sha512/ab/cd/ef0102", b"tarball");
        w("_cacache/content-v2/sha512/ff/ff/orphan", b"orphan!!");
        w("_cacache/tmp/x", b"tmp");
        w("_logs/debug.log", b"log");
        w("_npx/0123/package.json", br#"{"dependencies":{"cowsay":"1"}}"#);
        w("_update-notifier-last-checked", b"");
        let total: u64 = list_files(&root).iter().map(|f| f.len).sum();

        let ctx = ScanCtx { roots: &[], warnings: std::sync::Mutex::new(Vec::new()) };
        let pkgs = scan_root(&root, &ctx);
        let sum: u64 = pkgs.iter().map(|p| p.size()).sum();
        let names: Vec<&str> = pkgs.iter().map(|p| p.name.as_str()).collect();
        let lp = pkgs.iter().find(|p| p.name == "left-pad").expect("left-pad");
        let lp_targets = lp.versions[0].targets.clone();
        let _ = fs::remove_dir_all(&root);

        assert_eq!(sum, total, "{names:?}");
        // Both keys collapse into one version that owns the content outright.
        assert_eq!(lp.versions.len(), 1);
        assert_eq!(lp.versions[0].version, "1.3.0");
        assert_eq!(lp_targets.len(), 3);
        assert!(lp_targets.iter().all(|t| matches!(t, Target::Path(_))));
        for n in ["Orphaned content", "Stale index buckets", "Temporary files", "Logs", "npx: cowsay", "Other npm files"] {
            assert!(names.contains(&n), "missing {n} in {names:?}");
        }
    }

    #[test]
    fn shared_split_never_exceeds_len() {
        let mut accs = vec![Acc::default(), Acc::default(), Acc::default()];
        assign(Path::new("x"), 10, 1, &[0, 1, 2], &mut accs);
        assert_eq!(accs.iter().map(|a| a.size).sum::<u64>(), 10);
        assert!(accs.iter().all(|a| a.reclaimable == 0));
        assert!(matches!(accs[1].targets[0], Target::Shared { refs: 3, size: 10, .. }));
        assign(Path::new("y"), 7, 2, &[1], &mut accs);
        assert_eq!(accs[1].targets[1], Target::Path("y".into()));
        assert_eq!(accs[1].reclaimable, 0);
    }
}
