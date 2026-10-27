//! Bun's global install cache:
//! - `<name>@<version>@@@<n>/` and `@scope/<name>@<version>@@@<n>/`: extracted packages
//!   (files are usually hard-linked into node_modules, so reclaimable < size);
//! - `<name>/` and `@scope/<name>/`: junctions `<version>@@@<n>` to the dirs above;
//! - `<hash>.npm`: registry manifests (one per package name);
//! - `@t@/`: the runtime transpiler cache.

use super::util;
use crate::fsutil;
use crate::model::{Kind, Package, Provider, ScanCtx, Target};
use rayon::prelude::*;
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

pub struct Bun;

fn cache_dir() -> PathBuf {
    fsutil::env_path("BUN_INSTALL_CACHE_DIR")
        .or_else(|| fsutil::env_path("BUN_INSTALL").map(|p| p.join("install").join("cache")))
        .unwrap_or_else(|| fsutil::home().join(".bun").join("install").join("cache"))
}

impl Provider for Bun {
    fn id(&self) -> &'static str {
        "bun"
    }
    fn name(&self) -> &'static str {
        "Bun"
    }
    fn cli_names(&self) -> &'static [&'static str] {
        &["bun"]
    }
    fn roots(&self) -> Vec<PathBuf> {
        fsutil::existing([cache_dir()])
    }
    fn scan(&self, ctx: &ScanCtx) -> Vec<Package> {
        let mut s = State::default();
        for root in ctx.roots {
            s.collect(root);
        }

        let versions: Vec<_> =
            s.versions.into_par_iter().map(|(name, ver, path)| (name, util::version_from_paths(ver, vec![path], true))).collect();
        let mut out = util::group(versions, Kind::Package);

        // Manifests and link folders belong to their package.
        let named: Vec<(Option<String>, PathBuf)> = s.manifests.into_par_iter().map(|p| (read_manifest_name(&p), p)).collect();
        let mut meta: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
        let mut stray_manifests = Vec::new();
        for (name, p) in named {
            match name {
                Some(n) => meta.entry(n).or_default().push(p),
                None => stray_manifests.push(p),
            }
        }
        let mut holders = s.holders;
        for p in &mut out {
            let mut paths = meta.remove(&p.name).unwrap_or_default();
            paths.extend(holders.remove(&p.name));
            attach_meta(p, paths);
        }
        stray_manifests.extend(meta.into_values().flatten());

        out.extend(util::other(
            "registry manifests",
            "npm metadata of packages not in the cache, re-downloaded on demand",
            stray_manifests,
            false,
        ));
        out.extend(util::other(
            "stale links",
            "link folders pointing at versions no longer cached",
            holders.into_values().collect(),
            false,
        ));
        out.extend(
            s.transpiler
                .and_then(|p| util::other("transpiler cache", "runtime transpiler cache (@t@), rebuilt automatically", vec![p], false)),
        );
        out.extend(util::other("other files", "unrecognized cache entries", s.leftovers, true));
        out
    }
}

#[derive(Default)]
struct State {
    /// (package name, version, extracted dir)
    versions: Vec<(String, String, PathBuf)>,
    /// package name -> folder of junctions to its versions
    holders: BTreeMap<String, PathBuf>,
    manifests: Vec<PathBuf>,
    transpiler: Option<PathBuf>,
    leftovers: Vec<PathBuf>,
}

impl State {
    fn collect(&mut self, root: &Path) {
        for (path, name, is_dir) in entries(root) {
            if !is_dir {
                if name.ends_with(".npm") {
                    self.manifests.push(path);
                } else {
                    self.leftovers.push(path);
                }
            } else if name == "@t@" {
                self.transpiler = Some(path);
            } else if let Some((n, v)) = parse_entry(&name, None) {
                self.versions.push((n, v, path));
            } else if name.starts_with('@') {
                for (sub, sname, is_dir) in entries(&path) {
                    if !is_dir {
                        self.leftovers.push(sub);
                    } else if let Some((n, v)) = parse_entry(&sname, Some(&name)) {
                        self.versions.push((n, v, sub));
                    } else {
                        self.holders.insert(format!("{name}/{sname}"), sub);
                    }
                }
            } else {
                self.holders.insert(name, path);
            }
        }
    }
}

/// "acorn@8.16.0@@@1" -> ("acorn", "8.16.0"); inside "@scope": "sdk@1.0.0@@@1" -> ("@scope/sdk", "1.0.0").
fn parse_entry(dir: &str, scope: Option<&str>) -> Option<(String, String)> {
    let (base, _) = dir.rsplit_once("@@@")?;
    let (name, ver) = util::split_at_version(base);
    if name.is_empty() {
        return None;
    }
    let name = match scope {
        Some(s) => format!("{s}/{name}"),
        None => name,
    };
    Some((name, ver))
}

/// Read the package name from a `.npm` manifest (binary format; the first
/// tarball URL `https://<registry>/<name>/-/<file>.tgz` appears near the top).
fn read_manifest_name(path: &Path) -> Option<String> {
    let mut buf = vec![0u8; 8192];
    let mut f = fs::File::open(path).ok()?;
    let n = f.read(&mut buf).ok()?;
    manifest_name(&buf[..n])
}

fn manifest_name(bytes: &[u8]) -> Option<String> {
    let s = String::from_utf8_lossy(bytes);
    let h = s.find("://")?;
    let after = &s[h + 3..];
    let end = after.find("/-/")?;
    let url = &after[..end];
    let path = &url[url.find('/')? + 1..];
    let path = path.replace("%2f", "/").replace("%2F", "/").replace("%40", "@");
    if path.is_empty() || path.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return None;
    }
    let segs: Vec<&str> = path.split('/').filter(|x| !x.is_empty()).collect();
    match segs.as_slice() {
        [.., scope, name] if scope.starts_with('@') => Some(format!("{scope}/{name}")),
        [.., name] => Some(name.to_string()),
        [] => None,
    }
}

/// Attach files shared by all versions of a package. Their bytes are counted
/// once (on the newest version); they are deleted with the last version.
fn attach_meta(pkg: &mut Package, paths: Vec<PathBuf>) {
    let n = pkg.versions.len() as u32;
    if n == 0 || paths.is_empty() {
        return;
    }
    let mut total = 0;
    for path in paths {
        let size = fsutil::dir_stats(&path, false).bytes;
        total += size;
        if n == 1 {
            pkg.versions[0].targets.push(Target::Path(path));
        } else {
            for v in &mut pkg.versions {
                v.targets.push(Target::Shared { path: path.clone(), size, refs: n });
            }
        }
    }
    pkg.versions[0].size += total;
    if n == 1 {
        pkg.versions[0].reclaimable += total;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_names() {
        assert_eq!(parse_entry("acorn@8.16.0@@@1", None), Some(("acorn".into(), "8.16.0".into())));
        assert_eq!(parse_entry("sdk@0.21.0@@@1", Some("@anthropic-ai")), Some(("@anthropic-ai/sdk".into(), "0.21.0".into())));
        assert_eq!(parse_entry("vite@7.0.0-beta.1+build@@@1", None), Some(("vite".into(), "7.0.0-beta.1+build".into())));
        assert_eq!(parse_entry("acorn", None), None);
        assert_eq!(parse_entry("@babel", None), None);
    }

    #[test]
    fn manifest_names() {
        let mut b = b"#!/usr/bin/env bun\nbun-npm-manifest-cache-v0.0.7\n\x00\x01".to_vec();
        b.extend_from_slice(b"\x00lightningcss-linux-arm64-gnuhttps://registry.npmjs.org/lightningcss-linux-arm64-gnu/-/x-1.0.0.tgz");
        assert_eq!(manifest_name(&b).as_deref(), Some("lightningcss-linux-arm64-gnu"));
        let b = b"\x00https://registry.npmjs.org/@rollup/rollup-freebsd-arm64/-/rollup-freebsd-arm64-4.0.0.tgz";
        assert_eq!(manifest_name(b).as_deref(), Some("@rollup/rollup-freebsd-arm64"));
        let b = b"https://npm.example.com/api/npm/repo/@scope%2fpkg/-/pkg-1.0.0.tgz";
        assert_eq!(manifest_name(b).as_deref(), Some("@scope/pkg"));
        assert_eq!(manifest_name(b"no url here"), None);
    }
}
