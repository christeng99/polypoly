//! Buffered price history JSON: `{coin}_{up|down}.json` under `POLY_HISTORY_DIR` (default `./data/poly_history/`).
//!
//! Each file is `{ "slug": [mid_price, ...], ... }`. Mids are **rounded to 0.01** (cent); a value is
//! appended only when that rounded level **changes** from the last stored point. Flushes every [`FLUSH_INTERVAL_SECS`] and on round end.

use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

pub const ENV_HISTORY_DIR: &str = "POLY_HISTORY_DIR";
const DEFAULT_DIR: &str = "./data/poly_history/";

/// Background flush interval (seconds).
pub const FLUSH_INTERVAL_SECS: u64 = 10;

/// Mid prices are quantized to this step (e.g. 0.01 → hundredths) for compare + store.
const MID_TICK: f64 = 0.01;

/// `{ "slug": [mid, mid, ...], ... }`
pub type SlugFile = BTreeMap<String, Vec<f64>>;

struct Inner {
    root: PathBuf,
    files: HashMap<PathBuf, SlugFile>,
    dirty: HashSet<PathBuf>,
}

pub struct PolyHistory {
    inner: Mutex<Inner>,
}

impl PolyHistory {
    pub fn from_env() -> Self {
        let raw = std::env::var(ENV_HISTORY_DIR).unwrap_or_else(|_| DEFAULT_DIR.to_string());
        let root = PathBuf::from(raw.trim());
        if let Err(_e) = fs::create_dir_all(&root) {
            // log::error!("create_dir_all({root:?}): {e}");
        }
        Self {
            inner: Mutex::new(Inner {
                root,
                files: HashMap::new(),
                dirty: HashSet::new(),
            }),
        }
    }

    /// Appends `round(mid_raw, 0.01)` only if that cent level differs from the last stored point.
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

        let mid = mid_to_tick(mid_from_bid_ask(best_bid, best_ask));

        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let path = g.root.join(format!("{coin_key}_{side}.json"));

        if !g.files.contains_key(&path) {
            let doc = match fs::read_to_string(&path) {
                Ok(s) => parse_file_content(&s),
                Err(_) => SlugFile::new(),
            };
            g.files.insert(path.clone(), doc);
        }

        let doc = g.files.get_mut(&path).expect("just inserted");
        let series = doc.entry(slug.to_string()).or_default();
        if mid_changed(series.as_slice(), mid) {
            series.push(mid);
            g.dirty.insert(path);
        }
    }

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
                Err(_e) => {
                    // log::error!("history serialize {:?}: {e}", path);
                    continue;
                }
            };
            match fs::write(&path, payload) {
                Ok(()) => ok_paths.push(path),
                Err(_e) => { /* log::error!("history write {:?}: {e}", path) */ }
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

fn mid_changed(series: &[f64], mid: f64) -> bool {
    match series.last() {
        None => true,
        Some(prev) => (mid - *prev).abs() >= MID_TICK * 0.5,
    }
}

/// Round to nearest `MID_TICK` (e.g. 0.01).
fn mid_to_tick(mid: f64) -> f64 {
    if !mid.is_finite() {
        return 0.0;
    }
    (mid / MID_TICK).round() * MID_TICK
}

fn mid_from_bid_ask(best_bid: f64, best_ask: f64) -> f64 {
    if best_bid > 0.0 && best_ask < 1.0 {
        (best_bid + best_ask) / 2.0
    } else if best_bid > 0.0 {
        best_bid
    } else if best_ask < 1.0 {
        best_ask
    } else {
        0.5
    }
}

/// Loads numeric arrays; migrates legacy `{ "buy", "sell", "time" }` entries to mid = f(bid, ask).
fn parse_file_content(s: &str) -> SlugFile {
    let Ok(val) = serde_json::from_str::<Value>(s) else {
        return SlugFile::new();
    };
    let Some(obj) = val.as_object() else {
        return SlugFile::new();
    };
    let mut out = SlugFile::new();
    for (k, v) in obj {
        let Some(arr) = v.as_array() else {
            continue;
        };
        let mut mids = Vec::new();
        for item in arr {
            if let Some(n) = item.as_f64() {
                mids.push(mid_to_tick(n));
            } else if let Some(o) = item.as_object() {
                let buy = o.get("buy").and_then(Value::as_f64);
                let sell = o.get("sell").and_then(Value::as_f64);
                if let (Some(b), Some(s)) = (buy, sell) {
                    mids.push(mid_to_tick(mid_from_bid_ask(s, b)));
                }
            }
        }
        out.insert(k.clone(), mids);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mid_from_spread() {
        assert!((mid_from_bid_ask(0.48, 0.52) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn parse_legacy_then_numeric_roundtrip() {
        let legacy = r#"{"s":[{"buy":0.52,"sell":0.48,"time":0}]}"#;
        let m = parse_file_content(legacy);
        assert!((m["s"][0] - 0.5).abs() < 1e-9);
    }

    #[test]
    fn skip_duplicate_mid_in_row() {
        assert!(mid_changed(&[], 0.50));
        assert!(!mid_changed(&[0.50], 0.50));
        assert!(mid_changed(&[0.50], 0.51));
        assert_eq!(mid_to_tick(0.5012), 0.50);
        assert_eq!(mid_to_tick(0.505), 0.51);
    }
}
