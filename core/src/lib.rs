//! Prune core: detects dev package managers, scans their global caches and
//! plans deletions. No UI code lives here.

pub mod clean;
pub mod fsutil;
pub mod model;
pub mod providers;
pub mod report;
pub mod scan;
pub mod settings;
pub mod timefmt;

pub use model::*;
