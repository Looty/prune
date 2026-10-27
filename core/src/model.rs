use std::path::PathBuf;
use std::time::SystemTime;

/// Whether an entry is a real downloadable package or a provider-level extra
/// (logs, build cache, index metadata, unidentified blobs...).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Package,
    Other,
}

/// One thing that must be removed to delete a version from the cache.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Target {
    /// A file or directory owned only by this version.
    Path(PathBuf),
    /// A file shared by `refs` versions (content-addressed stores).
    /// It is deleted only when every owner is selected.
    Shared { path: PathBuf, size: u64, refs: u32 },
    /// A row in pnpm's `index.db` (v11+ store).
    PnpmIndexRow { db: PathBuf, key: String },
    /// Let `uv cache clean <package>` remove it (uv holds its own locks).
    UvClean { cache_dir: PathBuf, package: String },
}

#[derive(Debug, Clone)]
pub struct Version {
    pub version: String,
    /// Bytes on disk owned by this version. Shared content is split between
    /// its owners, so provider totals never exceed the real disk usage.
    pub size: u64,
    /// Bytes actually freed if deleted alone: excludes hard-linked files and
    /// content shared with other versions.
    pub reclaimable: u64,
    /// When the version was downloaded into the cache.
    pub fetched: Option<SystemTime>,
    pub targets: Vec<Target>,
}

#[derive(Debug, Clone)]
pub struct Package {
    pub name: String,
    pub kind: Kind,
    /// Short human hint shown in the UI, e.g. "build cache, safe to delete".
    pub note: Option<String>,
    pub versions: Vec<Version>,
}

impl Package {
    pub fn new(name: impl Into<String>, kind: Kind) -> Self {
        Self { name: name.into(), kind, note: None, versions: Vec::new() }
    }
    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }
    pub fn size(&self) -> u64 {
        self.versions.iter().map(|v| v.size).sum()
    }
    pub fn reclaimable(&self) -> u64 {
        self.versions.iter().map(|v| v.reclaimable).sum()
    }
    /// Most recent fetch across versions.
    pub fn fetched(&self) -> Option<SystemTime> {
        self.versions.iter().filter_map(|v| v.fetched).max()
    }
}

#[derive(Debug, Clone)]
pub struct ProviderReport {
    pub id: &'static str,
    pub name: &'static str,
    /// Resolved CLI executable, if found on PATH.
    pub cli: Option<PathBuf>,
    /// Cache directories that were scanned.
    pub roots: Vec<PathBuf>,
    /// Total bytes on disk under `roots` (independent walk, for checking).
    pub disk_size: u64,
    pub packages: Vec<Package>,
    pub scan_ms: u128,
    pub warnings: Vec<String>,
}

impl ProviderReport {
    pub fn package_size(&self) -> u64 {
        self.packages.iter().map(|p| p.size()).sum()
    }
    pub fn reclaimable(&self) -> u64 {
        self.packages.iter().map(|p| p.reclaimable()).sum()
    }
}

/// Context handed to a provider's scan.
pub struct ScanCtx<'a> {
    pub roots: &'a [PathBuf],
    pub warnings: std::sync::Mutex<Vec<String>>,
}

impl ScanCtx<'_> {
    pub fn warn(&self, msg: impl Into<String>) {
        if let Ok(mut w) = self.warnings.lock() {
            w.push(msg.into());
        }
    }
}

pub trait Provider: Sync + Send {
    /// Stable id used in settings, e.g. "npm".
    fn id(&self) -> &'static str;
    /// Display name, e.g. "npm".
    fn name(&self) -> &'static str;
    /// Executable names to look for on PATH (without extension).
    fn cli_names(&self) -> &'static [&'static str];
    /// Cache directories that exist on this machine. Must be fast (env vars,
    /// defaults, config files). Return empty when the tool has no cache here.
    fn roots(&self) -> Vec<PathBuf>;
    /// Enumerate cached packages. Read-only: must never modify anything.
    fn scan(&self, ctx: &ScanCtx) -> Vec<Package>;
}
