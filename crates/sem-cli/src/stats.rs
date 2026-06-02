use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use sem_core::parser::differ::DiffResult;

#[derive(Serialize, Deserialize, Default)]
pub struct SemLifetimeStats {
    pub version: u32,
    pub first_run: Option<String>,
    pub last_run: Option<String>,
    pub total_diffs: u64,
    pub total_files_analyzed: u64,
    pub total_entities_analyzed: u64,
    pub total_changes_detected: u64,
    pub noise_filtered: u64,
    pub added_count: u64,
    pub modified_count: u64,
    pub deleted_count: u64,
    pub moved_count: u64,
    pub renamed_count: u64,
    pub reordered_count: u64,
}

fn stats_path() -> Option<PathBuf> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()?;
    Some(PathBuf::from(home).join(".sem").join("stats.json"))
}

fn now_iso() -> String {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    let time_of_day = secs % 86400;
    let h = time_of_day / 3600;
    let m = (time_of_day % 3600) / 60;
    let s = time_of_day % 60;

    let mut y = 1970i64;
    let mut remaining = (secs / 86400) as i64;
    loop {
        let days_in_year = if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
            366
        } else {
            365
        };
        if remaining < days_in_year {
            break;
        }
        remaining -= days_in_year;
        y += 1;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let month_days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut mon = 0;
    for (i, &md) in month_days.iter().enumerate() {
        if remaining < md as i64 {
            mon = i;
            break;
        }
        remaining -= md as i64;
    }
    let day = remaining + 1;
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y,
        mon + 1,
        day,
        h,
        m,
        s
    )
}

impl SemLifetimeStats {
    pub fn load() -> Self {
        let path = match stats_path() {
            Some(p) => p,
            None => return Self::default(),
        };
        fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn record_diff(mut self, result: &DiffResult, binary_count: usize) -> Self {
        let now = now_iso();
        if self.first_run.is_none() {
            self.first_run = Some(now.clone());
        }
        self.last_run = Some(now);
        self.version = 1;

        self.total_diffs += 1;
        self.total_files_analyzed += result.file_count as u64;

        let entities_analyzed = result
            .total_entities_before
            .max(result.total_entities_after) as u64;
        self.total_entities_analyzed += entities_analyzed;

        let changes = (result.added_count
            + result.modified_count
            + result.deleted_count
            + result.moved_count
            + result.renamed_count
            + result.reordered_count
            + binary_count) as u64;
        self.total_changes_detected += changes;
        self.noise_filtered += entities_analyzed.saturating_sub(changes);

        self.added_count += result.added_count as u64;
        self.modified_count += result.modified_count as u64;
        self.deleted_count += result.deleted_count as u64;
        self.moved_count += result.moved_count as u64;
        self.renamed_count += result.renamed_count as u64;
        self.reordered_count += result.reordered_count as u64;

        self
    }

    pub fn save(self) -> Self {
        if let Some(path) = stats_path() {
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            if let Ok(json) = serde_json::to_string_pretty(&self) {
                let _ = fs::write(&path, json);
            }
        }
        self
    }
}
