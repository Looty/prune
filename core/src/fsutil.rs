//! File-system helpers shared by providers. Everything here is read-only
//! except `remove_force`, which only `clean::execute` calls.

use rayon::prelude::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub bytes: u64,
    pub files: u64,
    /// Bytes in files with a single hard link (freed when deleted).
    pub reclaimable: u64,
    pub newest_mtime: Option<SystemTime>,
}

impl Stats {
    fn add(&mut self, o: Stats) {
        self.bytes += o.bytes;
        self.files += o.files;
        self.reclaimable += o.reclaimable;
        self.newest_mtime = max_time(self.newest_mtime, o.newest_mtime);
    }
}

pub fn max_time(a: Option<SystemTime>, b: Option<SystemTime>) -> Option<SystemTime> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (x, None) => x,
        (None, y) => y,
    }
}

/// Creation time (= when it was fetched into the cache), falling back to mtime.
pub fn fetched_time(meta: &fs::Metadata) -> Option<SystemTime> {
    meta.created().ok().or_else(|| meta.modified().ok())
}

pub fn fetched_time_of(path: &Path) -> Option<SystemTime> {
    fs::symlink_metadata(path).ok().as_ref().and_then(fetched_time)
}

/// One directory entry with the metadata the OS listing already provides.
#[derive(Debug, Clone)]
pub struct RawEntry {
    pub path: PathBuf,
    pub is_dir: bool,
    /// Symlink or junction (never followed).
    pub is_link: bool,
    pub len: u64,
    pub created: Option<SystemTime>,
    pub modified: Option<SystemTime>,
}

/// List a directory without touching each entry. On Windows `DirEntry::metadata`
/// can open every file (very slow under real-time antivirus), whereas
/// `FindFirstFileExW` returns sizes and times straight from the listing.
#[cfg(windows)]
pub fn read_dir_fast(dir: &Path) -> Vec<RawEntry> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        FindClose, FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW, FindNextFileW, FILE_ATTRIBUTE_DIRECTORY,
        FILE_ATTRIBUTE_REPARSE_POINT, FIND_FIRST_EX_LARGE_FETCH, WIN32_FIND_DATAW,
    };
    let pattern = dir.join("*");
    let s = pattern.as_os_str().to_string_lossy();
    // Long paths need the verbatim prefix.
    let pattern =
        if s.len() >= 248 && !s.starts_with(r"\\") && pattern.is_absolute() { PathBuf::from(format!(r"\\?\{s}")) } else { pattern };
    let wide: Vec<u16> = pattern.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut out = Vec::new();
    unsafe {
        let mut fd: WIN32_FIND_DATAW = std::mem::zeroed();
        let h = FindFirstFileExW(
            wide.as_ptr(),
            FindExInfoBasic,
            &mut fd as *mut WIN32_FIND_DATAW as *mut core::ffi::c_void,
            FindExSearchNameMatch,
            std::ptr::null(),
            FIND_FIRST_EX_LARGE_FETCH,
        );
        if h == INVALID_HANDLE_VALUE {
            return read_dir_std(dir);
        }
        loop {
            let n = fd.cFileName.iter().position(|&c| c == 0).unwrap_or(fd.cFileName.len());
            let name = &fd.cFileName[..n];
            let dot = b'.' as u16;
            if name != [dot] && name != [dot, dot] {
                let attrs = fd.dwFileAttributes;
                // Only name-surrogate reparse points (symlinks, junctions) are links;
                // cloud placeholders and dedup files are regular entries.
                let is_link = attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 && fd.dwReserved0 & 0x2000_0000 != 0;
                let ft = |t: windows_sys::Win32::Foundation::FILETIME| filetime(((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64);
                out.push(RawEntry {
                    path: dir.join(std::ffi::OsString::from_wide(name)),
                    is_dir: attrs & FILE_ATTRIBUTE_DIRECTORY != 0,
                    is_link,
                    len: ((fd.nFileSizeHigh as u64) << 32) | fd.nFileSizeLow as u64,
                    created: ft(fd.ftCreationTime),
                    modified: ft(fd.ftLastWriteTime),
                });
            }
            if FindNextFileW(h, &mut fd) == 0 {
                break;
            }
        }
        FindClose(h);
    }
    out
}

#[cfg(windows)]
fn filetime(t: u64) -> Option<SystemTime> {
    use std::time::{Duration, UNIX_EPOCH};
    // 100 ns ticks since 1601-01-01.
    const EPOCH_DIFF_SECS: u64 = 11_644_473_600;
    let secs = t / 10_000_000;
    if t == 0 || secs < EPOCH_DIFF_SECS {
        return None;
    }
    Some(UNIX_EPOCH + Duration::from_secs(secs - EPOCH_DIFF_SECS) + Duration::from_nanos((t % 10_000_000) * 100))
}

#[cfg(not(windows))]
pub fn read_dir_fast(dir: &Path) -> Vec<RawEntry> {
    read_dir_std(dir)
}

fn read_dir_std(dir: &Path) -> Vec<RawEntry> {
    let Ok(rd) = fs::read_dir(dir) else { return Vec::new() };
    rd.flatten()
        .filter_map(|e| {
            let ft = e.file_type().ok()?;
            let m = e.metadata().ok();
            Some(RawEntry {
                path: e.path(),
                is_dir: ft.is_dir(),
                is_link: ft.is_symlink(),
                len: if ft.is_file() { m.as_ref().map(|m| m.len()).unwrap_or(0) } else { 0 },
                created: m.as_ref().and_then(fetched_time),
                modified: m.as_ref().and_then(|m| m.modified().ok()),
            })
        })
        .collect()
}

fn file_stats(len: u64, path: &Path, modified: Option<SystemTime>, links: bool) -> Stats {
    Stats { bytes: len, files: 1, reclaimable: if !links || link_count(path) <= 1 { len } else { 0 }, newest_mtime: modified }
}

/// Serial recursive walk using the fast lister. Symlinks/junctions are not
/// followed. When `links` is true, each file's hard-link count is read too.
pub fn dir_stats(path: &Path, links: bool) -> Stats {
    let mut s = Stats::default();
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_file() => return file_stats(m.len(), path, m.modified().ok(), links),
        Ok(m) if m.is_dir() => {}
        _ => return s,
    }
    for e in read_dir_fast(path) {
        if e.is_link {
            continue;
        }
        if e.is_dir {
            s.add(dir_stats(&e.path, links));
        } else {
            s.add(file_stats(e.len, &e.path, e.modified, links));
        }
    }
    s
}

/// Parallel walk for large trees: fans out over sub-directories.
pub fn dir_stats_par(path: &Path, links: bool) -> Stats {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => {}
        _ => return dir_stats(path, links),
    }
    read_dir_fast(path)
        .into_par_iter()
        .map(|e| {
            if e.is_link {
                Stats::default()
            } else if e.is_dir {
                dir_stats_par(&e.path, links)
            } else {
                file_stats(e.len, &e.path, e.modified, links)
            }
        })
        .reduce(Stats::default, |mut a, b| {
            a.add(b);
            a
        })
}

/// Hard-link count of a file (1 when unknown). Uses `GetFileInformationByName`
/// (Windows 11 24H2+, no handle needed) when available.
#[cfg(windows)]
pub fn link_count(path: &Path) -> u32 {
    if let Some(n) = links_by_name(path) {
        return n;
    }
    link_count_handle(path)
}

#[cfg(windows)]
fn links_by_name(path: &Path) -> Option<u32> {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::sync::OnceLock;
    type ByName = unsafe extern "system" fn(*const u16, i32, *mut c_void, u32) -> i32;
    #[link(name = "kernel32")]
    extern "system" {
        fn GetModuleHandleW(name: *const u16) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
    }
    static F: OnceLock<Option<ByName>> = OnceLock::new();
    let f = (*F.get_or_init(|| unsafe {
        let k32: Vec<u16> = "kernel32.dll".encode_utf16().chain(Some(0)).collect();
        let m = GetModuleHandleW(k32.as_ptr());
        if m.is_null() {
            return None;
        }
        let f = GetProcAddress(m, c"GetFileInformationByName".as_ptr() as *const u8);
        (!f.is_null()).then(|| std::mem::transmute::<*mut c_void, ByName>(f))
    }))?;
    const FILE_STAT_BASIC_BY_NAME_INFO: i32 = 3;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // FILE_STAT_BASIC_INFORMATION is 104 bytes; NumberOfLinks is the u32 at offset 64.
    let mut buf = [0u64; 13];
    let ok = unsafe { f(wide.as_ptr(), FILE_STAT_BASIC_BY_NAME_INFO, buf.as_mut_ptr().cast(), 104) };
    (ok != 0).then(|| ((buf[8] & 0xffff_ffff) as u32).max(1))
}

#[cfg(windows)]
fn link_count_handle(path: &Path) -> u32 {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        let h = CreateFileW(
            wide.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        );
        if h == INVALID_HANDLE_VALUE {
            return 1;
        }
        let mut info: BY_HANDLE_FILE_INFORMATION = std::mem::zeroed();
        let ok = GetFileInformationByHandle(h, &mut info);
        CloseHandle(h);
        if ok == 0 {
            1
        } else {
            info.nNumberOfLinks.max(1)
        }
    }
}

#[cfg(not(windows))]
pub fn link_count(path: &Path) -> u32 {
    use std::os::unix::fs::MetadataExt;
    fs::symlink_metadata(path).map(|m| m.nlink() as u32).unwrap_or(1)
}

/// Sub-directories of `path` (no symlinks).
pub fn subdirs(path: &Path) -> Vec<PathBuf> {
    let Ok(rd) = fs::read_dir(path) else { return Vec::new() };
    rd.flatten().filter(|e| e.file_type().map(|t| t.is_dir() && !t.is_symlink()).unwrap_or(false)).map(|e| e.path()).collect()
}

/// Find an executable on PATH, honoring PATHEXT. No process is spawned.
pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".into())
            .split(';')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_ascii_lowercase())
            .collect()
    } else {
        vec![String::new()]
    };
    for dir in std::env::split_paths(&path) {
        for ext in &exts {
            let p = dir.join(format!("{name}{ext}"));
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

pub fn home() -> PathBuf {
    std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

pub fn local_app_data() -> PathBuf {
    std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|| home().join("AppData").join("Local"))
}

pub fn app_data() -> PathBuf {
    std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| home().join("AppData").join("Roaming"))
}

/// Env var as a path, if set and non-empty.
pub fn env_path(var: &str) -> Option<PathBuf> {
    std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// Keep only paths that exist.
pub fn existing(paths: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for p in paths {
        if p.exists() && !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// Delete a file or directory tree, clearing read-only attributes when needed
/// (Go's module cache is read-only). Used only by `clean::execute`.
pub fn remove_force(path: &Path) -> std::io::Result<()> {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if meta.file_type().is_symlink() {
        // Junctions and directory symlinks need remove_dir; the target is untouched.
        return fs::remove_file(path).or_else(|_| fs::remove_dir(path));
    }
    if meta.is_dir() {
        match fs::remove_dir_all(path) {
            Ok(()) => Ok(()),
            Err(_) => {
                clear_readonly(path);
                fs::remove_dir_all(path)
            }
        }
    } else {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(_) => {
                clear_readonly(path);
                fs::remove_file(path)
            }
        }
    }
}

fn clear_readonly(path: &Path) {
    if let Ok(m) = fs::symlink_metadata(path) {
        let mut perm = m.permissions();
        if perm.readonly() {
            #[allow(clippy::permissions_set_readonly_false)]
            perm.set_readonly(false);
            let _ = fs::set_permissions(path, perm);
        }
        if m.is_dir() {
            if let Ok(rd) = fs::read_dir(path) {
                for e in rd.flatten() {
                    clear_readonly(&e.path());
                }
            }
        }
    }
}

pub fn human_bytes(b: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else if v >= 100.0 {
        format!("{v:.0} {}", U[i])
    } else {
        format!("{v:.1} {}", U[i])
    }
}
