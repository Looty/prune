//! NuGet: the global packages folder (`<id>/<version>/`) plus the HTTP cache,
//! plugins cache, scratch dir and the legacy v2 cache.

use super::util;
use crate::fsutil;
use crate::model::{Kind, Package, Provider, ScanCtx, Version};
use rayon::prelude::*;
use std::fs;
use std::path::{Path, PathBuf};

pub struct Nuget;

fn packages_dir() -> PathBuf {
    fsutil::env_path("NUGET_PACKAGES").unwrap_or_else(|| fsutil::home().join(".nuget").join("packages"))
}

/// Non-package caches: (path, display name, note).
fn extra_dirs() -> Vec<(PathBuf, &'static str, &'static str)> {
    let nuget = fsutil::local_app_data().join("NuGet");
    vec![
        (
            fsutil::env_path("NUGET_HTTP_CACHE_PATH").unwrap_or_else(|| nuget.join("v3-cache")),
            "http-cache",
            "HTTP responses from package feeds, re-downloaded as needed",
        ),
        (
            fsutil::env_path("NUGET_PLUGINS_CACHE_PATH").unwrap_or_else(|| nuget.join("plugins-cache")),
            "plugins-cache",
            "credential/plugin discovery cache, rebuilt automatically",
        ),
        (std::env::temp_dir().join("NuGetScratch"), "temp", "scratch files from restores, safe to delete when no restore runs"),
        (nuget.join("Cache"), "legacy v2 cache", "old nuget.exe package cache, no longer used by dotnet"),
    ]
}

impl Provider for Nuget {
    fn id(&self) -> &'static str {
        "nuget"
    }
    fn name(&self) -> &'static str {
        "NuGet"
    }
    fn cli_names(&self) -> &'static [&'static str] {
        &["dotnet", "nuget"]
    }
    fn roots(&self) -> Vec<PathBuf> {
        fsutil::existing(std::iter::once(packages_dir()).chain(extra_dirs().into_iter().map(|e| e.0)))
    }
    fn scan(&self, ctx: &ScanCtx) -> Vec<Package> {
        let pkgs = packages_dir();
        let extras = extra_dirs();
        let mut out = Vec::new();
        let mut items: Vec<(String, String, PathBuf)> = Vec::new();
        let mut leftovers = Vec::new();
        for root in ctx.roots {
            if *root == pkgs {
                collect_packages(root, &mut items, &mut leftovers);
            } else if let Some((_, name, note)) = extras.iter().find(|e| e.0 == *root) {
                out.extend(util::other(name, note, vec![root.clone()], false));
            }
        }
        let versions: Vec<_> = items.into_par_iter().map(|(id, ver, path)| (id, version(ver, path))).collect();
        let mut packages = util::group(versions, Kind::Package);
        packages.extend(out);
        packages.extend(util::other("other files", "loose files in the packages folder", leftovers, false));
        packages
    }
}

/// `<root>/<id>/<version>/`. Anything else is a leftover.
fn collect_packages(root: &Path, items: &mut Vec<(String, String, PathBuf)>, leftovers: &mut Vec<PathBuf>) {
    for (id_dir, id, is_dir) in entries(root) {
        if !is_dir {
            leftovers.push(id_dir);
            continue;
        }
        for (ver_dir, ver, is_dir) in entries(&id_dir) {
            if is_dir {
                items.push((id.clone(), ver, ver_dir));
            } else {
                leftovers.push(ver_dir);
            }
        }
    }
}

/// One version dir. Fetched = when `.nupkg.metadata` was written (the last
/// step of a successful extraction); dirs without it are incomplete.
fn version(ver: String, path: PathBuf) -> Version {
    let marker = path.join(".nupkg.metadata");
    let complete = marker.is_file();
    let label = if complete { ver } else { format!("{ver} (incomplete)") };
    let mut v = util::version_from_paths(label, vec![path], false);
    if complete {
        v.fetched = fsutil::fetched_time_of(&marker).or(v.fetched);
    }
    v
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

    /// Builds a throwaway packages folder under the system temp dir.
    #[test]
    fn layout() {
        let root = std::env::temp_dir().join(format!("devsweep-nuget-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let v1 = root.join("newtonsoft.json").join("13.0.3");
        let v2 = root.join("newtonsoft.json").join("12.0.1");
        fs::create_dir_all(&v1).unwrap();
        fs::create_dir_all(&v2).unwrap();
        fs::write(v1.join(".nupkg.metadata"), "{}").unwrap();
        fs::write(v1.join("newtonsoft.json.13.0.3.nupkg"), vec![0u8; 100]).unwrap();
        fs::write(root.join("stray.txt"), "x").unwrap();

        let mut items = Vec::new();
        let mut left = Vec::new();
        collect_packages(&root, &mut items, &mut left);
        items.sort();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].0, "newtonsoft.json");
        assert_eq!(left, vec![root.join("stray.txt")]);

        let a = version("13.0.3".into(), v1);
        assert_eq!(a.version, "13.0.3");
        assert_eq!(a.size, 102);
        assert!(a.fetched.is_some());
        let b = version("12.0.1".into(), v2);
        assert_eq!(b.version, "12.0.1 (incomplete)");
        let _ = fs::remove_dir_all(&root);
    }
}
