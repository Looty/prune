//! Go: the module cache (GOMODCACHE: extracted `module@version` dirs plus
//! `cache/download/<module>/@v/<version>.*`) and the build cache (GOCACHE).

use super::util;
use crate::fsutil;
use crate::model::{Kind, Package, Provider, ScanCtx, Target};
use rayon::prelude::*;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

pub struct Go;

/// Settings written by `go env -w` (%APPDATA%\go\env). Plain file read, no process.
fn env_file() -> HashMap<String, String> {
    let Ok(text) = fs::read_to_string(fsutil::app_data().join("go").join("env")) else {
        return HashMap::new();
    };
    text.lines().filter_map(|l| l.split_once('=')).map(|(k, v)| (k.trim().to_string(), v.trim().to_string())).collect()
}

fn go_var(name: &str, file: &HashMap<String, String>) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty()).or_else(|| file.get(name).filter(|v| !v.is_empty()).cloned())
}

fn modcache(file: &HashMap<String, String>) -> PathBuf {
    if let Some(m) = go_var("GOMODCACHE", file) {
        return PathBuf::from(m);
    }
    let gopath = go_var("GOPATH", file).and_then(|g| std::env::split_paths(&g).next()).unwrap_or_else(|| fsutil::home().join("go"));
    gopath.join("pkg").join("mod")
}

fn gocache(file: &HashMap<String, String>) -> Option<PathBuf> {
    match go_var("GOCACHE", file) {
        Some(v) if v == "off" => None,
        Some(v) => Some(PathBuf::from(v)),
        None => Some(fsutil::local_app_data().join("go-build")),
    }
}

impl Provider for Go {
    fn id(&self) -> &'static str {
        "go"
    }
    fn name(&self) -> &'static str {
        "Go"
    }
    fn cli_names(&self) -> &'static [&'static str] {
        &["go"]
    }
    fn roots(&self) -> Vec<PathBuf> {
        let f = env_file();
        fsutil::existing([Some(modcache(&f)), gocache(&f)].into_iter().flatten())
    }
    fn scan(&self, ctx: &ScanCtx) -> Vec<Package> {
        let f = env_file();
        let build = gocache(&f);
        let mut s = State::default();
        for root in ctx.roots {
            if Some(root) == build.as_ref() {
                s.others.extend(util::other("build cache", "build cache, rebuilt automatically", vec![root.clone()], false));
            } else {
                s.walk_extracted(root, "", 0);
            }
        }

        let items: Vec<_> = s.items.into_iter().collect();
        let versions: Vec<_> =
            items.into_par_iter().map(|((name, ver), paths)| (name, util::version_from_paths(ver, paths, false))).collect();
        let mut out = util::group(versions, Kind::Package);

        // Per-module metadata (`@v/list`, stray files) goes with the module.
        let mut meta = s.meta;
        for p in &mut out {
            if let Some(paths) = meta.remove(&p.name) {
                attach_meta(p, paths);
            }
        }
        // Metadata of modules with no cached version left.
        s.leftovers.extend(meta.into_values().flatten());

        out.extend(s.others);
        out.extend(util::other("other files", "locks, version lists of uncached modules and unrecognized entries", s.leftovers, false));
        out
    }
}

#[derive(Default)]
struct State {
    /// (module, version) -> extracted dir + download files.
    items: BTreeMap<(String, String), Vec<PathBuf>>,
    /// module -> metadata files shared by all its versions.
    meta: BTreeMap<String, Vec<PathBuf>>,
    others: Vec<Package>,
    leftovers: Vec<PathBuf>,
}

impl State {
    /// GOMODCACHE tree: descend until a dir name contains '@'.
    fn walk_extracted(&mut self, dir: &Path, rel: &str, depth: usize) {
        for (path, name, is_dir) in entries(dir) {
            if !is_dir {
                self.leftovers.push(path);
            } else if depth == 0 && name == "cache" {
                self.cache(&path);
            } else if let Some((m, v)) = name.split_once('@') {
                let module = unescape(&join(rel, m));
                self.items.entry((module, unescape(v))).or_default().push(path);
            } else {
                self.walk_extracted(&path, &join(rel, &name), depth + 1);
            }
        }
    }

    /// GOMODCACHE/cache: download/, vcs/, lock.
    fn cache(&mut self, dir: &Path) {
        for (path, name, is_dir) in entries(dir) {
            match (name.as_str(), is_dir) {
                ("download", true) => self.walk_download(&path, "", 0),
                ("vcs", true) => self.others.extend(util::other(
                    "vcs clones",
                    "git clones for direct VCS fetches, re-cloned on demand",
                    vec![path],
                    false,
                )),
                _ => self.leftovers.push(path),
            }
        }
    }

    /// cache/download/<escaped module>/@v/... ; modules may nest (foo/bar and foo/bar/v2).
    fn walk_download(&mut self, dir: &Path, rel: &str, depth: usize) {
        for (path, name, is_dir) in entries(dir) {
            if !is_dir {
                self.leftovers.push(path);
            } else if depth == 0 && name == "sumdb" {
                self.others.extend(util::other("checksum database", "sumdb lookups and tiles, re-downloaded on demand", vec![path], false));
            } else if name == "@v" {
                self.version_files(&path, unescape(rel));
            } else {
                self.walk_download(&path, &join(rel, &name), depth + 1);
            }
        }
    }

    fn version_files(&mut self, dir: &Path, module: String) {
        for (path, name, is_dir) in entries(dir) {
            match (!is_dir).then(|| version_of_file(&name)).flatten() {
                Some(v) => self.items.entry((module.clone(), unescape(v))).or_default().push(path),
                None => self.meta.entry(module.clone()).or_default().push(path),
            }
        }
    }
}

fn join(rel: &str, name: &str) -> String {
    if rel.is_empty() {
        name.to_string()
    } else {
        format!("{rel}/{name}")
    }
}

/// "v1.2.3.zip" -> "v1.2.3". `list`/`list.lock` and unknown names -> None.
fn version_of_file(name: &str) -> Option<&str> {
    const EXT: [&str; 7] = [".partial", ".tmp", ".ziphash", ".zip", ".mod", ".info", ".lock"];
    let mut s = name;
    let mut stripped = false;
    while let Some(e) = EXT.iter().find(|e| s.ends_with(*e)) {
        s = &s[..s.len() - e.len()];
        stripped = true;
    }
    (stripped && s.starts_with('v') && s.len() > 1).then_some(s)
}

/// Go's case encoding for module paths and versions: "!x" -> "X".
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut upper = false;
    for c in s.chars() {
        if c == '!' {
            upper = true;
        } else if upper {
            out.extend(c.to_uppercase());
            upper = false;
        } else {
            out.push(c);
        }
    }
    out
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
    fn unescapes_case() {
        assert_eq!(unescape("github.com/!burnt!sushi/toml"), "github.com/BurntSushi/toml");
        assert_eq!(unescape("github.com/!azure/azure-sdk-for-go"), "github.com/Azure/azure-sdk-for-go");
        assert_eq!(unescape("v0.0.0-20220816155156-cfacd8902214"), "v0.0.0-20220816155156-cfacd8902214");
        assert_eq!(unescape("v1.0.0-!r!c1"), "v1.0.0-RC1");
    }

    #[test]
    fn download_file_versions() {
        assert_eq!(version_of_file("v1.2.3.zip"), Some("v1.2.3"));
        assert_eq!(version_of_file("v1.2.3.mod"), Some("v1.2.3"));
        assert_eq!(version_of_file("v2.2.0+incompatible.info"), Some("v2.2.0+incompatible"));
        assert_eq!(version_of_file("v0.0.14.ziphash"), Some("v0.0.14"));
        assert_eq!(version_of_file("v1.0.0.zip.partial"), Some("v1.0.0"));
        assert_eq!(version_of_file("v1.0.0.lock"), Some("v1.0.0"));
        assert_eq!(version_of_file("list"), None);
        assert_eq!(version_of_file("list.lock"), None);
        assert_eq!(version_of_file("README"), None);
    }

    /// Throwaway GOMODCACHE under the system temp dir.
    #[test]
    fn layout() {
        let root = std::env::temp_dir().join(format!("devsweep-go-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let mk = |p: &str| fs::create_dir_all(root.join(p)).unwrap();
        let wr = |p: &str| fs::write(root.join(p), "x").unwrap();
        mk("github.com/!burnt!sushi/toml@v1.2.0");
        mk("github.com/org/cache/v2@v2.0.0"); // a module named ".../cache"
        mk("go.opencensus.io@v0.23.0");
        mk("cache/download/github.com/!burnt!sushi/toml/@v");
        mk("cache/download/github.com/!burnt!sushi/toml/v2/@v");
        mk("cache/download/sumdb/sum.golang.org");
        wr("cache/download/github.com/!burnt!sushi/toml/@v/v1.2.0.zip");
        wr("cache/download/github.com/!burnt!sushi/toml/@v/v1.2.0.mod");
        wr("cache/download/github.com/!burnt!sushi/toml/@v/v0.9.0.mod");
        wr("cache/download/github.com/!burnt!sushi/toml/@v/list");
        wr("cache/download/github.com/!burnt!sushi/toml/v2/@v/v2.0.0.info");
        wr("cache/lock");

        let mut s = State::default();
        s.walk_extracted(&root, "", 0);
        let keys: Vec<(&str, &str, usize)> = s.items.iter().map(|((m, v), p)| (m.as_str(), v.as_str(), p.len())).collect();
        assert_eq!(
            keys,
            vec![
                ("github.com/BurntSushi/toml", "v0.9.0", 1),
                ("github.com/BurntSushi/toml", "v1.2.0", 3),
                ("github.com/BurntSushi/toml/v2", "v2.0.0", 1),
                ("github.com/org/cache/v2", "v2.0.0", 1),
                ("go.opencensus.io", "v0.23.0", 1),
            ]
        );
        assert_eq!(s.meta.get("github.com/BurntSushi/toml").map(|v| v.len()), Some(1));
        assert_eq!(s.others.len(), 1); // sumdb
        assert_eq!(s.leftovers, vec![root.join("cache").join("lock")]);
        let _ = fs::remove_dir_all(&root);
    }
}
