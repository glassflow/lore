//! Progress of repo pushes, for the admin page. The indexer reports into a [`PushRegistry`] as it
//! goes: a push is `waiting` for its source's lock, then `fetching` the bundle into the mirror,
//! then `indexing` its files, and ends `done` or `failed`. The registry keeps the running pushes
//! and the last [`RECENT_PUSHES`] that ended. It lives in memory only (lore runs as a single
//! replica), so a restart forgets every push.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};

use chrono::{DateTime, Utc};
use serde::Serialize;
use uuid::Uuid;

use super::IndexReport;
use crate::store::{IndexStats, Source};

/// How many ended pushes are kept.
pub const RECENT_PUSHES: usize = 20;

/// Where a push is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PushState {
    /// Waiting for another push to the same source to end.
    Waiting,
    /// Fetching the bundle into the mirror and listing the files to index.
    Fetching,
    /// Indexing files: `files_done` of `files_total`.
    Indexing,
    Done,
    Failed,
}

impl PushState {
    pub fn is_running(self) -> bool {
        !matches!(self, PushState::Done | PushState::Failed)
    }
}

/// One push, as the admin API shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PushProgress {
    /// Counts up from 1 since the server started.
    pub id: u64,
    pub tenant_id: Uuid,
    pub source_id: Uuid,
    /// The source name.
    pub source: String,
    /// The commit being indexed.
    pub commit: String,
    /// Whether the whole tree is indexed: asked for by the client, or the source's first push.
    pub full: bool,
    pub state: PushState,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    /// Files to index or delete. `None` until the bundle is fetched and the changes are listed.
    pub files_total: Option<usize>,
    pub files_done: usize,
    /// Chunks written by this push so far (unchanged files add none).
    pub chunks: usize,
    /// How many of those chunks the embedder computed a vector for. The others were unchanged
    /// within a changed file and kept their stored vector.
    pub chunks_embedded: usize,
    /// Wall-clock milliseconds spent embedding, the CPU-heavy part of a push. It includes waits
    /// for the embedder while another push uses it.
    pub embed_ms: u64,
    /// Why a failed push failed.
    pub error: Option<String>,
}

/// What [`PushRegistry::snapshot`] returns: running and ended pushes, newest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PushSnapshot {
    /// When the registry was created, that is when the server started.
    pub since: DateTime<Utc>,
    pub running: Vec<PushProgress>,
    pub recent: Vec<PushProgress>,
}

#[derive(Default)]
struct Inner {
    last_id: u64,
    /// In start order.
    running: Vec<PushProgress>,
    /// Newest first, at most `RECENT_PUSHES`.
    recent: VecDeque<PushProgress>,
}

/// The pushes of this server: the running ones and the last few that ended.
pub struct PushRegistry {
    since: DateTime<Utc>,
    inner: Mutex<Inner>,
}

impl Default for PushRegistry {
    fn default() -> Self {
        Self {
            since: Utc::now(),
            inner: Mutex::default(),
        }
    }
}

impl PushRegistry {
    /// Registers a push of `commit` to `source`, `waiting` for the source's lock. The push ends
    /// through the returned handle; a handle dropped before that marks the push failed.
    pub fn start(self: &Arc<Self>, source: &Source, commit: &str, full: bool) -> PushHandle {
        let mut inner = self.lock();
        inner.last_id += 1;
        let id = inner.last_id;
        inner.running.push(PushProgress {
            id,
            tenant_id: source.tenant_id,
            source_id: source.id,
            source: source.name.clone(),
            commit: commit.to_string(),
            full,
            state: PushState::Waiting,
            started_at: Utc::now(),
            finished_at: None,
            files_total: None,
            files_done: 0,
            chunks: 0,
            chunks_embedded: 0,
            embed_ms: 0,
            error: None,
        });
        PushHandle {
            registry: self.clone(),
            id,
            ended: false,
        }
    }

    /// The running pushes and the recent ones that ended, newest first.
    pub fn snapshot(&self) -> PushSnapshot {
        let inner = self.lock();
        PushSnapshot {
            since: self.since,
            running: inner.running.iter().rev().cloned().collect(),
            recent: inner.recent.iter().cloned().collect(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn update(&self, id: u64, f: impl FnOnce(&mut PushProgress)) {
        let mut inner = self.lock();
        if let Some(p) = inner.running.iter_mut().find(|p| p.id == id) {
            f(p);
        }
    }

    /// Moves a running push to the recent list, in its final state.
    fn end(&self, id: u64, f: impl FnOnce(&mut PushProgress)) {
        let mut inner = self.lock();
        let Some(at) = inner.running.iter().position(|p| p.id == id) else {
            return;
        };
        let mut p = inner.running.remove(at);
        f(&mut p);
        p.finished_at = Some(Utc::now());
        inner.recent.push_front(p);
        inner.recent.truncate(RECENT_PUSHES);
    }
}

/// The indexer's handle on its push. Every update takes the registry's lock for a moment and
/// never across an `await`.
pub struct PushHandle {
    registry: Arc<PushRegistry>,
    id: u64,
    ended: bool,
}

impl PushHandle {
    pub fn id(&self) -> u64 {
        self.id
    }

    /// The source's lock is held: the bundle is being fetched.
    pub fn fetching(&self) {
        self.registry
            .update(self.id, |p| p.state = PushState::Fetching);
    }

    /// The changes are listed: `files_total` files to index or delete.
    pub fn indexing(&self, files_total: usize, full: bool) {
        self.registry.update(self.id, |p| {
            p.state = PushState::Indexing;
            p.files_total = Some(files_total);
            p.full = full;
        });
    }

    /// One more file is done; `totals` are the push's counts so far.
    pub fn file_done(&self, totals: &IndexStats) {
        self.registry.update(self.id, |p| {
            p.files_done += 1;
            p.chunks = totals.chunks;
            p.chunks_embedded = totals.embedded;
            p.embed_ms = u64::try_from(totals.embed_time.as_millis()).unwrap_or(u64::MAX);
        });
    }

    pub fn done(mut self, report: &IndexReport) {
        self.ended = true;
        self.registry.end(self.id, |p| {
            p.state = PushState::Done;
            p.chunks = report.chunks;
        });
    }

    pub fn failed(mut self, error: &str) {
        self.ended = true;
        self.registry.end(self.id, |p| {
            p.state = PushState::Failed;
            p.error = Some(error.to_string());
        });
    }
}

impl Drop for PushHandle {
    fn drop(&mut self) {
        if !self.ended {
            self.registry.end(self.id, |p| {
                p.state = PushState::Failed;
                p.error = Some("the push stopped before it ended".to_string());
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(name: &str) -> Source {
        Source {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            kind: "repo".to_string(),
            name: name.to_string(),
            web_url_template: None,
            indexed_commit: None,
        }
    }

    fn report(chunks: usize) -> IndexReport {
        IndexReport {
            indexed_commit: "a".repeat(40),
            files_indexed: 2,
            files_unchanged: 0,
            files_skipped: 0,
            files_deleted: 0,
            chunks,
        }
    }

    #[test]
    fn a_push_moves_through_its_states_and_ends_done() {
        let registry = Arc::new(PushRegistry::default());
        let src = source("sample");
        let push = registry.start(&src, &"a".repeat(40), false);

        let snap = registry.snapshot();
        assert_eq!(snap.running.len(), 1);
        assert!(snap.recent.is_empty());
        let p = &snap.running[0];
        assert_eq!(p.id, push.id());
        assert_eq!(p.state, PushState::Waiting);
        assert_eq!(
            (p.source.as_str(), p.source_id, p.tenant_id),
            ("sample", src.id, src.tenant_id)
        );
        assert_eq!(p.files_total, None);
        assert!(p.finished_at.is_none());

        push.fetching();
        assert_eq!(registry.snapshot().running[0].state, PushState::Fetching);

        push.indexing(3, true);
        let mut totals = IndexStats {
            chunks: 4,
            embedded: 4,
            embed_time: std::time::Duration::from_millis(120),
        };
        push.file_done(&totals);
        totals.chunks = 9;
        totals.embedded = 7;
        totals.embed_time += std::time::Duration::from_millis(80);
        push.file_done(&totals);
        let p = registry.snapshot().running[0].clone();
        assert_eq!(p.state, PushState::Indexing);
        assert_eq!((p.files_done, p.files_total, p.chunks), (2, Some(3), 9));
        assert_eq!((p.chunks_embedded, p.embed_ms), (7, 200));
        assert!(p.full);

        push.done(&report(11));
        let snap = registry.snapshot();
        assert!(snap.running.is_empty());
        let p = &snap.recent[0];
        assert_eq!(p.state, PushState::Done);
        assert_eq!(p.chunks, 11);
        assert!(p.error.is_none());
        assert!(p.finished_at.is_some_and(|t| t >= p.started_at));
    }

    #[test]
    fn a_failed_push_keeps_its_error() {
        let registry = Arc::new(PushRegistry::default());
        let push = registry.start(&source("sample"), &"b".repeat(40), false);
        push.fetching();
        push.failed("need a full bundle");
        let snap = registry.snapshot();
        assert!(snap.running.is_empty());
        assert_eq!(snap.recent[0].state, PushState::Failed);
        assert_eq!(snap.recent[0].error.as_deref(), Some("need a full bundle"));
    }

    #[test]
    fn a_dropped_handle_marks_the_push_failed() {
        let registry = Arc::new(PushRegistry::default());
        let push = registry.start(&source("sample"), &"c".repeat(40), false);
        push.indexing(5, false);
        drop(push);
        let snap = registry.snapshot();
        assert!(snap.running.is_empty());
        assert_eq!(snap.recent[0].state, PushState::Failed);
        assert_eq!(
            snap.recent[0].error.as_deref(),
            Some("the push stopped before it ended")
        );
    }

    #[test]
    fn running_and_recent_are_newest_first_and_recent_is_capped() {
        let registry = Arc::new(PushRegistry::default());
        let src = source("sample");
        for _ in 0..RECENT_PUSHES + 5 {
            registry
                .start(&src, &"d".repeat(40), false)
                .done(&report(1));
        }
        let first = registry.start(&src, &"e".repeat(40), false);
        let second = registry.start(&src, &"f".repeat(40), false);
        let snap = registry.snapshot();

        let running: Vec<u64> = snap.running.iter().map(|p| p.id).collect();
        assert_eq!(running, [second.id(), first.id()]);
        assert_eq!(snap.recent.len(), RECENT_PUSHES);
        let recent: Vec<u64> = snap.recent.iter().map(|p| p.id).collect();
        let newest = (RECENT_PUSHES + 5) as u64;
        let expected: Vec<u64> = (6..=newest).rev().collect();
        assert_eq!(recent, expected);
        assert!(snap.recent.iter().all(|p| p.state == PushState::Done));
    }

    #[test]
    fn ids_count_up_and_states_say_whether_they_run() {
        let registry = Arc::new(PushRegistry::default());
        let a = registry.start(&source("a"), &"a".repeat(40), false);
        let b = registry.start(&source("b"), &"a".repeat(40), false);
        assert_eq!((a.id(), b.id()), (1, 2));
        for s in [PushState::Waiting, PushState::Fetching, PushState::Indexing] {
            assert!(s.is_running());
        }
        assert!(!PushState::Done.is_running());
        assert!(!PushState::Failed.is_running());
        assert_eq!(
            serde_json::to_value(PushState::Indexing).unwrap(),
            "indexing"
        );
    }
}
