//! RSS history: every feed item ZenTorrent has seen, kept after it scrolls
//! out of the feed.
//!
//! A feed only ever shows its newest few dozen items. With history on, each
//! read is merged in here, so search reaches back to the day history was
//! switched on. Swarm numbers are tracked over time, which is what makes
//! "activity" (grabs per hour since first seen) measurable at all.
//!
//! Stored at `<data dir>/zentorrent/history.json`, mode 0600 on Unix:
//! private-tracker download links carry the passkey, the same credential
//! feeds.json protects. Feeds are identified by a fingerprint of their URL,
//! never by the URL itself.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::rss;

/// Default number of items kept; the oldest (by last seen) go first.
pub const DEFAULT_CAP: usize = 20_000;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Entry {
    pub title: String,
    pub link: String,
    /// Feed name when the item was last seen.
    pub feed: String,
    /// [`feed_key`] of the feed's URL.
    pub feed_key: String,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub date: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub freeleech: bool,
    #[serde(default)]
    pub infohash: String,
    /// Latest numbers the feed reported.
    #[serde(default)]
    pub seeders: Option<u32>,
    #[serde(default)]
    pub leechers: Option<u32>,
    #[serde(default)]
    pub grabs: Option<u32>,
    /// Highest seeder count ever seen.
    #[serde(default)]
    pub max_seeders: u32,
    /// Grabs when first seen: the base for activity.
    #[serde(default)]
    pub first_grabs: Option<u32>,
    /// Unix seconds.
    pub first_seen: u64,
    pub last_seen: u64,
    /// In the feed's latest read (not saved: true again after the next read).
    #[serde(skip)]
    pub in_feed: bool,
}

impl Entry {
    fn from_item(it: &rss::Item, feed: &str, key: &str, now: u64) -> Self {
        let mut e = Entry {
            link: it.link.clone(),
            feed_key: key.to_string(),
            first_seen: now,
            first_grabs: it.grabs,
            ..Default::default()
        };
        e.update(it, feed, now);
        e
    }

    /// Fold in a fresh sighting of the same item.
    fn update(&mut self, it: &rss::Item, feed: &str, now: u64) {
        self.title = it.title.clone();
        self.feed = feed.to_string();
        self.size = it.size.or(self.size);
        if !it.date.is_empty() {
            self.date = it.date.clone();
        }
        if !it.category.is_empty() {
            self.category = it.category.clone();
        }
        if !it.tags.is_empty() {
            self.tags = it.tags.clone();
        }
        if !it.description.is_empty() {
            self.description = it.description.clone();
        }
        self.freeleech = it.freeleech;
        if !it.infohash.is_empty() {
            self.infohash = it.infohash.clone();
        }
        self.seeders = it.seeders.or(self.seeders);
        self.leechers = it.leechers.or(self.leechers);
        if it.grabs.is_some() {
            self.grabs = it.grabs;
            self.first_grabs = self.first_grabs.or(it.grabs);
        }
        self.max_seeders = self.max_seeders.max(it.seeders.unwrap_or(0));
        self.last_seen = now;
        self.in_feed = true;
    }

    /// Grabs per hour since first seen. `None` without grab numbers or
    /// before an hour has passed (one sighting says nothing about speed).
    pub fn activity(&self, now: u64) -> Option<f64> {
        let (first, last) = (self.first_grabs?, self.grabs?);
        let hours = now.saturating_sub(self.first_seen) as f64 / 3600.0;
        (hours >= 1.0).then(|| last.saturating_sub(first) as f64 / hours)
    }
}

/// A stable fingerprint of a feed URL (FNV-1a 64). The URL holds the
/// passkey, so history stores this instead.
pub fn feed_key(url: &str) -> String {
    let h = url.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3));
    format!("{h:016x}")
}

#[derive(Default, Serialize, Deserialize)]
pub struct History {
    /// Remember items after they leave the feed (the checkbox).
    #[serde(default = "yes")]
    pub enabled: bool,
    /// 0 = [`DEFAULT_CAP`].
    #[serde(default)]
    pub cap: usize,
    #[serde(default)]
    pub entries: Vec<Entry>,
    /// link → index into `entries`.
    #[serde(skip)]
    index: HashMap<String, usize>,
    /// Bumped on every change, so search knows when to run again.
    #[serde(skip)]
    pub version: u64,
    /// Changed since the last save.
    #[serde(skip)]
    pub dirty: bool,
}

fn yes() -> bool {
    true
}

/// "just now", "12 min", "5 h", "3 days".
pub fn ago(secs: u64) -> String {
    match secs {
        0..=59 => "under a minute".into(),
        60..=3599 => format!("{} min", secs / 60),
        3600..=86_399 => format!("{} h", secs / 3600),
        86_400..=172_799 => "1 day".into(),
        _ => format!("{} days", secs / 86_400),
    }
}

pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl History {
    fn path() -> PathBuf {
        crate::seed::data_dir().join("history.json")
    }

    pub fn load() -> Self {
        let mut h: History = std::fs::read(Self::path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_else(|| History { enabled: true, ..Default::default() });
        h.reindex();
        h
    }

    pub fn save(&mut self) -> anyhow::Result<()> {
        let p = Self::path();
        std::fs::create_dir_all(p.parent().unwrap())?;
        let tmp = p.with_extension("json.tmp");
        // Compact: tens of thousands of items; nobody reads this by hand.
        std::fs::write(&tmp, serde_json::to_vec(self)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(tmp, p)?;
        self.dirty = false;
        Ok(())
    }

    fn reindex(&mut self) {
        self.index = self.entries.iter().enumerate().map(|(i, e)| (e.link.clone(), i)).collect();
    }

    pub fn cap(&self) -> usize {
        if self.cap == 0 { DEFAULT_CAP } else { self.cap }
    }

    /// Merge one read of a feed. Items of this feed not in the read stop
    /// being "in the feed"; with history off they are dropped instead.
    pub fn absorb(&mut self, feed_name: &str, feed_url: &str, items: &[rss::Item], now: u64) {
        let key = feed_key(feed_url);
        for e in self.entries.iter_mut().filter(|e| e.feed_key == key) {
            e.in_feed = false;
        }
        for it in items {
            match self.index.get(&it.link) {
                Some(&i) => self.entries[i].update(it, feed_name, now),
                None => {
                    self.index.insert(it.link.clone(), self.entries.len());
                    self.entries.push(Entry::from_item(it, feed_name, &key, now));
                }
            }
        }
        if !self.enabled {
            self.entries.retain(|e| e.in_feed || e.feed_key != key);
        }
        self.prune();
        self.reindex();
        self.version += 1;
        self.dirty = self.enabled;
    }

    /// Over the cap: drop the items seen longest ago, never ones still in a feed.
    fn prune(&mut self) {
        let cap = self.cap();
        if self.entries.len() <= cap {
            return;
        }
        let mut old: Vec<(u64, usize)> =
            self.entries.iter().enumerate().filter(|(_, e)| !e.in_feed).map(|(i, e)| (e.last_seen, i)).collect();
        old.sort_unstable();
        let drop: std::collections::HashSet<usize> = old.into_iter().take(self.entries.len() - cap).map(|(_, i)| i).collect();
        let mut i = 0;
        self.entries.retain(|_| {
            let keep = !drop.contains(&i);
            i += 1;
            keep
        });
    }

    /// Turning history off forgets everything not currently in a feed.
    pub fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
        if !on {
            self.entries.retain(|e| e.in_feed);
            self.reindex();
            self.version += 1;
        }
        self.dirty = true;
    }

    pub fn clear(&mut self) {
        self.entries.retain(|e| e.in_feed);
        self.reindex();
        self.version += 1;
        self.dirty = true;
    }

    /// Oldest first-seen date, for "remembering since …".
    pub fn since(&self) -> Option<u64> {
        self.entries.iter().map(|e| e.first_seen).min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn it(title: &str, link: &str, seeders: u32, grabs: u32) -> rss::Item {
        rss::Item { title: title.into(), link: link.into(), seeders: Some(seeders), grabs: Some(grabs), ..Default::default() }
    }

    #[test]
    fn remembers_items_after_they_leave_the_feed() {
        let mut h = History { enabled: true, ..Default::default() };
        h.absorb("TL", "https://t/rss?passkey=SECRET", &[it("A", "a", 5, 10), it("B", "b", 1, 0)], 1_000);
        h.absorb("TL", "https://t/rss?passkey=SECRET", &[it("C", "c", 9, 0)], 2_000);
        assert_eq!(h.entries.len(), 3, "A and B stay in history");
        assert_eq!(h.entries.iter().filter(|e| e.in_feed).count(), 1);
        assert!(h.entries.iter().all(|e| !e.feed_key.contains("SECRET")) && h.entries[0].feed_key.len() == 16);
        assert!(h.dirty);
    }

    #[test]
    fn a_resighting_updates_numbers_and_keeps_first_seen() {
        let mut h = History { enabled: true, ..Default::default() };
        h.absorb("F", "u", &[it("A", "a", 5, 100)], 0);
        h.absorb("F", "u", &[it("A", "a", 20, 160)], 7_200);
        h.absorb("F", "u", &[it("A", "a", 8, 200)], 3 * 3_600);
        let e = &h.entries[0];
        assert_eq!((e.first_seen, e.last_seen, e.seeders, e.max_seeders), (0, 10_800, Some(8), 20));
        // 100 more grabs in 3 hours.
        assert!((e.activity(10_800).unwrap() - 33.333).abs() < 0.01);
        assert_eq!(h.entries.len(), 1);
    }

    #[test]
    fn activity_needs_an_hour_and_grab_numbers() {
        let e = Entry { first_grabs: Some(1), grabs: Some(50), first_seen: 0, ..Default::default() };
        assert!(e.activity(1_800).is_none(), "half an hour says nothing yet");
        assert!(Entry { grabs: None, ..e.clone() }.activity(9_999).is_none());
    }

    #[test]
    fn history_off_keeps_only_whats_in_the_feed() {
        let mut h = History { enabled: false, ..Default::default() };
        h.absorb("F", "u", &[it("A", "a", 1, 0)], 0);
        h.absorb("F", "u", &[it("B", "b", 1, 0)], 10);
        assert_eq!(h.entries.iter().map(|e| e.title.as_str()).collect::<Vec<_>>(), ["B"]);
        assert!(!h.dirty, "nothing to save with history off");
        // Another feed's items are untouched by this feed's read.
        h.absorb("G", "v", &[it("X", "x", 1, 0)], 20);
        h.absorb("F", "u", &[it("B", "b", 1, 0)], 30);
        assert_eq!(h.entries.len(), 2);
    }

    #[test]
    fn cap_drops_the_oldest_but_never_live_items() {
        let mut h = History { enabled: true, cap: 3, ..Default::default() };
        for n in 0..5u64 {
            h.absorb("F", "u", &[it(&format!("T{n}"), &format!("l{n}"), 1, 0)], n * 10);
        }
        let titles: Vec<_> = h.entries.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(titles, ["T2", "T3", "T4"]);
        // The index still points at the right items after pruning.
        h.absorb("F", "u", &[it("T2", "l2", 99, 0)], 100);
        assert_eq!(h.entries.iter().find(|e| e.link == "l2").unwrap().seeders, Some(99));
        assert_eq!(h.entries.len(), 3);
    }

    #[test]
    fn saved_history_round_trips_and_clear_keeps_live() {
        let mut h = History { enabled: true, ..Default::default() };
        h.absorb("F", "u", &[it("A", "a", 3, 0)], 5);
        h.absorb("F", "u", &[it("B", "b", 3, 0)], 6);
        let mut back: History = serde_json::from_slice(&serde_json::to_vec(&h).unwrap()).unwrap();
        back.reindex();
        assert_eq!(back.entries.len(), 2);
        assert!(back.entries.iter().all(|e| !e.in_feed), "in_feed is not saved");
        h.clear();
        assert_eq!(h.entries.iter().map(|e| e.title.as_str()).collect::<Vec<_>>(), ["B"]);
    }
}
