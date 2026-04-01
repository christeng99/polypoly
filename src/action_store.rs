use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::poly_history::ENV_HISTORY_DIR;

const DEFAULT_HISTORY_DIR: &str = "./data/poly_history/";

pub struct ActionStore {
    conn: Mutex<Connection>,
}

impl ActionStore {
    pub fn from_env() -> Result<Self> {
        let root = std::env::var(ENV_HISTORY_DIR).unwrap_or_else(|_| DEFAULT_HISTORY_DIR.to_string());
        let root = PathBuf::from(root.trim());
        std::fs::create_dir_all(&root).context("create history dir for sqlite")?;
        let db_path = root.join("actions.db");
        let conn = Connection::open(db_path).context("open sqlite actions.db")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS bot_actions (
                coin TEXT NOT NULL,
                round_slug TEXT NOT NULL,
                amount REAL NOT NULL,
                usdt REAL NOT NULL,
                action TEXT NOT NULL,
                msecs_from_round_start INTEGER NOT NULL,
                timestamp INTEGER NOT NULL
            );",
        )
        .context("create bot_actions table")?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn log_action(
        &self,
        coin: &str,
        round_slug: &str,
        amount: f64,
        usdt: f64,
        action: &str,
    ) -> Result<()> {
        let ts_ms = now_ms();
        let round_start_ms = round_start_ms_from_slug(round_slug).unwrap_or(ts_ms);
        let msecs_from_round_start = ts_ms.saturating_sub(round_start_ms);

        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute(
            "INSERT INTO bot_actions (coin, round_slug, amount, usdt, action, msecs_from_round_start, timestamp)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                coin,
                round_slug,
                amount,
                usdt,
                action,
                msecs_from_round_start as i64,
                ts_ms as i64
            ],
        )
        .context("insert bot action")?;
        Ok(())
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn round_start_ms_from_slug(slug: &str) -> Option<u64> {
    let (_, tail) = slug.rsplit_once('-')?;
    let secs: u64 = tail.parse().ok()?;
    Some(secs.saturating_mul(1000))
}
