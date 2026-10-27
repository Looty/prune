//! Small helpers for providers.

use crate::fsutil::{self, Stats};
use crate::model::{Kind, Package, Target, Version};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A version backed by whole paths (dirs or files) that it owns exclusively.
pub fn version_from_paths(version: impl Into<String>, paths: Vec<PathBuf>, links: bool) -> Version {
    let mut st = Stats::default();
    let mut fetched = None;
    for p in &paths {
        let s = fsutil::dir_stats(p, links);
        st.bytes += s.bytes;
        st.reclaimable += s.reclaimable;
        fetched = fsutil::max_time(fetched, fsutil::fetched_time_of(p));
    }
    Version {
        version: version.into(),
        size: st.bytes,
        reclaimable: st.reclaimable,
        fetched,
        targets: paths.into_iter().map(Target::Path).collect(),
    }
}

/// An "Other" entry covering whole paths, e.g. logs or a build cache.
pub fn other(name: &str, note: &str, paths: Vec<PathBuf>, links: bool) -> Option<Package> {
    let paths: Vec<PathBuf> = paths.into_iter().filter(|p| p.exists()).collect();
    if paths.is_empty() {
        return None;
    }
    let v = version_from_paths("-", paths, links);
    let mut p = Package::new(name, Kind::Other).with_note(note);
    p.versions.push(v);
    Some(p)
}

/// Group (name, version) pairs into packages.
pub fn group(items: Vec<(String, Version)>, kind: Kind) -> Vec<Package> {
    let mut map: BTreeMap<String, Vec<Version>> = BTreeMap::new();
    for (name, v) in items {
        map.entry(name).or_default().push(v);
    }
    map.into_iter()
        .map(|(name, mut versions)| {
            versions.sort_by_key(|a| std::cmp::Reverse(a.fetched));
            Package { name, kind, note: None, versions }
        })
        .collect()
}

/// Split "name@1.2.3" / "@scope/name@1.2.3" into (name, version).
pub fn split_at_version(s: &str) -> (String, String) {
    let search_from = if s.starts_with('@') { 1 } else { 0 };
    match s[search_from..].find('@') {
        Some(i) => {
            let i = i + search_from;
            (s[..i].to_string(), s[i + 1..].to_string())
        }
        None => (s.to_string(), String::new()),
    }
}

/// Split "name-1.2.3" (cargo crate files, wheel names...) at the first '-'
/// followed by a digit-led semver-ish version.
pub fn split_dash_version(s: &str) -> (String, String) {
    let b = s.as_bytes();
    for i in 0..b.len() {
        if b[i] == b'-' && i + 1 < b.len() && b[i + 1].is_ascii_digit() {
            let rest = &s[i + 1..];
            if rest.contains('.') {
                return (s[..i].to_string(), rest.to_string());
            }
        }
    }
    (s.to_string(), String::new())
}

pub fn file_name(p: &Path) -> String {
    p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}
