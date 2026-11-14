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
//! running index waits for it instead of writing at the same time (and, once it has the row, checks
//! that the indexed commit is still the one its diff started from, else it asks for a full
//! index). The advisory lock adds what
//! that cannot: a second `lore index` fails at once instead of embedding the repo twice.
//!
//! The connection sits idle for the whole run, which can be hours (embedding a big repo), and a
//! NAT or load balancer in between may drop an idle TCP connection without either end noticing;
//! Postgres would then end the session and release the lock silently. sqlx 0.8 has no TCP
//! keepalive option, so a background task owns the connection and runs `SELECT 1` on it every
//! [`PING_INTERVAL`] until the lock is released or dropped.
//!
//! A half-open connection (the same NAT case) does not fail a query, it just never answers, and
//! the kernel's TCP retransmit timeout can take a quarter of an hour to give up. Every query on
//! the lock connection, the ping, the unlock and the close, is therefore bound by
//! [`QUERY_TIMEOUT`], and [`SourceAdvisoryLock::release`] waits for the task for a bounded time
//! too, so `lore index` can always exit. A ping that fails or times out ends the keepalive: the
//! connection is gone, so is the session lock, and there is nothing left to keep alive. The index
//! goes on (the row lock and the lock check at commit stay in force) and an error is logged.
//!
//! This needs a session-level connection to Postgres itself: through a transaction-mode pooler
//! (PgBouncer in `transaction` pool mode) the session advisory lock would belong to whichever
//! server connection the pooler handed out for that statement, and it would be lost or leak.
//! Point `LORE_DATABASE_URL` of `lore index` at Postgres directly, or at a session-mode pooler.

use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::postgres::PgConnection;
use sqlx::{Connection, Row};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use uuid::Uuid;

/// How often the idle lock connection is used, so that a NAT or load balancer does not drop it.
pub const PING_INTERVAL: Duration = Duration::from_secs(30);

/// The longest a query on the lock connection may take (the ping, the unlock, the close). Longer
/// means the connection is half-open: it never answers.
pub const QUERY_TIMEOUT: Duration = Duration::from_secs(10);

/// The advisory lock key of a source: the first 8 bytes of its (random, version 4) id.
pub fn advisory_key(source_id: Uuid) -> i64 {
    let bytes = source_id.as_bytes();
    let mut head = [0u8; 8];
    head.copy_from_slice(&bytes[..8]);
    i64::from_be_bytes(head)
}

/// The held advisory lock of one source. Call [`SourceAdvisoryLock::release`] when done; the
/// lock is dropped with the connection either way (the task ends and closes it when this is
/// dropped).
pub struct SourceAdvisoryLock {
    /// Tells the task owning the connection to unlock and close it; dropping it stops the task.
    release: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
    query_timeout: Duration,
}

impl SourceAdvisoryLock {
    /// Opens a dedicated connection to `database_url` and tries the lock without waiting.
    /// `Ok(None)` means another session holds it.
    pub async fn try_acquire(database_url: &str, source_id: Uuid) -> Result<Option<Self>> {
        Self::try_acquire_pinging(database_url, source_id, PING_INTERVAL).await
    }

    /// [`Self::try_acquire`] with the interval of the keepalive pings (a test makes it short).
    pub async fn try_acquire_pinging(
        database_url: &str,
        source_id: Uuid,
        ping_every: Duration,
    ) -> Result<Option<Self>> {
        Self::try_acquire_timed(database_url, source_id, ping_every, QUERY_TIMEOUT).await
    }

    /// [`Self::try_acquire_pinging`] with the time one query on the connection may take (a test
    /// makes it short).
    pub async fn try_acquire_timed(
        database_url: &str,
        source_id: Uuid,
        ping_every: Duration,
        query_timeout: Duration,
    ) -> Result<Option<Self>> {
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
        if !got {
            let _ = tokio::time::timeout(query_timeout, conn.close()).await;
            return Ok(None);
        }
        let (release, released) = oneshot::channel();
        let task = tokio::spawn(hold(conn, key, ping_every, query_timeout, released));
        Ok(Some(Self {
            release: Some(release),
            task: Some(task),
            query_timeout,
        }))
    }

    /// Unlocks and closes the connection. A failure is not an error: closing the connection
    /// releases the lock anyway. It waits for the task for a bounded time (the unlock and the
    /// close each have the query timeout), so it returns even when the connection is half-open.
    pub async fn release(mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
        if let Some(task) = self.task.take() {
            let abort = task.abort_handle();
            if tokio::time::timeout(self.query_timeout * 3, task)
                .await
                .is_err()
            {
                // Aborting drops the connection, which ends the session, and the lock with it.
                abort.abort();
                tracing::warn!("the source lock connection did not answer; dropped it");
            }
        }
    }
}

/// Owns the lock connection: pings it every `ping_every`, and on a release request unlocks and
/// closes it. When the sender is dropped without a request, the connection is just closed. Every
/// query has `query_timeout`; a ping that fails or times out ends the task, which drops the
/// connection.
async fn hold(
    mut conn: PgConnection,
    key: i64,
    ping_every: Duration,
    query_timeout: Duration,
    mut released: oneshot::Receiver<()>,
) {
    let mut next = tokio::time::Instant::now() + ping_every;
    loop {
        tokio::select! {
            requested = &mut released => {
                if requested.is_ok() {
                    let unlock = tokio::time::timeout(
                        query_timeout,
                        sqlx::query("SELECT pg_advisory_unlock($1)")
                            .bind(key)
                            .execute(&mut conn),
                    )
                    .await;
                    match unlock {
                        Ok(Ok(_)) => {}
                        Ok(Err(e)) => tracing::warn!(error = %e, "could not unlock the source"),
                        Err(_) => tracing::warn!("the source unlock got no answer in time"),
                    }
                }
                let _ = tokio::time::timeout(query_timeout, conn.close()).await;
                return;
            }
            _ = tokio::time::sleep_until(next) => {
                next = tokio::time::Instant::now() + ping_every;
                let ping = tokio::time::timeout(
                    query_timeout,
                    sqlx::query("SELECT 1").execute(&mut conn),
                )
                .await;
                match ping {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => {
                        tracing::error!(error = %e, "the source lock connection failed its keepalive; the lock may be lost");
                        return;
                    }
                    Err(_) => {
                        tracing::error!("the source lock connection did not answer its keepalive; the lock may be lost");
                        return;
                    }
                }
            }
        }
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
