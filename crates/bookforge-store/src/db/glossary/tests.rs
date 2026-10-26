//! Two-connection tests for the guidance upserts in `glossary.rs`.
//!
//! The race tests pause the first writer just before its INSERT runs, using an
//! SQLite statement trace, and detect a second writer blocked on the lock
//! through its busy handler. Neither side sleeps, so the interleaving is
//! deterministic: an upsert that does not reserve the writer before its first
//! lookup lets the second writer finish first and makes the paused one fail.

use std::{
    cell::RefCell,
    ffi::{CStr, c_char, c_int, c_uint, c_void},
    fs,
    path::PathBuf,
    sync::mpsc,
    time::Duration,
};

use bookforge_core::{GlossaryCategory, GlossaryScopeKind, GlossaryStatus, GlossaryTerm};

use crate::db::{JobStore, NewEntity, NewGlossaryCandidate, NewStyleSheet, Result};

const GATE_TIMEOUT: Duration = Duration::from_secs(10);

fn temp_path(name: &str) -> PathBuf {
    // Local mirror of db/tests.rs's helper: a counter keeps parallel tests on
    // distinct database files even when the clock is coarse.
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "bookforge-store-guidance-test-{}-{}-{}-{name}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

fn remove_database(path: &PathBuf) {
    for suffix in ["", "-wal", "-shm"] {
        let mut file = path.clone().into_os_string();
        file.push(suffix);
        let _ = fs::remove_file(PathBuf::from(file));
    }
}

/// One guidance write path, driven with a distinguishing value.
#[derive(Clone, Copy, Debug)]
enum Upsert {
    GlobalGlossaryTerm,
    GlobalStyleSheet,
    GlobalEntity,
    BookGlossaryCandidate,
}

impl Upsert {
    fn insert_prefix(self) -> &'static str {
        match self {
            Self::GlobalGlossaryTerm | Self::BookGlossaryCandidate => "INSERT INTO glossary_terms",
            Self::GlobalStyleSheet => "INSERT INTO style_sheets",
            Self::GlobalEntity => "INSERT INTO entities",
        }
    }

    fn write(self, store: &JobStore, value: u8) -> Result<()> {
        let text = format!("value-{value}");
        match self {
            Self::GlobalGlossaryTerm => store
                .upsert_glossary_terms(&[GlossaryTerm {
                    id: None,
                    scope_kind: GlossaryScopeKind::Global,
                    scope_id: None,
                    source_text: "Quillon".to_owned(),
                    target_text: text,
                    category: GlossaryCategory::Object,
                    notes: None,
                    case_sensitive: true,
                    always_active: false,
                    status: GlossaryStatus::UserSeeded,
                    source_language: "English".to_owned(),
                    target_language: "Italian".to_owned(),
                    source_count: 0,
                }])
                .map(drop),
            Self::GlobalStyleSheet => store
                .upsert_style_sheet(&NewStyleSheet {
                    scope_kind: GlossaryScopeKind::Global,
                    scope_id: None,
                    target_language: "Italian",
                    content_toml: &text,
                    fingerprint: &text,
                })
                .map(drop),
            Self::GlobalEntity => store
                .upsert_entities(&[NewEntity {
                    scope_kind: GlossaryScopeKind::Global,
                    scope_id: None,
                    source_name: "Quillon",
                    target_name: &text,
                    gender_target: None,
                    role: None,
                    notes: None,
                    source_language: "English",
                    target_language: "Italian",
                }])
                .map(drop),
            Self::BookGlossaryCandidate => store
                .upsert_glossary_candidates(
                    "book-one",
                    "English",
                    "Italian",
                    &[NewGlossaryCandidate {
                        source_text: "Quillon",
                        category: GlossaryCategory::Object,
                        source_count: usize::from(value),
                    }],
                )
                .map(drop),
        }
    }

    fn stored_values(self, store: &JobStore) -> Vec<String> {
        match self {
            Self::GlobalGlossaryTerm => store
                .load_active_glossary_terms("English", "Italian", None, None)
                .unwrap()
                .into_iter()
                .map(|row| row.target_text)
                .collect(),
            Self::GlobalStyleSheet => store
                .load_active_style_sheets("Italian", None, None)
                .unwrap()
                .into_iter()
                .map(|row| row.content_toml)
                .collect(),
            Self::GlobalEntity => store
                .load_active_entities("English", "Italian", None, None)
                .unwrap()
                .into_iter()
                .map(|row| row.target_name)
                .collect(),
            Self::BookGlossaryCandidate => store
                .list_glossary_candidates("book-one", "English", "Italian")
                .unwrap()
                .into_iter()
                .map(|row| format!("value-{}", row.source_count))
                .collect(),
        }
    }
}

/// Pauses the first statement on this thread whose SQL starts with `prefix`.
struct StatementGate {
    prefix: &'static str,
    reached: mpsc::Sender<()>,
    resume: mpsc::Receiver<()>,
}

/// Reports the first time this thread's connection waits for a lock, then
/// holds the wait until released.
struct BusyGate {
    waiting: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
}

thread_local! {
    static STATEMENT_GATE: RefCell<Option<StatementGate>> = const { RefCell::new(None) };
    static BUSY_GATE: RefCell<Option<BusyGate>> = const { RefCell::new(None) };
}

unsafe extern "C" fn pause_at_gated_statement(
    _event: c_uint,
    _context: *mut c_void,
    _statement: *mut c_void,
    sql: *mut c_void,
) -> c_int {
    // SAFETY: registered for SQLITE_TRACE_STMT only, for which SQLite passes
    // the statement's NUL-terminated SQL text, valid until this returns.
    let sql = unsafe { CStr::from_ptr(sql.cast::<c_char>()) };
    let gate = STATEMENT_GATE.with(|slot| {
        let mut slot = slot.borrow_mut();
        let hit = slot
            .as_ref()
            .is_some_and(|gate| sql.to_bytes().starts_with(gate.prefix.as_bytes()));
        if hit { slot.take() } else { None }
    });
    if let Some(gate) = gate {
        let _ = gate.reached.send(());
        let _ = gate.resume.recv_timeout(GATE_TIMEOUT);
    }
    0
}

fn hold_first_busy_wait(attempt: i32) -> bool {
    if attempt != 0 {
        return false;
    }
    BUSY_GATE.with(|slot| {
        slot.borrow_mut().take().is_some_and(|gate| {
            gate.waiting.send(()).is_ok() && gate.release.recv_timeout(GATE_TIMEOUT).is_ok()
        })
    })
}

fn assert_concurrent_upserts_are_serialized(kind: Upsert) {
    let db_path = temp_path("guidance_race.sqlite");
    let first = JobStore::open(&db_path).unwrap();
    let second = JobStore::open(&db_path).unwrap();

    let (reached_tx, reached_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let first_writer = std::thread::spawn(move || {
        STATEMENT_GATE.with(|slot| {
            *slot.borrow_mut() = Some(StatementGate {
                prefix: kind.insert_prefix(),
                reached: reached_tx,
                resume: resume_rx,
            });
        });
        // SAFETY: the connection lives on this thread for the whole call and
        // the callback keeps no pointer beyond its own invocation.
        let status = unsafe {
            rusqlite::ffi::sqlite3_trace_v2(
                first.conn.borrow().handle(),
                rusqlite::ffi::SQLITE_TRACE_STMT,
                Some(pause_at_gated_statement),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(status, rusqlite::ffi::SQLITE_OK);
        kind.write(&first, 1)
    });
    reached_rx
        .recv_timeout(GATE_TIMEOUT)
        .expect("first writer should reach its INSERT");

    let (waiting_tx, waiting_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let second_writer = std::thread::spawn(move || {
        BUSY_GATE.with(|slot| {
            *slot.borrow_mut() = Some(BusyGate {
                waiting: waiting_tx.clone(),
                release: release_rx,
            });
        });
        second
            .conn
            .borrow()
            .busy_handler(Some(hold_first_busy_wait))
            .unwrap();
        let result = kind.write(&second, 2);
        // Whichever comes first, a lock wait or completion, unblocks the test.
        let _ = waiting_tx.send(());
        (second, result)
    });
    waiting_rx
        .recv_timeout(GATE_TIMEOUT)
        .expect("second writer should wait or finish");
    resume_tx.send(()).unwrap();
    let first_result = first_writer.join().unwrap();
    let _ = release_tx.send(());
    let (second, second_result) = second_writer.join().unwrap();

    first_result.unwrap_or_else(|error| panic!("{kind:?}: first upsert failed: {error}"));
    second_result.unwrap_or_else(|error| panic!("{kind:?}: second upsert failed: {error}"));
    assert_eq!(
        kind.stored_values(&second),
        ["value-2"],
        "{kind:?}: the later writer must update the row the earlier one created"
    );
    drop(second);
    remove_database(&db_path);
}

#[test]
fn concurrent_global_glossary_upserts_queue_instead_of_failing() {
    assert_concurrent_upserts_are_serialized(Upsert::GlobalGlossaryTerm);
}

#[test]
fn concurrent_global_style_upserts_queue_instead_of_failing() {
    assert_concurrent_upserts_are_serialized(Upsert::GlobalStyleSheet);
}

#[test]
fn concurrent_global_entity_upserts_queue_instead_of_failing() {
    assert_concurrent_upserts_are_serialized(Upsert::GlobalEntity);
}

#[test]
fn concurrent_glossary_candidate_upserts_queue_instead_of_failing() {
    assert_concurrent_upserts_are_serialized(Upsert::BookGlossaryCandidate);
}

#[test]
fn empty_guidance_batches_write_nothing() {
    let db_path = temp_path("guidance_empty.sqlite");
    let store = JobStore::open(&db_path).unwrap();
    assert_eq!(store.upsert_glossary_terms(&[]).unwrap(), 0);
    assert_eq!(
        store
            .upsert_glossary_candidates("book-one", "English", "Italian", &[])
            .unwrap(),
        Default::default()
    );
    assert_eq!(store.upsert_entities(&[]).unwrap(), 0);
    drop(store);
    remove_database(&db_path);
}
