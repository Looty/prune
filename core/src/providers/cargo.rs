//! Cargo: `$CARGO_HOME/registry` (downloaded `.crate` files + extracted
//! sources) and `$CARGO_HOME/git` (bare repos + checkouts). Installed tools in
//! `$CARGO_HOME/bin` are never touched.

use super::util;
use crate::fsutil;
use crate::model::{Kind, Package, Provider, ScanCtx};
use rayon::prelude::*;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

pub struct Cargo;

fn cargo_home() -> PathBuf {
    fsutil::env_path("CARGO_HOME").unwrap_or_else(|| fsutil::home().join(".cargo"))
}

impl Provider for Cargo {
    fn id(&self) -> &'static str {
        "cargo"
    }
    fn name(&self) -> &'static str {
        "Cargo"
    }
    fn cli_names(&self) -> &'static [&'static str] {
        &["cargo"]
    }
    fn roots(&self) -> Vec<PathBuf> {
        let home = cargo_home();
        fsutil::existing([home.join("registry"), home.join("git")])
    }
    fn scan(&self, ctx: &ScanCtx) -> Vec<Package> {
        let mut s = State::default();
        for root in ctx.roots {
            match util::file_name(root).as_str() {
                "registry" => s.registry(root),
                "git" => s.git(root),
                _ => s.leftovers.push(root.clone()),
            }
        }
        let items: Vec<_> = s.items.into_iter().collect();
        let versions: Vec<_> =
            items.into_par_iter().map(|((name, ver), paths)| (name, util::version_from_paths(ver, paths, false))).collect();
        let mut out = util::group(versions, Kind::Package);
        out.extend(s.others);
        out.extend(util::other("other files", "cache tags, locks and unrecognized entries", s.leftovers, false));
        out
    }
}

#[derive(Default)]
struct State {
    /// (package, version) -> owned paths (`.crate` + extracted src, possibly
    /// from several registries).
    items: BTreeMap<(String, String), Vec<PathBuf>>,
    others: Vec<Package>,
    leftovers: Vec<PathBuf>,
}

impl State {
    fn add(&mut self, name: String, ver: String, path: PathBuf) {
        self.items.entry((name, ver)).or_default().push(path);
    }

    fn registry(&mut self, reg: &Path) {
        for (path, name, is_dir) in entries(reg) {
            match (name.as_str(), is_dir) {
                ("cache", true) => self.registry_dir(&path, true),
                ("src", true) => self.registry_dir(&path, false),
                ("index", true) => {
                    self.others.extend(util::other("registry index", "registry index, re-downloaded on next build", vec![path], false))
                }
                _ => self.leftovers.push(path),
            }
        }
    }

    /// `registry/cache/<index>/<name>-<ver>.crate` or `registry/src/<index>/<name>-<ver>/`.
    fn registry_dir(&mut self, dir: &Path, crate_files: bool) {
        for (idx, _, is_dir) in entries(dir) {
            if !is_dir {
                self.leftovers.push(idx);
                continue;
            }
            for (path, name, is_dir) in entries(&idx) {
                let stem = if crate_files {
                    if is_dir {
                        None
                    } else {
                        name.strip_suffix(".crate")
                    }
                } else if is_dir {
                    Some(name.as_str())
                } else {
                    None
                };
                match stem.map(split_crate) {
                    Some((n, v)) if !n.is_empty() && !v.is_empty() => self.add(n, v, path),
                    _ => self.leftovers.push(path),
                }
            }
        }
    }

    /// `git/db/<name>-<hash>` and `git/checkouts/<name>-<hash>/<rev>`.
    fn git(&mut self, git: &Path) {
        for (path, name, is_dir) in entries(git) {
            match (name.as_str(), is_dir) {
                ("db", true) => {
                    for (repo, rname, is_dir) in entries(&path) {
                        if !is_dir {
                            self.leftovers.push(repo);
                            continue;
                        }
                        let (n, hash) = split_git(&rname);
                        self.add(format!("git: {n}"), format!("{hash} db"), repo);
                    }
                }
                ("checkouts", true) => {
                    for (repo, rname, is_dir) in entries(&path) {
                        if !is_dir {
                            self.leftovers.push(repo);
                            continue;
                        }
                        let (n, hash) = split_git(&rname);
                        for (rev, revname, is_dir) in entries(&repo) {
                            if is_dir {
                                self.add(format!("git: {n}"), format!("{hash}/{revname}"), rev);
                            } else {
                                self.leftovers.push(rev);
                            }
                        }
                    }
                }
                _ => self.leftovers.push(path),
            }
        }
    }
}

/// (path, file name, is_dir) for each entry, skipping symlinks/junctions.
fn entries(dir: &Path) -> Vec<(PathBuf, String, bool)> {
    let Ok(rd) = fs::read_dir(dir) else { return Vec::new() };
    rd.flatten()
        .filter_map(|e| {
            let ft = e.file_type().ok()?;
            if ft.is_symlink() {
                return None;
            }
            Some((e.path(), e.file_name().to_string_lossy().into_owned(), ft.is_dir()))
        })
        .collect()
}

/// Full semver: `MAJOR.MINOR.PATCH[-pre][+build]`.
fn is_semver(s: &str) -> bool {
    let (rest, build) = match s.split_once('+') {
        Some((r, b)) => (r, Some(b)),
        None => (s, None),
    };
    let (core, pre) = match rest.split_once('-') {
        Some((c, p)) => (c, Some(p)),
        None => (rest, None),
    };
    let ident_ok = |x: &str| !x.is_empty() && x.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.');
    let parts: Vec<&str> = core.split('.').collect();
    parts.len() == 3
        && parts.iter().all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        && pre.is_none_or(ident_ok)
        && build.is_none_or(ident_ok)
}

/// Split "name-1.2.3" where both the crate name and the version may contain
/// '-' (`md-5-0.10.6`, `foo-1.0.0-alpha.1`, `wasi-0.11.0+wasi-snapshot-preview1`).
fn split_crate(s: &str) -> (String, String) {
    for (i, _) in s.match_indices('-') {
        if i > 0 && is_semver(&s[i + 1..]) {
            return (s[..i].to_string(), s[i + 1..].to_string());
        }
    }
    util::split_dash_version(s)
}

/// Split "name-<16 hex hash>" from git/db and git/checkouts.
fn split_git(s: &str) -> (String, String) {
    match s.rsplit_once('-') {
        Some((n, h)) if !n.is_empty() && h.len() >= 8 && h.bytes().all(|b| b.is_ascii_hexdigit()) => (n.to_string(), h.to_string()),
        _ => (s.to_string(), String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sp(s: &str) -> (String, String) {
        split_crate(s)
    }

    #[test]
    fn crate_names() {
        assert_eq!(sp("windows-sys-0.52.0"), ("windows-sys".into(), "0.52.0".into()));
        assert_eq!(sp("md-5-0.10.6"), ("md-5".into(), "0.10.6".into()));
        assert_eq!(sp("utf-8-0.7.6"), ("utf-8".into(), "0.7.6".into()));
        assert_eq!(sp("foo-1.0.0-alpha.1"), ("foo".into(), "1.0.0-alpha.1".into()));
        assert_eq!(sp("foo-bar-2.0.0-rc-1"), ("foo-bar".into(), "2.0.0-rc-1".into()));
        assert_eq!(sp("wasi-0.11.0+wasi-snapshot-preview1"), ("wasi".into(), "0.11.0+wasi-snapshot-preview1".into()));
        assert_eq!(sp("spirv-0.4.0+sdk-1.4.341.0"), ("spirv".into(), "0.4.0+sdk-1.4.341.0".into()));
        assert_eq!(sp("serde"), ("serde".into(), String::new()));
    }

    #[test]
    fn semver() {
        assert!(is_semver("1.2.3"));
        assert!(is_semver("0.0.1-beta.2+build.5"));
        assert!(!is_semver("5-0.10.6"));
        assert!(!is_semver("1.2"));
        assert!(!is_semver("1.2.3-"));
    }

    #[test]
    fn git_names() {
        assert_eq!(split_git("serde-1a2b3c4d5e6f7a8b"), ("serde".into(), "1a2b3c4d5e6f7a8b".into()));
        assert_eq!(split_git("my-crate-0123456789abcdef"), ("my-crate".into(), "0123456789abcdef".into()));
        assert_eq!(split_git("weird"), ("weird".into(), String::new()));
    }
}
