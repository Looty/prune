use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IntervalUnit {
    Minutes,
    Hours,
    Days,
}

impl IntervalUnit {
    pub const ALL: [IntervalUnit; 3] = [IntervalUnit::Minutes, IntervalUnit::Hours, IntervalUnit::Days];
    pub fn label(self) -> &'static str {
        match self {
            IntervalUnit::Minutes => "minutes",
            IntervalUnit::Hours => "hours",
            IntervalUnit::Days => "days",
        }
    }
    pub fn secs(self) -> u64 {
        match self {
            IntervalUnit::Minutes => 60,
            IntervalUnit::Hours => 3600,
            IntervalUnit::Days => 86400,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

impl Theme {
    pub const ALL: [Theme; 3] = [Theme::System, Theme::Light, Theme::Dark];
    pub fn label(self) -> &'static str {
        match self {
            Theme::System => "Use system setting",
            Theme::Light => "Light",
            Theme::Dark => "Dark",
        }
    }
}

/// An auto-clean rule: a whole provider (`package == None`) or one package.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Rule {
    pub provider: String,
    pub package: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub auto_clean: bool,
    pub interval_value: u32,
    pub interval_unit: IntervalUnit,
    /// Only auto-clean packages not fetched within this many days.
    pub min_age_days: u32,
    pub rules: Vec<Rule>,
    pub start_with_windows: bool,
    /// Keep the tray icon running after the window closes.
    pub run_in_tray: bool,
    pub theme: Theme,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            auto_clean: false,
            interval_value: 1,
            interval_unit: IntervalUnit::Days,
            min_age_days: 30,
            rules: Vec::new(),
            start_with_windows: false,
            run_in_tray: true,
            theme: Theme::System,
        }
    }
}

impl Settings {
    pub fn interval_secs(&self) -> u64 {
        (self.interval_value.max(1) as u64) * self.interval_unit.secs()
    }
    pub fn has_rule(&self, provider: &str, package: Option<&str>) -> bool {
        self.rules.iter().any(|r| r.provider == provider && r.package.as_deref() == package)
    }
    pub fn set_rule(&mut self, provider: &str, package: Option<&str>, on: bool) {
        self.rules.retain(|r| !(r.provider == provider && r.package.as_deref() == package));
        if on {
            self.rules.push(Rule { provider: provider.into(), package: package.map(Into::into) });
        }
    }
    /// Whether a package is covered by a rule (its own or its provider's).
    pub fn covers(&self, provider: &str, package: &str) -> bool {
        self.has_rule(provider, None) || self.has_rule(provider, Some(package))
    }

    pub fn load() -> Self {
        std::fs::read_to_string(settings_path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }
    pub fn save(&self) -> std::io::Result<()> {
        write_atomic(&settings_path(), &serde_json::to_vec_pretty(self).unwrap_or_default())
    }
}

/// Runtime state written by the tray (kept apart from settings to avoid races).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// Unix seconds of the last auto-clean run.
    pub last_auto_run: Option<u64>,
    pub last_freed: u64,
    pub total_freed: u64,
    pub last_error: Option<String>,
    /// Freeable bytes from the window's last scan (for the tray tooltip).
    pub last_reclaimable: Option<u64>,
}

impl State {
    pub fn load() -> Self {
        std::fs::read_to_string(data_dir().join("state.json")).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }
    pub fn save(&self) -> std::io::Result<()> {
        write_atomic(&data_dir().join("state.json"), &serde_json::to_vec_pretty(self).unwrap_or_default())
    }
}

/// %APPDATA%\Prune (override with PRUNE_HOME). Data from before the rename
/// (%APPDATA%\DevSweep) is moved over the first time.
pub fn data_dir() -> PathBuf {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        if let Some(p) = crate::fsutil::env_path("PRUNE_HOME").or_else(|| crate::fsutil::env_path("DEVSWEEP_HOME")) {
            return p;
        }
        let dir = crate::fsutil::app_data().join("Prune");
        let legacy = crate::fsutil::app_data().join("DevSweep");
        if !dir.exists() && legacy.is_dir() && std::fs::rename(&legacy, &dir).is_err() {
            return legacy; // couldn't move it (in use?): keep using it as is
        }
        dir
    })
    .clone()
}

pub fn settings_path() -> PathBuf {
    data_dir().join("settings.json")
}

/// Append a line to history.log.
pub fn log_history(line: &str) {
    use std::io::Write;
    let dir = data_dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("history.log")) {
        let _ = writeln!(f, "{} {}", crate::timefmt::now_iso(), line);
    }
}

fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}
