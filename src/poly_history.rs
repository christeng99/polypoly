//! Buffered price history JSON: `{coin}_{up|down}.json` under `POLY_HISTORY_DIR` (default `./data/poly_history/`).
//!
//! Ticks are accumulated in memory and flushed to disk every [`FLUSH_INTERVAL_SECS`] and when a round ends
//! (see [`PolyHistory::flush_to_disk`]).

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub const ENV_HISTORY_DIR: &str = "POLY_HISTORY_DIR";
const DEFAULT_DIR: &str = "./data/poly_history/";

/// Background flush interval (seconds).
pub const FLUSH_INTERVAL_SECS: u64 = 10;

/// Outer object is slug → list of ticks: `{ "slug": [{ "buy", "sell", "time" }, ...], ... }`.
pub type SlugFile = BTreeMap<String, Vec<PriceTick>>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceTick {
    pub buy: f64,
    pub sell: f64,
    /// Milliseconds since this market window started (from slug trailing unix seconds).
    pub time: u64,
}

struct Inner {
    root: PathBuf,
    /// Merged on-disk + pending content per file path.
    files: HashMap<PathBuf, SlugFile>,
    /// Paths that differ from last successful write.
    dirty: HashSet<PathBuf>,
}

pub struct PolyHistory {
    inner: Mutex<Inner>,
}

impl PolyHistory {
    pub fn from_env() -> Self {
        let raw = std::env::var(ENV_HISTORY_DIR).unwrap_or_else(|_| DEFAULT_DIR.to_string());
        let root = PathBuf::from(raw.trim());
        if let Err(e) = fs::create_dir_all(&root) {
            log::error!("create_dir_all({root:?}): {e}");
        }
        Self {
            inner: Mutex::new(Inner {
                root,
                files: HashMap::new(),
                dirty: HashSet::new(),
            }),
        }
    }

    /// `outcome_label` is `"Up"` / `"Down"` (or lowercase); filename uses lowercase `up` / `down`.
    /// `buy` = best ask, `sell` = best bid. Does not hit disk until [`flush_to_disk`](Self::flush_to_disk).
    pub fn record_price_change(
        &self,
        coin: &str,
        outcome_label: &str,
        slug: &str,
        best_bid: f64,
        best_ask: f64,
    ) {
        let coin_key = coin.trim().to_ascii_lowercase();
        if coin_key.is_empty() {
            return;
        }
        let side = outcome_label.trim().to_ascii_lowercase();
        if side.is_empty() {
            return;
        }

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        let round_start_ms = slug_window_start_ms(slug).unwrap_or(0);
        let elapsed = now_ms.saturating_sub(round_start_ms);

        let tick = PriceTick {
            buy: best_ask,
            sell: best_bid,
            time: elapsed,
        };

        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let path = g.root.join(format!("{coin_key}_{side}.json"));

        if !g.files.contains_key(&path) {
            let doc: SlugFile = match fs::read_to_string(&path) {
                Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
                Err(_) => SlugFile::new(),
            };
            g.files.insert(path.clone(), doc);
        }

        let doc = g.files.get_mut(&path).expect("just inserted");
        doc.entry(slug.to_string()).or_default().push(tick);
        g.dirty.insert(path);
    }

    /// Writes all dirty files. Safe to call from any thread; performs blocking I/O.
    pub fn flush_to_disk(&self) {
        let to_write: Vec<(PathBuf, SlugFile)> = {
            let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if g.dirty.is_empty() {
                return;
            }
            g.dirty
                .iter()
                .filter_map(|p| g.files.get(p).map(|doc| (p.clone(), doc.clone())))
                .collect()
        };

        let mut ok_paths = Vec::new();
        for (path, doc) in to_write {
            let payload = match serde_json::to_string_pretty(&doc) {
                Ok(p) => p,
                Err(e) => {
                    log::error!("history serialize {:?}: {e}", path);
                    continue;
                }
            };
            match fs::write(&path, payload) {
                Ok(()) => ok_paths.push(path),
                Err(e) => log::error!("history write {:?}: {e}", path),
            }
        }

        if ok_paths.is_empty() {
            return;
        }
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        for p in ok_paths {
            g.dirty.remove(&p);
        }
    }
}

/// Parses trailing `-{unix_secs}` from slugs like `btc-updown-5m-1739123456`.
fn slug_window_start_ms(slug: &str) -> Option<u64> {
    let tail = slug.rsplit('-').next()?;
    let secs: u64 = tail.parse().ok()?;
    Some(secs.saturating_mul(1000))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_start_ms() {
        assert_eq!(
            slug_window_start_ms("btc-updown-5m-1700000100"),
            Some(1_700_000_100_000)
        );
        assert_eq!(slug_window_start_ms("no-number"), None);
    }
}
