//! A cross-process lock per source, for `lore index`. The in-process [`super::SourceLocks`]
//! only keeps two pushes of one server apart; a Kubernetes Job is another process, so it takes a
//! Postgres session advisory lock keyed by the source id instead.
//!
//! The lock lives on its own connection, opened for the purpose and held for the whole run, so
//! it is never a pooled connection that goes back to the pool (and loses or keeps the lock by
//! accident). Postgres drops a session lock when its connection ends, so a killed process
//! releases it too. The server's own push and sync path does not take it. It is already kept apart
//! from a running index in the database: `index_commit` locks the source row (`FOR NO KEY
//! UPDATE`) before it writes anything and holds it until it commits, so a push that meets a
//! running index waits for it instead of writing at the same time. The advisory lock adds what
//! that cannot: a second `lore index` fails at once instead of embedding the repo twice.

use anyhow::{Context, Result};
use sqlx::postgres::PgConnection;
use sqlx::{Connection, Row};
use uuid::Uuid;

/// The advisory lock key of a source: the first 8 bytes of its (random, version 4) id.
pub fn advisory_key(source_id: Uuid) -> i64 {
    let bytes = source_id.as_bytes();
    let mut head = [0u8; 8];
    head.copy_from_slice(&bytes[..8]);
    i64::from_be_bytes(head)
}

/// The held advisory lock of one source. Call [`SourceAdvisoryLock::release`] when done; the
/// lock is dropped with the connection either way.
pub struct SourceAdvisoryLock {
    conn: PgConnection,
    key: i64,
}

impl SourceAdvisoryLock {
    /// Opens a dedicated connection to `database_url` and tries the lock without waiting.
    /// `Ok(None)` means another session holds it.
    pub async fn try_acquire(database_url: &str, source_id: Uuid) -> Result<Option<Self>> {
        let mut conn = PgConnection::connect(database_url)
            .await
            .context("opening the lock connection to the database")?;
        let key = advisory_key(source_id);
        let got: bool = sqlx::query("SELECT pg_try_advisory_lock($1)")
            .bind(key)
            .fetch_one(&mut conn)
            .await
            .context("taking the source lock")?
            .get(0);
        if got {
            Ok(Some(Self { conn, key }))
        } else {
            let _ = conn.close().await;
            Ok(None)
        }
    }

    /// Unlocks and closes the connection. A failure is not an error: closing the connection
    /// releases the lock anyway.
    pub async fn release(mut self) {
        if let Err(e) = sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(self.key)
            .execute(&mut self.conn)
            .await
        {
            tracing::warn!(error = %e, "could not unlock the source");
        }
        let _ = self.conn.close().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_is_the_first_eight_bytes_of_the_id() {
        let id = Uuid::parse_str("00000000-0000-0000-8000-000000000000").unwrap();
        assert_eq!(advisory_key(id), 0);
        // Big-endian: the version nibble of a v4 id sits in the key.
        let id = Uuid::parse_str("00000000-0000-4000-8000-000000000000").unwrap();
        assert_eq!(advisory_key(id), 0x4000);
        let id = Uuid::parse_str("00000000-0000-0001-8000-000000000000").unwrap();
        assert_eq!(advisory_key(id), 1);
        let other = Uuid::parse_str("00000000-0000-0002-8000-000000000000").unwrap();
        assert_ne!(advisory_key(id), advisory_key(other));
    }
}
