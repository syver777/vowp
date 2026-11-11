//! Repository methods for the `secret_index` table.
//!
//! The table holds metadata only: the name, the last four characters and the two
//! timestamps. Secret values are files owned by `vowp-core` and never enter the database.
//! The first vault implementation kept the same metadata in the global `kv` table under
//! `secret:<NAME>`; [`Store::migrate_secret_kv_to_index`] moves those rows into this table.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §9 and §13.3.

use rusqlite::params;
use vowp_protocol::{SecretInfo, SecretName, Timestamp};

use crate::convert::{corrupt, ts_from_text, ts_to_text};
use crate::db::Store;
use crate::error::StoreError;

/// Prefix of the legacy `kv` rows written by the earlier vault implementation.
const LEGACY_KV_PREFIX: &str = "secret:";

/// Column list shared by every `secret_index` query.
const SECRET_COLUMNS: &str = "name, last4, created_at, updated_at";

impl Store {
    /// Inserts one secret index row, or updates its `last4` and `updated_at`.
    ///
    /// `created_at` is kept from the first insert. The value itself is never stored here.
    pub fn upsert_secret_index(&self, name: &SecretName, last4: &str, now: Timestamp) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO main.secret_index (name, last4, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?3)
             ON CONFLICT(name) DO UPDATE SET
               last4 = excluded.last4,
               updated_at = excluded.updated_at",
            params![name.as_str(), last4, ts_to_text(now)],
        )?;
        Ok(())
    }

    /// Lists every secret index row, ordered by name.
    pub fn list_secret_index(&self) -> Result<Vec<SecretInfo>, StoreError> {
        let mut statement = self
            .conn
            .prepare(&format!("SELECT {SECRET_COLUMNS} FROM main.secret_index ORDER BY name"))?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        let mut infos = Vec::new();
        for row in rows {
            let (name, last4, created_at, updated_at) = row?;
            infos.push(SecretInfo {
                name: SecretName::parse(&name).map_err(|error| corrupt("secret_index.name", error))?,
                last4,
                created_at: ts_from_text(&created_at, "secret_index.created_at")?,
                updated_at: ts_from_text(&updated_at, "secret_index.updated_at")?,
            });
        }
        Ok(infos)
    }

    /// Removes one secret index row, returning whether it existed.
    pub fn remove_secret_index(&self, name: &SecretName) -> Result<bool, StoreError> {
        let changed = self
            .conn
            .execute("DELETE FROM main.secret_index WHERE name = ?1", [name.as_str()])?;
        Ok(changed > 0)
    }

    /// Moves legacy `secret:<NAME>` `kv` metadata into `secret_index`.
    ///
    /// Every row is copied with its recorded timestamps and its `kv` key is deleted in the
    /// same transaction, so an interrupted migration leaves the keys in place for the next
    /// call. Running the migration again is a no-op.
    ///
    /// The vault mode file and the secret files live outside the database and are untouched.
    /// A malformed legacy value is reported as [`StoreError::Corrupt`] and rolls the whole
    /// migration back rather than dropping metadata silently.
    pub fn migrate_secret_kv_to_index(&self) -> Result<u32, StoreError> {
        self.transaction(|tx| {
            let rows = tx.kv_prefix(LEGACY_KV_PREFIX)?;
            let mut migrated = 0_u32;
            for (key, value) in rows {
                let info: SecretInfo =
                    serde_json::from_str(&value).map_err(|error| corrupt(&format!("kv:{key}"), error))?;
                if key != format!("{LEGACY_KV_PREFIX}{}", info.name) {
                    return Err(corrupt(
                        &format!("kv:{key}"),
                        "legacy secret key does not match its stored name",
                    ));
                }
                tx.conn.execute(
                    "INSERT INTO main.secret_index (name, last4, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(name) DO UPDATE SET
                       last4 = excluded.last4,
                       created_at = MIN(secret_index.created_at, excluded.created_at),
                       updated_at = MAX(secret_index.updated_at, excluded.updated_at)",
                    params![
                        info.name.as_str(),
                        info.last4,
                        ts_to_text(info.created_at),
                        ts_to_text(info.updated_at),
                    ],
                )?;
                tx.kv_delete(&key)?;
                migrated += 1;
            }
            Ok(migrated)
        })
    }
}

#[cfg(test)]
mod tests {
    use vowp_protocol::SecretInfo;

    use super::*;

    /// Returns an in-memory global store.
    fn store() -> Store {
        Store::open_memory().expect("in-memory store")
    }

    /// Parses a fixed test instant.
    fn at(text: &str) -> Timestamp {
        Timestamp::parse(text).expect("test timestamp")
    }

    #[test]
    fn upsert_keeps_created_at_and_updates_last4() {
        let store = store();
        let name = SecretName::parse("API_KEY").expect("name");
        let first = at("2026-01-01T00:00:00Z");
        let later = at("2026-01-02T03:04:05Z");

        store.upsert_secret_index(&name, "1111", first).expect("insert");
        store.upsert_secret_index(&name, "2222", later).expect("replace");

        assert_eq!(
            store.list_secret_index().expect("rows"),
            vec![SecretInfo {
                name: name.clone(),
                last4: "2222".to_owned(),
                created_at: first,
                updated_at: later,
            }]
        );
        assert!(store.remove_secret_index(&name).expect("remove"));
        assert!(!store.remove_secret_index(&name).expect("second remove"));
        assert!(store.list_secret_index().expect("empty").is_empty());
    }

    #[test]
    fn migration_copies_legacy_kv_rows_once_and_removes_them() {
        let store = store();
        let name = SecretName::parse("LEGACY_KEY").expect("name");
        let info = SecretInfo {
            name: name.clone(),
            last4: "4f2a".to_owned(),
            created_at: at("2026-01-01T00:00:00Z"),
            updated_at: at("2026-01-02T00:00:00Z"),
        };
        store
            .kv_set("secret:LEGACY_KEY", &serde_json::to_string(&info).expect("metadata"))
            .expect("seed legacy row");
        store.kv_set("unrelated", "kept").expect("seed unrelated row");

        assert_eq!(store.migrate_secret_kv_to_index().expect("first migration"), 1);

        assert_eq!(store.list_secret_index().expect("rows"), vec![info]);
        assert!(
            store.kv_prefix(LEGACY_KV_PREFIX).expect("legacy rows").is_empty(),
            "the migrated kv keys must be removed"
        );
        assert_eq!(
            store.kv_get("unrelated").expect("unrelated row"),
            Some("kept".to_owned()),
            "unrelated kv rows must stay"
        );

        assert_eq!(store.migrate_secret_kv_to_index().expect("second migration"), 0);
        assert_eq!(store.list_secret_index().expect("rows").len(), 1);
    }

    #[test]
    fn migration_merges_with_an_existing_index_row() {
        let store = store();
        let name = SecretName::parse("MERGED_KEY").expect("name");
        store
            .upsert_secret_index(&name, "1111", at("2026-01-02T00:00:00Z"))
            .expect("existing row");
        let legacy = SecretInfo {
            name: name.clone(),
            last4: "2222".to_owned(),
            created_at: at("2025-12-31T00:00:00Z"),
            updated_at: at("2026-01-03T00:00:00Z"),
        };
        store
            .kv_set("secret:MERGED_KEY", &serde_json::to_string(&legacy).expect("metadata"))
            .expect("seed legacy row");

        assert_eq!(store.migrate_secret_kv_to_index().expect("migration"), 1);

        assert_eq!(
            store.list_secret_index().expect("rows"),
            vec![SecretInfo {
                name,
                last4: "2222".to_owned(),
                created_at: at("2025-12-31T00:00:00Z"),
                updated_at: at("2026-01-03T00:00:00Z"),
            }]
        );
    }

    #[test]
    fn migration_rolls_back_on_a_malformed_legacy_row() {
        let store = store();
        let valid = SecretInfo {
            name: SecretName::parse("VALID_KEY").expect("name"),
            last4: "9c1d".to_owned(),
            created_at: at("2026-01-01T00:00:00Z"),
            updated_at: at("2026-01-01T00:00:00Z"),
        };
        store
            .kv_set("secret:VALID_KEY", &serde_json::to_string(&valid).expect("metadata"))
            .expect("seed valid row");
        // Sorts after VALID_KEY, so the transaction inserts one row before it fails.
        store
            .kv_set("secret:ZZZ_BROKEN_KEY", "{not json")
            .expect("seed broken row");

        let error = store.migrate_secret_kv_to_index().expect_err("malformed row");
        assert!(error.to_string().contains("corrupt"), "{error}");
        assert!(
            store.list_secret_index().expect("rows").is_empty(),
            "the transaction must roll back the valid row too"
        );
        assert_eq!(
            store.kv_prefix(LEGACY_KV_PREFIX).expect("legacy rows").len(),
            2,
            "a failed migration keeps every key for a later retry"
        );
    }
}
