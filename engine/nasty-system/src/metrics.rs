//! Time-series metrics backed by SQLite.
//!
//! Stores per-resource I/O rates (bytes/s) at 5-second intervals with 30-day retention.
//! For longer time ranges, samples are bucketed and averaged to keep response sizes small.

use std::sync::Mutex;

use rusqlite::Connection;
use serde::Serialize;
use tracing::{info, warn};

const DB_PATH: &str = "/var/lib/nasty/metrics.db";
const RETENTION_SECS: i64 = 30 * 24 * 3600; // 30 days

#[derive(Debug, Serialize)]
pub struct IoSample {
    /// Unix epoch milliseconds
    pub ts: i64,
    pub in_rate: f64,
    pub out_rate: f64,
}

pub struct MetricsDb {
    conn: Mutex<Connection>,
}

impl MetricsDb {
    pub fn open() -> Result<Self, String> {
        let conn = Connection::open(DB_PATH)
            .map_err(|e| format!("failed to open metrics db: {e}"))?;

        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             CREATE TABLE IF NOT EXISTS io_samples (
                 ts      INTEGER NOT NULL,
                 kind    TEXT NOT NULL,
                 name    TEXT NOT NULL,
                 in_rate REAL NOT NULL,
                 out_rate REAL NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_io_lookup
                 ON io_samples(kind, name, ts);",
        )
        .map_err(|e| format!("failed to initialize metrics schema: {e}"))?;

        info!("Metrics database opened at {DB_PATH}");
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Insert a batch of I/O rate samples.
    pub fn insert(&self, kind: &str, samples: &[(&str, f64, f64)]) {
        let conn = self.conn.lock().expect("metrics db mutex poisoned");
        let ts = now_ms();
        let mut stmt = match conn.prepare(
            "INSERT INTO io_samples (ts, kind, name, in_rate, out_rate) VALUES (?1, ?2, ?3, ?4, ?5)",
        ) {
            Ok(s) => s,
            Err(e) => {
                warn!("Failed to prepare metrics insert: {e}");
                return;
            }
        };
        for &(name, in_rate, out_rate) in samples {
            if let Err(e) = stmt.execute(rusqlite::params![ts, kind, name, in_rate, out_rate]) {
                warn!("Failed to insert metric for {kind}/{name}: {e}");
            }
        }
    }

    /// Prune samples older than the retention period.
    pub fn prune(&self) {
        let cutoff = now_ms() - RETENTION_SECS * 1000;
        let conn = self.conn.lock().expect("metrics db mutex poisoned");
        match conn.execute("DELETE FROM io_samples WHERE ts < ?1", [cutoff]) {
            Ok(n) if n > 0 => info!("Pruned {n} old metric samples"),
            Err(e) => warn!("Failed to prune metrics: {e}"),
            _ => {}
        }
    }

    /// Query history for a given kind and optional resource name.
    ///
    /// `range` is one of: "5m", "1h", "1d", "7d", "30d".
    /// For ranges longer than 5m, samples are bucketed and averaged
    /// to keep the response to ~360 points per series.
    pub fn query(
        &self,
        kind: &str,
        name: Option<&str>,
        range: &str,
    ) -> Vec<ResourceHistory> {
        let (duration_ms, bucket_ms) = range_to_params(range);
        let since = now_ms() - duration_ms;
        let conn = self.conn.lock().expect("metrics db mutex poisoned");

        // Collect distinct resource names in range
        let names: Vec<String> = if let Some(n) = name {
            vec![n.to_string()]
        } else {
            match conn.prepare("SELECT DISTINCT name FROM io_samples WHERE kind = ?1 AND ts >= ?2") {
                Ok(mut stmt) => stmt
                    .query_map(rusqlite::params![kind, since], |row| row.get(0))
                    .map(|rows| rows.filter_map(|r| r.ok()).collect())
                    .unwrap_or_default(),
                Err(e) => {
                    warn!("Failed to query metric names for {kind}: {e}");
                    return Vec::new();
                }
            }
        };

        let mut results = Vec::new();

        if bucket_ms == 0 {
            // Raw samples — no bucketing
            let mut stmt = match conn.prepare(
                "SELECT ts, in_rate, out_rate FROM io_samples
                 WHERE kind = ?1 AND name = ?2 AND ts >= ?3
                 ORDER BY ts",
            ) {
                Ok(s) => s,
                Err(e) => {
                    warn!("Failed to prepare raw metrics query: {e}");
                    return Vec::new();
                }
            };

            for n in &names {
                let samples: Vec<IoSample> = stmt
                    .query_map(rusqlite::params![kind, n, since], |row| {
                        Ok(IoSample {
                            ts: row.get(0)?,
                            in_rate: row.get(1)?,
                            out_rate: row.get(2)?,
                        })
                    })
                    .map(|rows| rows.filter_map(|r| r.ok()).collect())
                    .unwrap_or_default();

                results.push(ResourceHistory { name: n.clone(), samples });
            }
        } else {
            // Bucketed averages
            let mut stmt = match conn.prepare(
                "SELECT (ts / ?4) * ?4 AS bucket,
                        AVG(in_rate), AVG(out_rate)
                 FROM io_samples
                 WHERE kind = ?1 AND name = ?2 AND ts >= ?3
                 GROUP BY bucket
                 ORDER BY bucket",
            ) {
                Ok(s) => s,
                Err(e) => {
                    warn!("Failed to prepare bucketed metrics query: {e}");
                    return Vec::new();
                }
            };

            for n in &names {
                let samples: Vec<IoSample> = stmt
                    .query_map(rusqlite::params![kind, n, since, bucket_ms], |row| {
                        Ok(IoSample {
                            ts: row.get(0)?,
                            in_rate: row.get(1)?,
                            out_rate: row.get(2)?,
                        })
                    })
                    .map(|rows| rows.filter_map(|r| r.ok()).collect())
                    .unwrap_or_default();

                results.push(ResourceHistory { name: n.clone(), samples });
            }
        }

        results
    }
}

#[derive(Debug, Serialize)]
pub struct ResourceHistory {
    pub name: String,
    pub samples: Vec<IoSample>,
}

/// Returns (duration_ms, bucket_ms) for a range string.
/// bucket_ms == 0 means return raw samples.
fn range_to_params(range: &str) -> (i64, i64) {
    match range {
        "1h"  => (3_600_000,      60_000),   // 1-min buckets  → up to 60 points
        "1d"  => (86_400_000,    300_000),   // 5-min buckets  → up to 288 points
        "7d"  => (604_800_000, 1_800_000),   // 30-min buckets → up to 336 points
        "30d" => (2_592_000_000, 7_200_000), // 2-hr buckets   → up to 360 points
        _     => (300_000, 0),               // "5m" → raw
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock is before Unix epoch")
        .as_millis() as i64
}
