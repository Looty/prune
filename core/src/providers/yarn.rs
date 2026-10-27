//! Yarn caches. Classic (v1): `<cache>/v6/npm-<name>-<version>-<hash>-integrity/`
//! directories. Berry (v2+): a global cache of `<name>-npm-<version>-<hash>.zip`
//! archives. Read-only.

use crate::fsutil;
use crate::model::{Kind, Package, Provider, ScanCtx, Target, Version};
use crate::providers::npm::{read_dir_fast, size_only, tree_bytes, RawEntry};
use crate::providers::util;
use rayon::prelude::*;
use std::fs;
use std::path::{Path, PathBuf};

pub struct Yarn;

impl Provider for Yarn {
    fn id(&self) -> &'static str {
        "yarn"
    }
    fn name(&self) -> &'static str {
        "Yarn"
    }
    fn cli_names(&self) -> &'static [&'static str] {
        &["yarn"]
    }
    fn roots(&self) -> Vec<PathBuf> {
        let mut c = classic_dirs();
        c.extend(berry_dirs());
        fsutil::existing(c)
    }
    fn scan(&self, ctx: &ScanCtx) -> Vec<Package> {
        let mut items: Vec<(String, Version)> = Vec::new();
        let mut out = Vec::new();
        for root in ctx.roots {
            let entries = read_dir_fast(root);
            let classic = entries.iter().any(|e| e.is_dir && is_version_dir(&util::file_name(&e.path)));
            let (found, others) = if classic { scan_classic(&entries) } else { scan_berry(entries) };
            items.extend(found);
            out.extend(others);
        }
        out.extend(util::group(items, Kind::Package));
        out
    }
}

/// Classic cache folders (the parent of `v6`).
fn classic_dirs() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(p) = fsutil::env_path("YARN_CACHE_FOLDER") {
        v.push(p);
    }
    if let Some(p) = fs::read_to_string(fsutil::home().join(".yarnrc")).ok().and_then(|t| yarnrc_value(&t, "cache-folder")) {
        v.push(crate::providers::npm::expand_path(&p));
    }
    v.push(fsutil::local_app_data().join("Yarn").join("Cache"));
    v
}

/// Berry global cache folders.
fn berry_dirs() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(p) = fsutil::env_path("YARN_GLOBAL_FOLDER") {
        v.push(p.join("cache"));
    }
    if let Ok(t) = fs::read_to_string(fsutil::home().join(".yarnrc.yml")) {
        if let Some(p) = yml_value(&t, "globalFolder") {
            v.push(crate::providers::npm::expand_path(&p).join("cache"));
        }
    }
    v.push(fsutil::local_app_data().join("Yarn").join("Berry").join("cache"));
    v
}

/// `key "value"` / `key value` from a classic `.yarnrc`.
fn yarnrc_value(text: &str, key: &str) -> Option<String> {
    text.lines()
        .filter_map(|l| {
            let l = l.trim();
            let rest = l.strip_prefix(key)?;
            if !rest.starts_with([' ', '\t']) {
                return None;
            }
            let v = rest.trim().trim_matches('"').replace("\\\\", "\\");
            (!v.is_empty()).then_some(v)
        })
        .next_back()
}

/// Top-level `key: value` from `.yarnrc.yml`.
fn yml_value(text: &str, key: &str) -> Option<String> {
    text.lines()
        .filter_map(|l| {
            if l.starts_with([' ', '\t']) {
                return None;
            }
            let (k, v) = l.split_once(':')?;
            if k.trim() != key {
                return None;
            }
            let v = v.trim().trim_matches('"').trim_matches('\'');
            (!v.is_empty()).then(|| v.to_string())
        })
        .next_back()
}

fn is_version_dir(n: &str) -> bool {
    n.len() > 1 && n.starts_with('v') && n[1..].bytes().all(|b| b.is_ascii_digit())
}

fn is_hex_hash(s: &str, min: usize) -> bool {
    s.len() >= min && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Position of the first `-` followed by `\d+\.\d+\.\d+`.
fn semver_dash(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    (0..b.len()).find(|&i| b[i] == b'-' && looks_semver(&s[i + 1..]))
}

fn looks_semver(s: &str) -> bool {
    let mut parts = s.splitn(3, '.');
    let (Some(a), Some(b), Some(c)) = (parts.next(), parts.next(), parts.next()) else { return false };
    let digits = |x: &str| !x.is_empty() && x.bytes().all(|b| b.is_ascii_digit());
    digits(a) && digits(b) && c.bytes().next().is_some_and(|b| b.is_ascii_digit())
}

/// "@scope-name" -> "@scope/name" (first dash; ambiguous for dashed scopes).
fn unflatten_scope(n: &str) -> String {
    if n.starts_with('@') {
        n.replacen('-', "/", 1)
    } else {
        n.to_string()
    }
}

/// Parse a classic cache entry name. `known` is the package name read from the
/// entry's `node_modules`, which resolves scoped names with dashes.
fn parse_classic(dir: &str, known: Option<&str>) -> Option<(String, String)> {
    let s = dir.strip_prefix("npm-")?;
    let s = s.strip_suffix("-integrity").unwrap_or(s);
    let (name, rest) = match known.and_then(|k| s.strip_prefix(&format!("{}-", k.replace('/', "-"))).map(|r| (k, r))) {
        Some((k, r)) => (k.to_string(), r),
        None => {
            let i = semver_dash(s)?;
            (unflatten_scope(&s[..i]), &s[i + 1..])
        }
    };
    // Trailing content hash (sha1, 40 hex).
    let version = match rest.rsplit_once('-') {
        Some((v, h)) if is_hex_hash(h, 32) && !v.is_empty() => v,
        _ => rest,
    };
    Some((name, version.to_string()))
}

/// Parse a Berry cache archive name: `<name>-npm-<version>-<hash>[-<cachekey>].zip`.
fn parse_berry(file: &str) -> Option<(String, String)> {
    let stem = file.strip_suffix(".zip")?;
    if let Some(i) = stem.rfind("-npm-") {
        let name = &stem[..i];
        let mut rest = &stem[i + 5..];
        // Strip up to two trailing hash segments, never the version itself.
        for _ in 0..2 {
            match rest.rsplit_once('-') {
                Some((v, h)) if is_hex_hash(h, 8) && looks_semver(v) => rest = v,
                _ => break,
            }
        }
        if name.is_empty() || rest.is_empty() {
            return None;
        }
        return Some((unflatten_scope(name), rest.to_string()));
    }
    for proto in ["-patch-", "-git-", "-file-", "-exec-", "-https-", "-http-"] {
        if let Some(i) = stem.find(proto) {
            if i > 0 {
                return Some((unflatten_scope(&stem[..i]), proto.trim_matches('-').to_string()));
            }
        }
    }
    None
}

/// Package name from `<entry>/node_modules/<name>` or `<entry>/node_modules/@scope/<name>`.
fn inner_name(entry: &Path) -> Option<String> {
    let nm = entry.join("node_modules");
    let first = fsutil::subdirs(&nm).into_iter().next()?;
    let n = util::file_name(&first);
    if n.starts_with('@') {
        let inner = fsutil::subdirs(&first).into_iter().next()?;
        Some(format!("{n}/{}", util::file_name(&inner)))
    } else {
        Some(n)
    }
}

fn entry_version(version: String, e: &RawEntry) -> Version {
    let size = if e.is_dir { tree_bytes(&e.path) } else { e.len };
    Version {
        version,
        size,
        // Yarn copies out of its cache (no hard links), so everything is freed.
        reclaimable: size,
        fetched: e.created,
        targets: vec![Target::Path(e.path.clone())],
    }
}

fn scan_classic(root_entries: &[RawEntry]) -> (Vec<(String, Version)>, Vec<Package>) {
    let mut misc: Vec<PathBuf> = Vec::new();
    let mut tmp: Vec<PathBuf> = Vec::new();
    let mut found: Vec<(String, Version)> = Vec::new();
    for e in root_entries {
        let n = util::file_name(&e.path);
        if !(e.is_dir && !e.is_link && is_version_dir(&n)) {
            misc.push(e.path.clone());
            continue;
        }
        let entries = read_dir_fast(&e.path);
        let (pkgs, rest): (Vec<_>, Vec<_>) =
            entries.into_iter().partition(|x| x.is_dir && !x.is_link && util::file_name(&x.path).starts_with("npm-"));
        found.par_extend(pkgs.par_iter().map(|x| {
            let dn = util::file_name(&x.path);
            let known = inner_name(&x.path);
            let (name, ver) = parse_classic(&dn, known.as_deref())
                .or_else(|| known.clone().map(|k| (k, String::new())))
                .unwrap_or_else(|| (dn.clone(), String::new()));
            let ver = if ver.is_empty() { dn } else { ver };
            (name, entry_version(ver, x))
        }));
        for x in rest {
            if util::file_name(&x.path) == ".tmp" {
                tmp.push(x.path);
            } else {
                misc.push(x.path);
            }
        }
    }
    let mut others = Vec::new();
    others.extend(util::other("Temporary files", "leftovers from interrupted installs; safe to delete", tmp, false));
    others.extend(size_only("Other cache files", "unrecognized entries in the Yarn cache; not deleted", &misc));
    (found, others)
}

fn scan_berry(entries: Vec<RawEntry>) -> (Vec<(String, Version)>, Vec<Package>) {
    let (zips, rest): (Vec<_>, Vec<_>) = entries.into_iter().partition(|x| !x.is_dir && util::file_name(&x.path).ends_with(".zip"));
    let parsed: Vec<(Option<(String, String)>, &RawEntry)> = zips.par_iter().map(|x| (parse_berry(&util::file_name(&x.path)), x)).collect();
    let mut found = Vec::new();
    let mut misc: Vec<PathBuf> = rest.into_iter().map(|x| x.path).collect();
    for (p, x) in parsed {
        match p {
            Some((n, v)) => found.push((n, entry_version(v, x))),
            None => misc.push(x.path.clone()),
        }
    }
    let others =
        size_only("Other cache files", "temporary or unrecognized files in the Yarn cache; not deleted", &misc).into_iter().collect();
    (found, others)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classic_names() {
        let h = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(parse_classic(&format!("npm-lodash-4.17.21-{h}-integrity"), None), Some(("lodash".into(), "4.17.21".into())));
        assert_eq!(
            parse_classic(&format!("npm-utf-8-validate-5.0.10-{h}-integrity"), None),
            Some(("utf-8-validate".into(), "5.0.10".into()))
        );
        assert_eq!(
            parse_classic(&format!("npm-@babel-core-7.22.0-beta.1-{h}-integrity"), None),
            Some(("@babel/core".into(), "7.22.0-beta.1".into()))
        );
        // Dashed scope: the guess is wrong, the inner node_modules name fixes it.
        assert_eq!(
            parse_classic(&format!("npm-@angular-devkit-core-16.0.0-{h}-integrity"), Some("@angular-devkit/core")),
            Some(("@angular-devkit/core".into(), "16.0.0".into()))
        );
        assert_eq!(parse_classic(&format!("npm-left-pad-1.3.0-{h}"), None), Some(("left-pad".into(), "1.3.0".into())));
        assert_eq!(parse_classic("git-foo", None), None);
    }

    #[test]
    fn berry_names() {
        assert_eq!(parse_berry("lodash-npm-4.17.21-6382451519-c3a1b3ab41.zip"), Some(("lodash".into(), "4.17.21".into())));
        assert_eq!(parse_berry("typescript-npm-5.4.5-abcdef1234.zip"), Some(("typescript".into(), "5.4.5".into())));
        assert_eq!(
            parse_berry("@types-node-npm-20.1.0-rc.1-0123456789-abcdef0123.zip"),
            Some(("@types/node".into(), "20.1.0-rc.1".into()))
        );
        assert_eq!(parse_berry("find-npm-prefix-npm-1.0.2-b8a1c2d3e4.zip"), Some(("find-npm-prefix".into(), "1.0.2".into())));
        assert_eq!(parse_berry("resolve-patch-4254c24959-a1b2c3d4e5.zip"), Some(("resolve".into(), "patch".into())));
        assert_eq!(parse_berry(".gitignore"), None);
    }

    /// Fake classic + Berry caches in a temp dir.
    #[test]
    fn scan_fake_caches() {
        let base = std::env::temp_dir().join(format!("devsweep-yarn-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let w = |rel: &str, data: &[u8]| {
            let p = base.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, data).unwrap();
        };
        let h = "0123456789abcdef0123456789abcdef01234567";
        w(&format!("classic/v6/npm-lodash-4.17.21-{h}-integrity/node_modules/lodash/index.js"), b"lodash");
        w(&format!("classic/v6/npm-@angular-devkit-core-16.0.0-{h}-integrity/node_modules/@angular-devkit/core/a"), b"ng");
        w("classic/v6/.tmp/partial", b"tmp");
        w("berry/lodash-npm-4.17.20-6382451519-c3a1b3ab41.zip", b"zipdata");
        w("berry/.gitignore", b"*");
        let roots = [base.join("classic"), base.join("berry")];
        let ctx = ScanCtx { roots: &roots, warnings: std::sync::Mutex::new(Vec::new()) };
        let pkgs = Yarn.scan(&ctx);
        let _ = fs::remove_dir_all(&base);

        let sum: u64 = pkgs.iter().map(|p| p.size()).sum();
        assert_eq!(sum, 6 + 2 + 3 + 7 + 1);
        let lodash = pkgs.iter().find(|p| p.name == "lodash").unwrap();
        let mut vs: Vec<&str> = lodash.versions.iter().map(|v| v.version.as_str()).collect();
        vs.sort();
        assert_eq!(vs, ["4.17.20", "4.17.21"]);
        assert!(pkgs.iter().any(|p| p.name == "@angular-devkit/core"));
        assert!(pkgs.iter().any(|p| p.name == "Temporary files"));
    }

    #[test]
    fn config_files() {
        assert_eq!(yarnrc_value("cache-folder \"D:\\\\yarn\"\nother x\n", "cache-folder"), Some("D:\\yarn".into()));
        assert_eq!(yarnrc_value("cache-folderx y\n", "cache-folder"), None);
        assert_eq!(
            yml_value("nodeLinker: pnp\nglobalFolder: 'E:\\y'\npackageExtensions:\n  globalFolder: no\n", "globalFolder"),
            Some("E:\\y".into())
        );
    }
}
