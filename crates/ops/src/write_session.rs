// SPDX-License-Identifier: Apache-2.0

//! Write sessions: the one entry every write takes, whatever the front
//! end.
//!
//! A session is the lock a write holds, the [`WriteIntent`] that names
//! what it may touch and on whose behalf, and the [`ChangeSink`] that
//! hears about it. Opening one is the only way to hold the lock, and
//! dropping it is the only way to release it, so a write cannot leave
//! the lock held or its subscribers unaware.
//!
//! Two kinds of session exist:
//!
//! * an **exclusive** session — a library session from
//!   [`crate::registry::LibraryRegistry::begin_write`] or a host session
//!   from [`crate::registry::LibraryRegistry::begin_host_write`] — takes
//!   the registry's shared write mutex without waiting, so a second
//!   exclusive writer is refused with [`WriteBusy`] instead of queueing
//!   behind the first. The sink hears it start and end.
//! * an **intake** session ([`Region::Intake`]) is opened by a library
//!   handle's pipeline methods around one ingest or glean. It waits for
//!   that handle's per-kind lock rather than the shared one, and tells
//!   no sink anything: the queue worker that drives it owns the job's
//!   announcements.
//!
//! An exclusive session runs its write through [`WriteSession::run_blocking`]
//! (or [`HostWrite::run_blocking`]): the write, the vector-store refresh
//! it asks for, its announcements, and the release all happen on one
//! blocking task, so a caller that stops waiting cannot separate a
//! committed write from its `library.changed` or from the release. An
//! intake session runs in place through [`WriteSession::run_inline`],
//! inside the blocking task its caller already runs on.

use std::sync::Arc;

use bookrack_core::{ItemKind, Problem};
use bookrack_embed::Embedder;
use bookrack_query::QueryError;
use tokio::sync::OwnedMutexGuard;

use crate::registry::LibraryHandle;
use crate::{Caller, Ops};

/// What a write may touch.
///
/// The region a write declares is what the lock it holds covers. Every
/// region but [`Region::Intake`] maps to the registry's one shared
/// write mutex today; the finer regions name what a finer lock map
/// would key on, without changing who waits for whom yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Region {
    /// The whole library: a reset, an index rebuild, a re-embed or
    /// corpus rebuild with no single target, or a change to the set of
    /// served libraries.
    Library,
    /// Bringing new items of `kind` in. The intake id exists only once
    /// the session registers it, so the region is the kind.
    Intake {
        /// The side the items arrive on.
        kind: ItemKind,
    },
    /// One item's derived data: removing it, re-embedding or
    /// rebuilding it alone.
    Item {
        /// The side the item lives on.
        kind: ItemKind,
        /// The item's intake id.
        intake_id: i64,
    },
    /// One item's curated record: its bibliographic fields and review
    /// state.
    Curation {
        /// The side the item lives on.
        kind: ItemKind,
        /// The item's intake id.
        intake_id: i64,
    },
    /// The entries of one reference book.
    Reference {
        /// The reference book's slug.
        book_slug: String,
    },
}

/// What a session is for: the library it writes, the region it may
/// touch, who asked, and why.
#[derive(Debug, Clone)]
pub struct WriteIntent {
    /// The registry name of the library the write acts on. For a host
    /// write, the library whose entry in the served set changes.
    pub library: String,
    /// What the write may touch. A host write declares
    /// [`Region::Library`].
    pub region: Region,
    /// Who the write is attributed to.
    pub caller: Caller,
    /// A free-form note on why the write runs, for the host's own
    /// records; `None` when the opener gave none.
    pub reason: Option<String>,
}

/// What a finished write changed, as its body reports it.
///
/// The session acts on it after the body returns: it refreshes the
/// named vector store, then announces each named library through the
/// sink. A write that changed nothing — a dry run — returns
/// [`ChangeSet::unchanged`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChangeSet {
    /// Libraries whose subscribers should refresh what they believe
    /// about them, in announcement order.
    pub announced_libraries: Vec<String>,
    /// The side whose warm vector store rebinds from disk before the
    /// session ends. A side without a vector store, or a session with
    /// no library handle, rebinds nothing.
    pub refresh_vectors: Option<ItemKind>,
}

impl ChangeSet {
    /// A write that changed nothing anyone needs to hear about.
    pub fn unchanged() -> ChangeSet {
        ChangeSet::default()
    }

    /// A write that changed `library`.
    pub fn announced(library: impl Into<String>) -> ChangeSet {
        ChangeSet {
            announced_libraries: vec![library.into()],
            refresh_vectors: None,
        }
    }

    /// This change set, also rebinding the warm vector store of `kind`
    /// before the session ends.
    pub fn with_refresh(mut self, kind: ItemKind) -> ChangeSet {
        self.refresh_vectors = Some(kind);
        self
    }
}

/// An exclusive session was asked for while another one holds the
/// write mutex.
#[derive(Debug, thiserror::Error)]
#[error("another write command is already in progress")]
pub struct WriteBusy;

impl bookrack_core::Explain for WriteBusy {
    fn explain(&self) -> Problem {
        Problem::new(self.to_string())
            .detail("Another write or curation session is running on the daemon.")
            .hint(
                "Retry once the running write finishes. A paper re-audit can hold the \
                 session while it waits for a glean job on the same library.",
            )
            .retryable(true)
    }
}

/// Why running a session's write failed.
#[derive(Debug, thiserror::Error)]
pub enum SessionRunError<E> {
    /// The write committed and was announced, but its vector store did
    /// not rebind from disk.
    #[error("refresh vector store after write")]
    RefreshStore(#[source] QueryError),
    /// The blocking task running the write did not finish: the write
    /// panicked. The session ended and released its lock all the same.
    #[error("write session task failed to join")]
    Join(#[source] tokio::task::JoinError),
    /// The write itself failed; nothing was announced.
    #[error(transparent)]
    Op(E),
}

/// What the host hears about sessions.
///
/// Implemented by the host — the daemon pauses its MCP surface around
/// an exclusive write and forwards announcements to its event stream —
/// so this crate never depends on the daemon's events.
pub trait ChangeSink: Send + Sync {
    /// An exclusive session took the write mutex.
    fn exclusive_write_started(&self, intent: &WriteIntent);
    /// A finished write changed `library`.
    fn library_changed(&self, library: &str);
    /// An exclusive session ended; its lock is released right after
    /// this returns.
    fn exclusive_write_ended(&self, intent: &WriteIntent);
}

/// A sink that hears nothing: for hosts with no subscribers, and for
/// intake sessions, whose announcements the queue worker owns.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopChangeSink;

impl ChangeSink for NoopChangeSink {
    fn exclusive_write_started(&self, _intent: &WriteIntent) {}
    fn library_changed(&self, _library: &str) {}
    fn exclusive_write_ended(&self, _intent: &WriteIntent) {}
}

/// The part every session holds: its intent, its sink, and its lock.
/// Dropping it ends the session — the sink hears an exclusive session
/// end first, and the lock is released after.
struct SessionCore {
    intent: WriteIntent,
    sink: Arc<dyn ChangeSink>,
    exclusive: bool,
    _guard: OwnedMutexGuard<()>,
}

impl SessionCore {
    /// An exclusive session over `guard`, already taken; tells the sink
    /// it started.
    fn exclusive(
        intent: WriteIntent,
        sink: Arc<dyn ChangeSink>,
        guard: OwnedMutexGuard<()>,
    ) -> SessionCore {
        sink.exclusive_write_started(&intent);
        SessionCore {
            intent,
            sink,
            exclusive: true,
            _guard: guard,
        }
    }
}

impl Drop for SessionCore {
    fn drop(&mut self) {
        if self.exclusive {
            self.sink.exclusive_write_ended(&self.intent);
        }
        // `_guard` releases the lock after this body returns, so the
        // next session starts only once this one's end was heard.
    }
}

/// A session over one library.
pub struct WriteSession<E: Embedder> {
    handle: Arc<LibraryHandle<E>>,
    core: SessionCore,
}

impl<E: Embedder> WriteSession<E> {
    /// An exclusive session on `handle` over the registry's write
    /// mutex, already taken.
    pub(crate) fn exclusive(
        handle: Arc<LibraryHandle<E>>,
        intent: WriteIntent,
        sink: Arc<dyn ChangeSink>,
        guard: OwnedMutexGuard<()>,
    ) -> WriteSession<E> {
        WriteSession {
            handle,
            core: SessionCore::exclusive(intent, sink, guard),
        }
    }

    /// An intake session on `handle` over its per-kind lock, already
    /// taken. It tells no sink anything.
    pub(crate) fn intake(
        handle: Arc<LibraryHandle<E>>,
        kind: ItemKind,
        caller: Caller,
        guard: OwnedMutexGuard<()>,
    ) -> WriteSession<E> {
        let intent = WriteIntent {
            library: handle.name().to_string(),
            region: Region::Intake { kind },
            caller,
            reason: None,
        };
        WriteSession {
            handle,
            core: SessionCore {
                intent,
                sink: Arc::new(NoopChangeSink),
                exclusive: false,
                _guard: guard,
            },
        }
    }

    /// The handle of the library this session writes.
    pub fn handle(&self) -> &Arc<LibraryHandle<E>> {
        &self.handle
    }

    /// The warm ops of the library this session writes.
    pub fn ops(&self) -> &Ops<E> {
        self.handle.ops()
    }

    /// Who the write is attributed to.
    pub fn caller(&self) -> &Caller {
        &self.core.intent.caller
    }

    /// What the session is for.
    pub fn intent(&self) -> &WriteIntent {
        &self.core.intent
    }
}

impl<E: Embedder + Send + Sync + 'static> WriteSession<E> {
    /// Run an exclusive session's write on a blocking task, then
    /// refresh, announce, and end the session on that same task.
    ///
    /// `op` gets the session and returns its value with the
    /// [`ChangeSet`] it made. The session then rebinds the vector store
    /// the change set names, announces each named library — even when
    /// the rebind failed, since the write itself is already on disk —
    /// and ends. A caller that stops waiting does not stop any of that.
    /// A failed `op` announces nothing; a panicking one is
    /// [`SessionRunError::Join`]. Either way the session ends and its
    /// lock is released.
    pub async fn run_blocking<T, Err, F>(self, op: F) -> Result<T, SessionRunError<Err>>
    where
        T: Send + 'static,
        Err: Send + 'static,
        F: AsyncFnOnce(&mut WriteSession<E>) -> Result<(T, ChangeSet), Err> + Send + 'static,
    {
        run_on_blocking_task(self, op).await
    }

    /// Run a session's write in place on the current task, then
    /// refresh, announce, and end the session, as
    /// [`Self::run_blocking`] does.
    ///
    /// For an intake session, whose caller already runs on a blocking
    /// task and lends `op` borrowed inputs.
    pub async fn run_inline<T, Err, F>(mut self, op: F) -> Result<T, SessionRunError<Err>>
    where
        F: AsyncFnOnce(&mut WriteSession<E>) -> Result<(T, ChangeSet), Err>,
    {
        let outcome = settle(&mut self, op).await;
        drop(self);
        outcome
    }
}

/// A host session: a write to the set of served libraries rather than
/// to one library's stores — a mount, an unmount, a fork. It holds the
/// same write mutex as a library session and has no library handle,
/// since the library it acts on may not be served yet.
pub struct HostWrite {
    core: SessionCore,
}

impl HostWrite {
    /// An exclusive host session over the registry's write mutex,
    /// already taken.
    pub(crate) fn exclusive(
        intent: WriteIntent,
        sink: Arc<dyn ChangeSink>,
        guard: OwnedMutexGuard<()>,
    ) -> HostWrite {
        HostWrite {
            core: SessionCore::exclusive(intent, sink, guard),
        }
    }

    /// Who the write is attributed to.
    pub fn caller(&self) -> &Caller {
        &self.core.intent.caller
    }

    /// What the session is for.
    pub fn intent(&self) -> &WriteIntent {
        &self.core.intent
    }

    /// Run the host write on a blocking task, then announce and end the
    /// session on that same task, as [`WriteSession::run_blocking`]
    /// does. A host session has no vector store, so a change set's
    /// refresh is ignored.
    pub async fn run_blocking<T, Err, F>(self, op: F) -> Result<T, SessionRunError<Err>>
    where
        T: Send + 'static,
        Err: Send + 'static,
        F: AsyncFnOnce(&mut HostWrite) -> Result<(T, ChangeSet), Err> + Send + 'static,
    {
        run_on_blocking_task(self, op).await
    }
}

/// What [`settle`] needs from a session: its core, and the vector-store
/// rebind its change set may ask for.
trait Session: Send + 'static {
    fn core(&self) -> &SessionCore;
    async fn refresh(&self, kind: Option<ItemKind>) -> Result<(), QueryError>;
}

impl<E: Embedder + Send + Sync + 'static> Session for WriteSession<E> {
    fn core(&self) -> &SessionCore {
        &self.core
    }

    async fn refresh(&self, kind: Option<ItemKind>) -> Result<(), QueryError> {
        let library = match kind {
            Some(ItemKind::Book) => self.handle.ops().library(),
            Some(ItemKind::Paper) => self.handle.ops().papers_library(),
            Some(ItemKind::Reference) | None => None,
        };
        match library {
            Some(library) => library.refresh_store().await,
            None => Ok(()),
        }
    }
}

impl Session for HostWrite {
    fn core(&self) -> &SessionCore {
        &self.core
    }

    async fn refresh(&self, _kind: Option<ItemKind>) -> Result<(), QueryError> {
        Ok(())
    }
}

/// Drive `session` through `op` and [`settle`] on a blocking task, and
/// end it there.
async fn run_on_blocking_task<S, T, Err, F>(
    mut session: S,
    op: F,
) -> Result<T, SessionRunError<Err>>
where
    S: Session,
    T: Send + 'static,
    Err: Send + 'static,
    F: AsyncFnOnce(&mut S) -> Result<(T, ChangeSet), Err> + Send + 'static,
{
    let joined = tokio::task::spawn_blocking(move || {
        let runtime = tokio::runtime::Handle::current();
        let outcome = runtime.block_on(settle(&mut session, op));
        drop(session);
        outcome
    })
    .await;
    joined.unwrap_or_else(|e| Err(SessionRunError::Join(e)))
}

/// Run `op`, then act on the change set it returned: rebind, announce
/// every named library, and only then report a failed rebind.
async fn settle<S, T, Err, F>(session: &mut S, op: F) -> Result<T, SessionRunError<Err>>
where
    S: Session,
    F: AsyncFnOnce(&mut S) -> Result<(T, ChangeSet), Err>,
{
    let (value, changes) = op(session).await.map_err(SessionRunError::Op)?;
    let refreshed = session.refresh(changes.refresh_vectors).await;
    for library in &changes.announced_libraries {
        session.core().sink.library_changed(library);
    }
    refreshed.map_err(SessionRunError::RefreshStore)?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::Duration;

    use bookrack_config::Config;
    use bookrack_core::Explain;
    use bookrack_embed::EmbedError;
    use tokio::sync::Mutex as AsyncMutex;

    use super::*;
    use crate::registry::LibraryRegistry;

    struct FakeEmbedder;

    impl Embedder for FakeEmbedder {
        async fn embed_batch(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
            Ok(Vec::new())
        }
    }

    /// A sink that writes down what it hears, in order.
    #[derive(Default)]
    struct RecordingSink(Mutex<Vec<String>>);

    impl RecordingSink {
        fn heard(&self) -> Vec<String> {
            self.0.lock().expect("sink lock").clone()
        }
    }

    impl ChangeSink for RecordingSink {
        fn exclusive_write_started(&self, intent: &WriteIntent) {
            self.0
                .lock()
                .expect("sink lock")
                .push(format!("started {}", intent.library));
        }
        fn library_changed(&self, library: &str) {
            self.0
                .lock()
                .expect("sink lock")
                .push(format!("changed {library}"));
        }
        fn exclusive_write_ended(&self, intent: &WriteIntent) {
            self.0
                .lock()
                .expect("sink lock")
                .push(format!("ended {}", intent.library));
        }
    }

    fn handle(name: &str) -> Arc<LibraryHandle<FakeEmbedder>> {
        let cfg = Arc::new(Config::new(PathBuf::from(format!("/dev/null/{name}"))));
        let ops = Ops::catalog_only(
            cfg.corpus_db(),
            cfg.catalog_db(),
            &cfg.lancedb_dir(),
            cfg.books_dir(),
            cfg.backup_dir(),
            Caller::cli(),
        );
        LibraryHandle::new(name, cfg, ops)
    }

    struct Fixture {
        registry: Arc<LibraryRegistry<FakeEmbedder>>,
        sink: Arc<RecordingSink>,
        write_guard: Arc<AsyncMutex<()>>,
        alpha: Arc<LibraryHandle<FakeEmbedder>>,
    }

    fn fixture() -> Fixture {
        let sink = Arc::new(RecordingSink::default());
        let write_guard = Arc::new(AsyncMutex::new(()));
        let alpha = handle("alpha");
        let registry = LibraryRegistry::from_handles_with_sink(
            [Arc::clone(&alpha), handle("beta")],
            Some("alpha".into()),
            Arc::clone(&sink) as Arc<dyn ChangeSink>,
            Arc::clone(&write_guard),
        )
        .expect("registry");
        Fixture {
            registry,
            sink,
            write_guard,
            alpha,
        }
    }

    fn begin(fx: &Fixture) -> Result<WriteSession<FakeEmbedder>, WriteBusy> {
        fx.registry
            .begin_write(&fx.alpha, Region::Library, Caller::cli())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_second_exclusive_session_is_refused_as_busy_and_retryable() {
        let fx = fixture();
        let held = begin(&fx).expect("the first session opens");

        let Err(busy) = begin(&fx) else {
            panic!("a second library session must be refused while the first is held");
        };
        let problem = busy.explain();
        assert_eq!(
            problem.summary, "another write command is already in progress",
            "the busy summary is the one the control plane already answers with"
        );
        assert!(problem.data.retryable, "busy is retryable: {problem:?}");
        assert!(
            fx.registry
                .begin_host_write(Caller::cli(), "beta", None)
                .is_err(),
            "a host write shares the library sessions' mutex"
        );

        drop(held);
        assert!(
            begin(&fx).is_ok(),
            "the mutex is free once the session ends"
        );
    }

    /// The mutex the registry was built with is the one it takes, so a
    /// holder outside the registry — the control plane's write path —
    /// and a session keep each other out.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_registry_takes_the_write_mutex_it_was_given() {
        let fx = fixture();
        let outside = fx
            .write_guard
            .clone()
            .try_lock_owned()
            .expect("the injected mutex is free");
        assert!(
            begin(&fx).is_err(),
            "a session must not open while the injected mutex is held elsewhere"
        );
        drop(outside);
        let session = begin(&fx).expect("the session opens");
        assert!(
            fx.write_guard.try_lock().is_err(),
            "an open session holds the injected mutex"
        );
        drop(session);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_session_announces_between_its_start_and_its_end() {
        let fx = fixture();
        let session = begin(&fx).expect("session");
        let value = session
            .run_blocking(async |_s| {
                Ok::<_, std::convert::Infallible>((7, ChangeSet::announced("alpha")))
            })
            .await
            .expect("the write succeeds");
        assert_eq!(value, 7);
        assert_eq!(
            fx.sink.heard(),
            ["started alpha", "changed alpha", "ended alpha"],
        );
        assert!(begin(&fx).is_ok(), "the session released its lock");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unchanged_session_announces_nothing() {
        let fx = fixture();
        begin(&fx)
            .expect("session")
            .run_blocking(async |_s| {
                Ok::<_, std::convert::Infallible>(((), ChangeSet::unchanged()))
            })
            .await
            .expect("the dry run succeeds");
        assert_eq!(fx.sink.heard(), ["started alpha", "ended alpha"]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_write_ends_the_session_without_announcing() {
        let fx = fixture();
        let err = begin(&fx)
            .expect("session")
            .run_blocking(async |_s| Err::<((), ChangeSet), _>("refused"))
            .await
            .expect_err("the write fails");
        assert!(matches!(err, SessionRunError::Op("refused")), "{err:?}");
        assert_eq!(fx.sink.heard(), ["started alpha", "ended alpha"]);
        assert!(begin(&fx).is_ok(), "the session released its lock");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_panicking_write_ends_the_session_and_releases_the_lock() {
        let fx = fixture();
        let err = begin(&fx)
            .expect("session")
            .run_blocking(
                async |_s| -> Result<((), ChangeSet), std::convert::Infallible> {
                    panic!("the write body panicked")
                },
            )
            .await
            .expect_err("a panicking write is an error");
        assert!(matches!(err, SessionRunError::Join(_)), "{err:?}");
        assert_eq!(fx.sink.heard(), ["started alpha", "ended alpha"]);
        assert!(begin(&fx).is_ok(), "the session released its lock");
    }

    /// The caller giving up mid-write is the case `run_blocking` exists
    /// for: the write goes on, and its announcement and the release
    /// still follow it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_caller_that_stops_waiting_does_not_split_the_write_from_its_announcement() {
        let fx = fixture();
        let session = begin(&fx).expect("session");
        let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        let caller = tokio::spawn(session.run_blocking(async move |_s| {
            entered_tx.send(()).expect("signal entry");
            go_rx.recv().expect("wait for the go");
            Ok::<_, std::convert::Infallible>(((), ChangeSet::announced("alpha")))
        }));
        tokio::task::spawn_blocking(move || entered_rx.recv().expect("the write started"))
            .await
            .expect("entry wait");
        caller.abort();
        assert!(
            caller
                .await
                .expect_err("the caller was aborted")
                .is_cancelled(),
            "the waiting caller is gone"
        );
        go_tx.send(()).expect("release the write");

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while fx.sink.heard().len() < 3 {
            assert!(
                std::time::Instant::now() < deadline,
                "the session never ended: {:?}",
                fx.sink.heard()
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            fx.sink.heard(),
            ["started alpha", "changed alpha", "ended alpha"],
        );
        assert!(begin(&fx).is_ok(), "the session released its lock");
    }

    /// A session whose rebind fails, so the order of announcing and
    /// reporting the failure can be observed through [`settle`].
    struct FailingRefresh(SessionCore);

    impl Session for FailingRefresh {
        fn core(&self) -> &SessionCore {
            &self.0
        }

        async fn refresh(&self, kind: Option<ItemKind>) -> Result<(), QueryError> {
            match kind {
                Some(_) => Err(QueryError::EmptyProbe),
                None => Ok(()),
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_rebind_is_reported_after_the_write_is_announced() {
        let sink = Arc::new(RecordingSink::default());
        let guard = Arc::new(AsyncMutex::new(()))
            .try_lock_owned()
            .expect("fresh mutex");
        let intent = WriteIntent {
            library: "alpha".into(),
            region: Region::Library,
            caller: Caller::cli(),
            reason: None,
        };
        let session = FailingRefresh(SessionCore::exclusive(
            intent,
            Arc::clone(&sink) as Arc<dyn ChangeSink>,
            guard,
        ));
        let err = run_on_blocking_task(session, async |_s| {
            Ok::<_, std::convert::Infallible>((
                (),
                ChangeSet::announced("alpha").with_refresh(ItemKind::Book),
            ))
        })
        .await
        .expect_err("the rebind fails");
        assert!(
            matches!(err, SessionRunError::RefreshStore(QueryError::EmptyProbe)),
            "{err:?}"
        );
        assert_eq!(
            sink.heard(),
            ["started alpha", "changed alpha", "ended alpha"],
            "a write already on disk is announced even when its rebind fails"
        );
    }

    /// An intake session waits on its handle's own lock, not the
    /// shared write mutex, and tells the registry's sink nothing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_intake_session_holds_its_handle_lock_and_tells_no_sink() {
        let fx = fixture();
        let session = fx.alpha.intake_session(ItemKind::Book).await;
        assert!(
            fx.alpha.intake_session_is_held(ItemKind::Book),
            "the book intake lock is held by the session"
        );
        assert!(
            !fx.alpha.intake_session_is_held(ItemKind::Paper),
            "the paper intake lock is a different lock"
        );
        let exclusive = begin(&fx).expect("an intake session leaves the write mutex free");
        drop(exclusive);

        let value = session
            .run_inline(async |s| {
                assert_eq!(
                    s.intent().region,
                    Region::Intake {
                        kind: ItemKind::Book
                    }
                );
                Ok::<_, std::convert::Infallible>((
                    "done",
                    ChangeSet::unchanged().with_refresh(ItemKind::Book),
                ))
            })
            .await
            .expect("the intake succeeds");
        assert_eq!(value, "done");
        assert!(
            !fx.alpha.intake_session_is_held(ItemKind::Book),
            "the intake lock is released once the session ends"
        );
        assert_eq!(
            fx.sink.heard(),
            ["started alpha", "ended alpha"],
            "only the exclusive session was heard"
        );
    }
}
