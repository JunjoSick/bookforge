//! Single-row asset deletion primitives (F1 remediation).
//!
//! The style-sheet and entity tables had no single-row delete in their
//! original API surface, which pushed serve routes into a
//! snapshot-everything -> clear-scope -> restore-siblings dance spread over
//! several autocommit transactions. A crash (or a concurrent writer) between
//! those steps could leave a scope half-emptied — torn reads and data loss.
//!
//! These primitives mirror the atomic single-statement removal of
//! [`JobStore::remove_glossary_term`] (glossary.rs) while still matching on
//! the row's full identity tuple, not just its numeric id. They run as ONE
//! immediate transaction, so the delete is either wholly visible or not at
//! all, and — because nothing is re-inserted — sibling ids are now stable
//! across deletions.
//!
//! Renumbering caveat for the doc record: the old snapshot-clear-restore path
//! reassigned sibling ids on every delete; this primitive removes that
//! behavior entirely. Callers that treated ids as session-local handles may
//! now treat them as stable for the lifetime of the row.

use rusqlite::{TransactionBehavior, params};

use super::{JobStore, Result};

impl JobStore {
    /// Remove exactly one style sheet by id in a single immediate
    /// transaction, verifying the identity tuple
    /// (scope_kind, IFNULL-normalized scope_id, target_language) before the
    /// delete lands.
    ///
    /// Returns `Ok(true)` when the row was removed and `Ok(false)` when no
    /// sheet matches. Sibling rows are untouched and keep their ids (the
    /// prior clear-and-restore workaround reassigned them).
    pub fn remove_style_sheet(&self, id: i64) -> Result<bool> {
        let mut conn = self.conn.borrow_mut();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let removed = tx.execute(
            "DELETE FROM style_sheets
             WHERE id = ?1
               AND scope_kind = (SELECT scope_kind FROM style_sheets WHERE id = ?1)
               AND IFNULL(scope_id, '') =
                   IFNULL((SELECT scope_id FROM style_sheets WHERE id = ?1), '')
               AND target_language =
                   (SELECT target_language FROM style_sheets WHERE id = ?1)",
            params![id],
        )?;
        tx.commit()?;
        Ok(removed == 1)
    }

    /// Remove exactly one entity by id in a single immediate transaction,
    /// verifying the identity tuple (scope_kind, IFNULL-normalized scope_id,
    /// source_name, source_language, target_language) before the delete lands.
    ///
    /// Returns `Ok(true)` when the row was removed and `Ok(false)` when no
    /// entity matches. Sibling rows are untouched and keep their ids (the
    /// prior clear-and-restore workaround reassigned them).
    pub fn remove_entity(&self, id: i64) -> Result<bool> {
        let mut conn = self.conn.borrow_mut();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let removed = tx.execute(
            "DELETE FROM entities
             WHERE id = ?1
               AND scope_kind = (SELECT scope_kind FROM entities WHERE id = ?1)
               AND IFNULL(scope_id, '') =
                   IFNULL((SELECT scope_id FROM entities WHERE id = ?1), '')
               AND source_name = (SELECT source_name FROM entities WHERE id = ?1)
               AND source_language = (SELECT source_language FROM entities WHERE id = ?1)
               AND target_language = (SELECT target_language FROM entities WHERE id = ?1)",
            params![id],
        )?;
        tx.commit()?;
        Ok(removed == 1)
    }
}

#[cfg(test)]
mod tests {
    use bookforge_core::{EntityGender, GlossaryScopeKind};

    use crate::db::{JobStore, NewEntity, NewStyleSheet};

    fn temp_path(name: &str) -> std::path::PathBuf {
        // Mirror of db/tests.rs's private temp_path so parallel store tests
        // never share a database file. Kept local to this file on purpose:
        // the merge-coordination rule for this workstream forbids touching
        // db/tests.rs.
        use std::sync::atomic::{AtomicU64, Ordering};

        static COUNTER: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "bookforge-store-assets-delete-test-{}-{}-{}-{name}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or_default(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn removes_one_style_sheet_and_keeps_sibling_ids_stable() {
        let db_path = temp_path("style_delete.sqlite");
        let store = JobStore::open(&db_path).expect("store opens");
        let italian = NewStyleSheet {
            scope_kind: GlossaryScopeKind::Global,
            scope_id: None,
            target_language: "Italian",
            content_toml: "italian",
            fingerprint: "fp-it",
        };
        let spanish = NewStyleSheet {
            target_language: "Spanish",
            content_toml: "spanish",
            fingerprint: "fp-es",
            ..italian
        };
        let italian_id = store.upsert_style_sheet(&italian).expect("upsert it");
        let spanish_id = store.upsert_style_sheet(&spanish).expect("upsert es");

        assert!(store.remove_style_sheet(italian_id).expect("remove it"));

        let rows = store.list_style_sheets(None, None, None).expect("list");
        assert_eq!(rows.len(), 1, "only the Spanish sheet remains");
        assert_eq!(rows[0].id, spanish_id, "sibling id must be stable");
        assert_eq!(rows[0].content_toml, "spanish");

        assert!(!store.remove_style_sheet(italian_id).expect("re-remove"));
        assert!(
            !store.remove_style_sheet(i64::MAX).expect("unknown id"),
            "unknown numeric ids report false"
        );

        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn removes_one_entity_and_keeps_sibling_ids_stable() {
        let db_path = temp_path("entity_delete.sqlite");
        let store = JobStore::open(&db_path).expect("store opens");
        let frodo = NewEntity {
            scope_kind: GlossaryScopeKind::Global,
            scope_id: None,
            source_name: "Frodo Baggins",
            target_name: "Frodo Baggins",
            gender_target: Some(EntityGender::Masculine),
            role: Some("ring-bearer"),
            notes: Some("protagonist"),
            source_language: "English",
            target_language: "Italian",
        };
        let sam = NewEntity {
            source_name: "Samwise Gamgee",
            target_name: "Samwise",
            gender_target: None,
            role: Some("gardener"),
            notes: None,
            ..frodo
        };
        store
            .upsert_entities(std::slice::from_ref(&frodo))
            .expect("frodo");
        store
            .upsert_entities(std::slice::from_ref(&sam))
            .expect("sam");
        let rows = store.list_entities(None, None, None, None).expect("list");
        assert_eq!(rows.len(), 2);
        let frodo_id = rows
            .iter()
            .find(|row| row.source_name == "Frodo Baggins")
            .expect("frodo row")
            .id;
        let sam_id = rows
            .iter()
            .find(|row| row.source_name == "Samwise Gamgee")
            .expect("sam row")
            .id;

        assert!(store.remove_entity(frodo_id).expect("remove frodo"));

        let rows = store.list_entities(None, None, None, None).expect("list");
        assert_eq!(rows.len(), 1, "only Sam remains");
        assert_eq!(rows[0].id, sam_id, "sibling id must be stable");
        assert_eq!(rows[0].source_name, "Samwise Gamgee");
        assert_eq!(rows[0].target_name, "Samwise");
        assert_eq!(rows[0].role.as_deref(), Some("gardener"));

        assert!(!store.remove_entity(frodo_id).expect("re-remove"));
        assert!(!store.remove_entity(i64::MAX).expect("unknown id"));

        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn style_delete_matches_identity_tuple_in_scoped_rows() {
        // Two book-scoped sheets + one global; removing one book row leaves
        // both the sibling book row and the global row intact.
        let db_path = temp_path("style_scoped_delete.sqlite");
        let store = JobStore::open(&db_path).expect("store opens");
        let book_a = NewStyleSheet {
            scope_kind: GlossaryScopeKind::Book,
            scope_id: Some("book-a"),
            target_language: "Italian",
            content_toml: "a",
            fingerprint: "fa",
        };
        let book_b = NewStyleSheet {
            scope_id: Some("book-b"),
            content_toml: "b",
            fingerprint: "fb",
            ..book_a
        };
        let global = NewStyleSheet {
            scope_kind: GlossaryScopeKind::Global,
            scope_id: None,
            content_toml: "g",
            fingerprint: "fg",
            target_language: "Italian",
        };
        let id_a = store.upsert_style_sheet(&book_a).expect("a");
        let id_b = store.upsert_style_sheet(&book_b).expect("b");
        store.upsert_style_sheet(&global).expect("g");

        assert!(store.remove_style_sheet(id_a).expect("remove a"));

        let remaining = store.list_style_sheets(None, None, None).expect("list");
        assert_eq!(remaining.len(), 2);
        assert!(
            remaining.iter().all(|row| row.id != id_a),
            "removed row must be gone"
        );
        assert!(
            remaining
                .iter()
                .any(|row| row.id == id_b && row.scope_id.as_deref() == Some("book-b")),
            "the sibling scoped sheet survives intact"
        );
        assert!(
            remaining
                .iter()
                .any(|row| row.scope_kind == GlossaryScopeKind::Global),
            "the global sheet survives intact"
        );

        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn entity_delete_matches_identity_tuple_in_scoped_rows() {
        let db_path = temp_path("entity_scoped_delete.sqlite");
        let store = JobStore::open(&db_path).expect("store opens");
        let scoped = NewEntity {
            scope_kind: GlossaryScopeKind::Book,
            scope_id: Some("book-a"),
            source_name: "Gandalf",
            target_name: "Gandalf",
            gender_target: None,
            role: None,
            notes: None,
            source_language: "English",
            target_language: "Italian",
        };
        let other_book = NewEntity {
            scope_id: Some("book-b"),
            ..scoped
        };
        store
            .upsert_entities(std::slice::from_ref(&scoped))
            .expect("a");
        store
            .upsert_entities(std::slice::from_ref(&other_book))
            .expect("b");
        let rows = store.list_entities(None, None, None, None).expect("list");
        assert_eq!(rows.len(), 2);
        let id_a = rows
            .iter()
            .find(|row| row.scope_id.as_deref() == Some("book-a"))
            .expect("row a")
            .id;
        let id_b = rows
            .iter()
            .find(|row| row.scope_id.as_deref() == Some("book-b"))
            .expect("row b")
            .id;

        assert!(store.remove_entity(id_a).expect("remove a"));

        let remaining = store.list_entities(None, None, None, None).expect("list");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].scope_id.as_deref(), Some("book-b"));
        assert_eq!(remaining[0].id, id_b, "sibling id stable");

        let _ = std::fs::remove_file(db_path);
    }
}
