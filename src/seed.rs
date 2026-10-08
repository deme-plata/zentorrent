//! Seeding & ratio ledger.
//!
//! librqbit counts uploaded bytes per torrent for the current run only; the
//! counter starts again at 0 after a restart. Private trackers judge you on
//! your lifetime ratio, so ZenTorrent keeps its own ledger per info-hash in
//! `<data dir>/zentorrent/ratio.json`: total uploaded across every run,
//! seed time, and the folder the files live in.

use std::{collections::HashMap, path::PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Entry {
    pub name: String,
    pub folder: PathBuf,
    pub size: u64,
    /// Bytes uploaded over all runs.
    pub uploaded: u64,
    /// Seconds spent seeding (complete, not paused).
    pub seed_secs: u64,
    pub private: bool,
    /// Set once the seed goal paused this torrent; the goal never pauses it
    /// a second time, so a manual Resume sticks.
    #[serde(default)]
    pub goal_reached: bool,
    /// The user's labels on this torrent (sidebar → Labels).
    #[serde(default)]
    pub labels: Vec<String>,
    /// librqbit's per-run counter as last seen (not saved: a new run starts at 0).
    #[serde(skip)]
    pub seen: u64,
}

impl Entry {
    pub fn ratio(&self) -> f64 {
        if self.size == 0 { 0.0 } else { self.uploaded as f64 / self.size as f64 }
    }

    /// Fold in librqbit's per-run upload counter.
    pub fn observe_uploaded(&mut self, run_counter: u64) {
        if run_counter >= self.seen {
            self.uploaded += run_counter - self.seen;
        } else {
            // The counter restarted (torrent re-added in this run).
            self.uploaded += run_counter;
        }
        self.seen = run_counter;
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Ledger {
    /// Pause a finished torrent once its ratio reaches this. 0 = off.
    #[serde(default)]
    pub ratio_goal: f64,
    /// Pause a finished torrent after seeding this many hours. 0 = off.
    #[serde(default)]
    pub hours_goal: f64,
    #[serde(default)]
    pub entries: HashMap<String, Entry>,
    /// Every label the user made, in the order they made them (kept even
    /// when no torrent carries it yet).
    #[serde(default)]
    pub labels: Vec<String>,
}

impl Default for Ledger {
    fn default() -> Self {
        Self { ratio_goal: 0.0, hours_goal: 0.0, entries: HashMap::new(), labels: Vec::new() }
    }
}

pub fn data_dir() -> PathBuf {
    dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join("zentorrent")
}

impl Ledger {
    fn path() -> PathBuf {
        data_dir().join("ratio.json")
    }

    pub fn load() -> Self {
        std::fs::read(Self::path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let p = Self::path();
        std::fs::create_dir_all(p.parent().unwrap())?;
        let tmp = p.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(tmp, p)?;
        Ok(())
    }

    /// The goal settings without the entries (cheap copy for borrow-free checks).
    pub fn clone_goals(&self) -> Ledger {
        Ledger { ratio_goal: self.ratio_goal, hours_goal: self.hours_goal, ..Default::default() }
    }

    /// The ratio below which a finished private torrent "needs seeding":
    /// the user's ratio goal, or 1.0 when no goal is set.
    pub fn ratio_floor(&self) -> f64 {
        if self.ratio_goal > 0.0 { self.ratio_goal } else { 1.0 }
    }

    /// Tag or un-tag a torrent; tagging also registers the label.
    pub fn set_label(&mut self, hash: &str, label: &str, on: bool) {
        if on && !self.labels.iter().any(|l| l == label) {
            self.labels.push(label.to_string());
        }
        let e = self.entries.entry(hash.to_string()).or_default();
        if on {
            if !e.labels.iter().any(|l| l == label) {
                e.labels.push(label.to_string());
            }
        } else {
            e.labels.retain(|l| l != label);
        }
    }

    /// Delete a label everywhere.
    pub fn delete_label(&mut self, label: &str) {
        self.labels.retain(|l| l != label);
        for e in self.entries.values_mut() {
            e.labels.retain(|l| l != label);
        }
    }

    /// Has this finished torrent met the seed goal?
    pub fn goal_met(&self, e: &Entry) -> bool {
        (self.ratio_goal > 0.0 && e.ratio() >= self.ratio_goal)
            || (self.hours_goal > 0.0 && e.seed_secs as f64 >= self.hours_goal * 3600.0)
    }

    pub fn totals(&self) -> (u64, u64) {
        self.entries.values().fold((0, 0), |(u, s), e| (u + e.uploaded, s + e.size))
    }
}

pub fn duration(secs: u64) -> String {
    let (d, h, m) = (secs / 86400, secs / 3600 % 24, secs / 60 % 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_survives_restarts_and_readds() {
        let mut e = Entry { size: 1000, ..Default::default() };
        e.observe_uploaded(100);
        e.observe_uploaded(300);
        assert_eq!(e.uploaded, 300);
        // New run: counter starts again at 0 (seen is not persisted).
        e.seen = 0;
        e.observe_uploaded(50);
        assert_eq!(e.uploaded, 350);
        // Re-added in the same run: counter drops.
        e.observe_uploaded(20);
        assert_eq!(e.uploaded, 370);
        assert!((e.ratio() - 0.37).abs() < 1e-9);
    }

    #[test]
    fn goals_are_off_by_default_and_either_one_triggers() {
        let mut l = Ledger::default();
        let e = Entry { size: 100, uploaded: 150, seed_secs: 7200, ..Default::default() };
        assert!(!l.goal_met(&e));
        l.ratio_goal = 2.0;
        assert!(!l.goal_met(&e));
        l.hours_goal = 2.0;
        assert!(l.goal_met(&e));
        l.hours_goal = 0.0;
        l.ratio_goal = 1.5;
        assert!(l.goal_met(&e));
    }

    #[test]
    fn saved_ledger_round_trips_without_the_run_counter() {
        let mut l = Ledger { ratio_goal: 1.0, ..Default::default() };
        l.entries.insert("ab".into(), Entry { name: "x".into(), uploaded: 5, seen: 99, ..Default::default() });
        let back: Ledger = serde_json::from_slice(&serde_json::to_vec(&l).unwrap()).unwrap();
        assert_eq!(back.entries["ab"].uploaded, 5);
        assert_eq!(back.entries["ab"].seen, 0);
        assert_eq!(duration(90061), "1d 1h");
    }

    #[test]
    fn labels_round_trip_and_old_ledgers_still_load() {
        // A ratio.json written by 0.5.x has no labels anywhere.
        let old = br#"{"ratio_goal":0.0,"hours_goal":0.0,"entries":{"ab":{"name":"x","folder":"/d","size":10,"uploaded":1,"seed_secs":0,"private":true}}}"#;
        let mut l: Ledger = serde_json::from_slice(old).unwrap();
        assert!(l.labels.is_empty() && l.entries["ab"].labels.is_empty());
        l.set_label("ab", "film", true);
        l.set_label("ab", "film", true);
        assert_eq!(l.entries["ab"].labels, ["film"]);
        assert_eq!(l.labels, ["film"]);
        let back: Ledger = serde_json::from_slice(&serde_json::to_vec(&l).unwrap()).unwrap();
        assert_eq!(back.entries["ab"].labels, ["film"]);
        l.set_label("ab", "film", false);
        assert!(l.entries["ab"].labels.is_empty() && l.labels == ["film"], "un-tag keeps the label");
        l.set_label("ab", "film", true);
        l.delete_label("film");
        assert!(l.labels.is_empty() && l.entries["ab"].labels.is_empty());
        assert_eq!(l.ratio_floor(), 1.0);
    }
}
