// mdview — internal document database (SQLite).
//
// Stores every saved document as a DEFLATE-compressed blob plus the metadata
// the library needs: an FTS5 trigram index for fuzzy title/content search and
// a 64-bit SimHash signature for near-duplicate detection. The database lives
// at %APPDATA%\mdview\documents.db, next to prefs.json.
//
// Design rules:
// - The canonical content is always the compressed blob. The FTS5 table is a
//   derived search index, not a second canonical copy.
// - Identity is never inferred from content alone. `find_similar` only returns
//   candidates; the caller decides whether to offer them. Two different files
//   with identical content stay two documents unless they share a source.
// - All SQL is parameterized. User content is never interpolated into SQL.
// - The schema is versioned with `PRAGMA user_version`; migrations are additive.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use crate::{truncate_str, MAX_MD_BYTES};

pub const DB_VERSION: i64 = 1;
pub const MAX_TITLE_LEN: usize = 200;
pub const MAX_SEARCH_LIMIT: usize = 50;
pub const DEFAULT_SEARCH_LIMIT: usize = 20;
pub const DEFAULT_RECENT_LIMIT: usize = 8;
/// Hamming distance at or below which two SimHashes count as "same-ish".
/// 12/64 keeps unrelated prose apart while tolerating light edits.
pub const SIMHASH_THRESHOLD: u32 = 12;
/// Below this many words a SimHash is mostly noise; exact-hash matching still
/// runs, but fuzzy matching does not.
pub const SIMHASH_MIN_WORDS: usize = 12;
pub const PREVIEW_LEN: usize = 200;

#[derive(Debug, Clone, Serialize)]
pub struct Document {
    pub id: i64,
    pub title: String,
    pub text: String,
    pub base: String,
    pub source_kind: String,
    pub source_ref: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub open_count: i64,
}

/// What the save button / auto-save hook need back: no document body.
#[derive(Debug, Clone, Serialize)]
pub struct SavedDoc {
    pub id: i64,
    pub title: String,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecentDoc {
    pub id: i64,
    pub title: String,
    pub preview: String,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    pub id: i64,
    pub title: String,
    pub snippet: String,
    pub updated_at: i64,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SimilarMatch {
    pub id: i64,
    pub title: String,
    pub updated_at: i64,
    pub source_kind: String,
    pub source_ref: String,
    /// 0.0..1.0. Exact hash matches report 1.0.
    pub score: f64,
    /// True when the stored content is byte-for-byte identical. The caller may
    /// treat that as "already saved" rather than offering a replacement.
    pub exact: bool,
}

#[derive(Debug)]
pub struct Store {
    conn: Connection,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS documents (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  title TEXT NOT NULL,
  content_blob BLOB NOT NULL,
  content_size INTEGER NOT NULL,
  content_hash INTEGER NOT NULL,
  simhash INTEGER NOT NULL,
  word_count INTEGER NOT NULL,
  preview TEXT NOT NULL DEFAULT '',
  base TEXT NOT NULL DEFAULT '',
  source_kind TEXT NOT NULL DEFAULT 'paste',
  source_ref TEXT NOT NULL DEFAULT '',
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  opened_at INTEGER NOT NULL,
  open_count INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_documents_updated ON documents(updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_documents_hash ON documents(content_hash);
CREATE INDEX IF NOT EXISTS idx_documents_source ON documents(source_kind, source_ref);
-- find_similar only scans rows long enough to have a stable SimHash.
CREATE INDEX IF NOT EXISTS idx_documents_words ON documents(word_count);
-- Regular (stored) FTS5 table: the canonical blob stays compressed, this is
-- the search index. Trigram tokenization gives substring and near-miss
-- matching, which word tokenizers cannot.
CREATE VIRTUAL TABLE IF NOT EXISTS documents_fts USING fts5(
  title, content, tokenize='trigram'
);
-- Reserved for the future highlight/annotation feature. The auto-save hook
-- guarantees a documents row exists before any annotation references it.
CREATE TABLE IF NOT EXISTS annotations (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  document_id INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
  anchor TEXT NOT NULL,
  color TEXT NOT NULL DEFAULT 'yellow',
  note TEXT NOT NULL DEFAULT '',
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_annotations_doc ON annotations(document_id);
"#;

impl Store {
    pub fn open(path: &Path) -> rusqlite::Result<Store> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let conn = Connection::open(path)?;
        let store = Store { conn };
        store.configure()?;
        store.migrate()?;
        Ok(store)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> rusqlite::Result<Store> {
        let conn = Connection::open_in_memory()?;
        let store = Store { conn };
        store.configure()?;
        store.migrate()?;
        Ok(store)
    }

    fn configure(&self) -> rusqlite::Result<()> {
        // WAL keeps reads off the writer's back; NORMAL is durable enough for a
        // local library and much faster than FULL. busy_timeout prevents
        // "database is locked" when a search lands during a save.
        let _ = self.conn.pragma_update(None, "journal_mode", "WAL");
        let _ = self.conn.pragma_update(None, "synchronous", "NORMAL");
        let _ = self.conn.pragma_update(None, "busy_timeout", 5000i64);
        let _ = self.conn.pragma_update(None, "foreign_keys", true);
        Ok(())
    }

    fn migrate(&self) -> rusqlite::Result<()> {
        let version: i64 = self.conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version > DB_VERSION {
            return Err(db_err(format!(
                "database version {version} is newer than this app ({DB_VERSION})"
            )));
        }
        if version < 1 {
            self.conn.execute_batch(SCHEMA)?;
            self.conn
                .execute_batch(&format!("PRAGMA user_version = {DB_VERSION};"))?;
        }
        // Future migrations: each gated on `version < N`, then bump DB_VERSION.
        Ok(())
    }

    /// Insert or update a document. `id` wins; otherwise a non-empty
    /// `source_ref` updates the row that was saved from the same file/URL.
    /// Identical content from a different source is deliberately a new row.
    pub fn save(
        &self,
        id: Option<i64>,
        title: &str,
        text: &str,
        base: &str,
        source_kind: &str,
        source_ref: &str,
    ) -> rusqlite::Result<SavedDoc> {
        self.save_internal(id, title, text, base, source_kind, source_ref, now())
    }

    /// The annotation path: make sure a document row exists for this content
    /// and return its id. Same source -> update; otherwise a fresh row.
    pub fn auto_save(
        &self,
        title: &str,
        text: &str,
        base: &str,
        source_kind: &str,
        source_ref: &str,
    ) -> rusqlite::Result<SavedDoc> {
        self.save(None, title, text, base, source_kind, source_ref)
    }

    fn save_internal(
        &self,
        id: Option<i64>,
        title: &str,
        text: &str,
        base: &str,
        source_kind: &str,
        source_ref: &str,
        now: i64,
    ) -> rusqlite::Result<SavedDoc> {
        if text.len() > MAX_MD_BYTES {
            return Err(db_err(format!(
                "document is larger than {} MB",
                MAX_MD_BYTES / 1024 / 1024
            )));
        }
        let title = sanitize_title(title).to_string();
        let blob = compress(text.as_bytes());
        let hash = fnv1a64(text.as_bytes());
        let sh = simhash(text);
        let words = word_count(text) as i64;
        let preview = make_preview(text);
        let kind = sanitize_source_kind(source_kind);
        let reference = source_ref.to_string();

        let tx = self.conn.unchecked_transaction()?;

        // Resolve the target row: explicit id, then same source, then insert.
        let existing = if let Some(want) = id {
            let found: i64 = tx
                .query_row("SELECT id FROM documents WHERE id = ?1", params![want], |r| r.get(0))
                .optional()?
                .ok_or_else(|| db_err("document not found"))?;
            Some(found)
        } else if !reference.is_empty() {
            tx.query_row(
                "SELECT id FROM documents WHERE source_kind = ?1 AND source_ref = ?2 LIMIT 1",
                params![kind, reference],
                |r| r.get(0),
            )
            .optional()?
        } else {
            // Ephemeral content (no source): reuse an existing *unsourced* row
            // of identical content, so repeated auto-saves (e.g. from
            // annotations) update one row instead of spawning duplicates. A
            // saved file/URL row is never reused here, and vice-versa.
            tx.query_row(
                "SELECT id FROM documents WHERE source_kind = 'paste' AND content_hash = ?1 AND content_size = ?2 ORDER BY id LIMIT 1",
                params![hash as i64, text.len() as i64],
                |r| r.get(0),
            )
            .optional()?
        };

        let id = match existing {
            Some(id) => {
                // Auto-save fires on every annotation; skip the write and the
                // FTS rebuild when nothing observable changed.
                let current = tx.query_row(
                    "SELECT content_hash, content_size, title, base, source_kind, source_ref, updated_at
                     FROM documents WHERE id = ?1",
                    params![id],
                    |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, i64>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, String>(3)?,
                            r.get::<_, String>(4)?,
                            r.get::<_, String>(5)?,
                            r.get::<_, i64>(6)?,
                        ))
                    },
                )?;
                let (cur_hash, cur_size, cur_title, cur_base, cur_kind, cur_ref, cur_updated) =
                    current;
                if cur_hash == hash as i64
                    && cur_size == text.len() as i64
                    && cur_title == title
                    && cur_base == base
                    && cur_kind == kind
                    && cur_ref == reference
                {
                    return Ok(SavedDoc { id, title, updated_at: cur_updated });
                }
                // Preserves created_at and open_count; refreshes everything the
                // user or the index can observe.
                tx.execute(
                    "UPDATE documents SET title=?1, content_blob=?2, content_size=?3, content_hash=?4, simhash=?5, word_count=?6, preview=?7, base=?8, source_kind=?9, source_ref=?10, updated_at=?11 WHERE id=?12",
                    params![title, blob, text.len() as i64, hash as i64, sh as i64, words, preview, base, kind, reference, now, id],
                )?;
                id
            }
            None => {
                tx.execute(
                    "INSERT INTO documents (title, content_blob, content_size, content_hash, simhash, word_count, preview, base, source_kind, source_ref, created_at, updated_at, opened_at, open_count)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11, ?11, 0)",
                    params![title, blob, text.len() as i64, hash as i64, sh as i64, words, preview, base, kind, reference, now],
                )?;
                tx.last_insert_rowid()
            }
        };

        // Rebuild this row's FTS entry. A regular FTS5 table has no UPDATE, so
        // delete + insert is the documented way to refresh it.
        tx.execute("DELETE FROM documents_fts WHERE rowid = ?1", params![id])?;
        tx.execute(
            "INSERT INTO documents_fts(rowid, title, content) VALUES (?1, ?2, ?3)",
            params![id, title, text],
        )?;

        tx.commit()?;

        Ok(SavedDoc { id, title, updated_at: now })
    }

    pub fn open_document(&self, id: i64) -> rusqlite::Result<Option<Document>> {
        let row = self
            .conn
            .query_row(
                "SELECT id, title, content_blob, content_size, base, source_kind, source_ref, created_at, updated_at, open_count
                 FROM documents WHERE id = ?1",
                params![id],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Vec<u8>>(2)?,
                        r.get::<_, i64>(3)? as usize,
                        r.get::<_, String>(4)?,
                        r.get::<_, String>(5)?,
                        r.get::<_, String>(6)?,
                        r.get::<_, i64>(7)?,
                        r.get::<_, i64>(8)?,
                        r.get::<_, i64>(9)?,
                    ))
                },
            )
            .optional()?;
        let Some((id, title, blob, size, base, kind, reference, created_at, updated_at, open_count)) =
            row
        else {
            return Ok(None);
        };
        let text = decompress(&blob, size).map_err(db_err)?;
        let now = now();
        self.conn.execute(
            "UPDATE documents SET opened_at=?1, open_count=open_count+1 WHERE id=?2",
            params![now, id],
        )?;
        Ok(Some(Document {
            id,
            title,
            text,
            base,
            source_kind: kind,
            source_ref: reference,
            created_at,
            updated_at,
            open_count: open_count + 1,
        }))
    }

    /// Decompress a stored row without touching open stats (used by rename).
    fn fetch_text(&self, id: i64) -> rusqlite::Result<Option<String>> {
        let row = self
            .conn
            .query_row(
                "SELECT content_blob, content_size FROM documents WHERE id = ?1",
                params![id],
                |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i64>(1)? as usize)),
            )
            .optional()?;
        match row {
            Some((blob, size)) => decompress(&blob, size).map(Some).map_err(db_err),
            None => Ok(None),
        }
    }

    pub fn search(&self, query: &str, limit: usize) -> rusqlite::Result<Vec<SearchHit>> {
        let q = query.trim();
        if q.is_empty() {
            return Ok(Vec::new());
        }
        let limit = limit.clamp(1, MAX_SEARCH_LIMIT) as i64;
        let mut out = Vec::new();

        if q.chars().count() < 3 {
            // The trigram tokenizer needs at least three characters. Fall back
            // to a case-insensitive substring scan over the indexed text; the
            // database is local and small, so this stays instant.
            let mut stmt = self.conn.prepare(
                "SELECT d.id, d.title, snippet(documents_fts, 1, '', '', '…', 40), d.updated_at, 0.0
                 FROM documents_fts JOIN documents d ON d.id = documents_fts.rowid
                 WHERE instr(lower(documents_fts.title), lower(?1)) > 0
                    OR instr(lower(documents_fts.content), lower(?1)) > 0
                 ORDER BY d.updated_at DESC LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![q, limit], |r| {
                Ok(SearchHit {
                    id: r.get(0)?,
                    title: r.get(1)?,
                    snippet: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    updated_at: r.get(3)?,
                    score: r.get(4)?,
                })
            })?;
            for row in rows {
                out.push(row?);
            }
        } else {
            // Wrap the whole query as a phrase and double any embedded quotes:
            // this makes FTS5 treat the user's input as a literal substring
            // instead of query syntax.
            let fts_query = format!("\"{}\"", q.replace('"', "\"\""));
            let mut stmt = self.conn.prepare(
                "SELECT d.id, d.title, snippet(documents_fts, 1, '', '', '…', 40), d.updated_at, bm25(documents_fts, 5.0, 1.0)
                 FROM documents_fts JOIN documents d ON d.id = documents_fts.rowid
                 WHERE documents_fts MATCH ?1
                 ORDER BY bm25(documents_fts, 5.0, 1.0) ASC LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![fts_query, limit], |r| {
                Ok(SearchHit {
                    id: r.get(0)?,
                    title: r.get(1)?,
                    snippet: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    updated_at: r.get(3)?,
                    score: r.get(4)?,
                })
            })?;
            for row in rows {
                out.push(row?);
            }
        }
        Ok(out)
    }

    pub fn recent(&self, limit: usize) -> rusqlite::Result<Vec<RecentDoc>> {
        let limit = limit.clamp(1, MAX_SEARCH_LIMIT) as i64;
        let mut stmt = self.conn.prepare(
            "SELECT id, title, preview, updated_at FROM documents ORDER BY updated_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |r| {
            Ok(RecentDoc {
                id: r.get(0)?,
                title: r.get(1)?,
                preview: r.get(2)?,
                updated_at: r.get(3)?,
            })
        })?;
        rows.collect()
    }

    pub fn rename(&self, id: i64, title: &str) -> rusqlite::Result<bool> {
        let title = sanitize_title(title).to_string();
        let now = now();
        let n = self.conn.execute(
            "UPDATE documents SET title=?1, updated_at=?2 WHERE id=?3",
            params![title, now, id],
        )?;
        if n == 0 {
            return Ok(false);
        }
        let text = self
            .fetch_text(id)?
            .ok_or_else(|| db_err("document not found"))?;
        self.conn
            .execute("DELETE FROM documents_fts WHERE rowid=?1", params![id])?;
        self.conn.execute(
            "INSERT INTO documents_fts(rowid, title, content) VALUES (?1, ?2, ?3)",
            params![id, title, text],
        )?;
        Ok(true)
    }

    /// Delete a saved document and its search-index entry. Annotations rows
    /// cascade via the foreign key (foreign_keys is enabled per connection).
    /// Returns false when the id does not exist — deleting is idempotent.
    pub fn delete(&self, id: i64) -> rusqlite::Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM documents WHERE id = ?1", params![id])?;
        self.conn
            .execute("DELETE FROM documents_fts WHERE rowid = ?1", params![id])?;
        Ok(n > 0)
    }

    /// Find a stored document that may be the same logical document as `text`.
    ///
    /// Exact content-hash matches are returned first. Otherwise a SimHash
    /// Hamming-distance scan runs over documents long enough to have a stable
    /// signature. The result is a *candidate*: the caller must offer it, never
    /// silently replace anything.
    pub fn find_similar(
        &self,
        text: &str,
        source_ref: &str,
    ) -> rusqlite::Result<Option<SimilarMatch>> {
        if text.is_empty() {
            return Ok(None);
        }
        let hash = fnv1a64(text.as_bytes());
        let exact = self
            .conn
            .query_row(
                "SELECT id, title, updated_at, source_kind, source_ref FROM documents
                 WHERE content_hash=?1 AND content_size=?2
                 ORDER BY CASE WHEN source_ref = ?3 THEN 0 ELSE 1 END
                 LIMIT 1",
                params![hash as i64, text.len() as i64, source_ref],
                |r| {
                    Ok(SimilarMatch {
                        id: r.get(0)?,
                        title: r.get(1)?,
                        updated_at: r.get(2)?,
                        source_kind: r.get(3)?,
                        source_ref: r.get(4)?,
                        score: 1.0,
                        exact: true,
                    })
                },
            )
            .optional()?;
        if exact.is_some() {
            return Ok(exact);
        }

        let words = word_count(text);
        if words < SIMHASH_MIN_WORDS {
            return Ok(None);
        }
        let sh = simhash(text);

        // Fast path: the same file/URL is the strongest identity signal. If it
        // is already a near match, return it without scanning the library.
        if !source_ref.is_empty() {
            let same = self
                .conn
                .query_row(
                    "SELECT id, title, simhash, updated_at, source_kind, source_ref FROM documents
                     WHERE source_ref = ?1 LIMIT 1",
                    params![source_ref],
                    |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, i64>(2)? as u64,
                            r.get::<_, i64>(3)?,
                            r.get::<_, String>(4)?,
                            r.get::<_, String>(5)?,
                        ))
                    },
                )
                .optional()?;
            if let Some((id, title, doc_hash, updated_at, kind, reference)) = same {
                let dist = (sh ^ doc_hash).count_ones();
                if dist <= SIMHASH_THRESHOLD {
                    return Ok(Some(SimilarMatch {
                        id,
                        title,
                        updated_at,
                        source_kind: kind,
                        source_ref: reference,
                        score: 1.0 - (dist as f64 / 64.0),
                        exact: false,
                    }));
                }
            }
        }

        let mut stmt = self.conn.prepare(
            "SELECT id, title, simhash, updated_at, source_kind, source_ref FROM documents WHERE word_count >= ?1",
        )?;
        let rows = stmt.query_map(params![SIMHASH_MIN_WORDS as i64], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)? as u64,
                r.get::<_, i64>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
            ))
        })?;

        let mut best: Option<SimilarMatch> = None;
        let mut best_same_source = false;
        for row in rows {
            let (id, title, doc_hash, updated_at, kind, doc_ref) = row?;
            let dist = (sh ^ doc_hash).count_ones();
            if dist > SIMHASH_THRESHOLD {
                continue;
            }
            let same_source = !source_ref.is_empty() && doc_ref == source_ref;
            let score = 1.0 - (dist as f64 / 64.0);
            let replace = match &best {
                None => true,
                Some(b) => {
                    (same_source && !best_same_source)
                        || (same_source == best_same_source && score > b.score)
                }
            };
            if replace {
                best = Some(SimilarMatch {
                    id,
                    title,
                    updated_at,
                    source_kind: kind,
                    source_ref: doc_ref,
                    score,
                    exact: false,
                });
                best_same_source = same_source;
            }
        }
        Ok(best)
    }
}

/// Process-wide store, opened lazily on first use. If the on-disk database
/// cannot be opened, every DB command returns the stored error instead of
/// taking the viewer down with it.
pub fn store() -> Result<&'static Mutex<Store>, String> {
    static STORE: OnceLock<Result<Mutex<Store>, String>> = OnceLock::new();
    let cell = STORE.get_or_init(|| {
        Store::open(&db_path())
            .map(Mutex::new)
            .map_err(|e| format!("document database unavailable: {e}"))
    });
    match cell {
        Ok(m) => Ok(m),
        Err(e) => Err(e.clone()),
    }
}

pub fn db_path() -> PathBuf {
    let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    PathBuf::from(base).join("mdview").join("documents.db")
}

/* ---------------- helpers ---------------- */

fn db_err(msg: impl Into<String>) -> rusqlite::Error {
    rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
        Some(msg.into()),
    )
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn sanitize_title(title: &str) -> &str {
    let t = title.trim();
    if t.is_empty() {
        "untitled"
    } else {
        truncate_str(t, MAX_TITLE_LEN)
    }
}

fn sanitize_source_kind(kind: &str) -> &str {
    match kind {
        "file" | "url" | "paste" => kind,
        _ => "paste",
    }
}

fn compress(data: &[u8]) -> Vec<u8> {
    miniz_oxide::deflate::compress_to_vec(data, 6)
}

fn decompress(blob: &[u8], expected_size: usize) -> Result<String, String> {
    let out = miniz_oxide::inflate::decompress_to_vec(blob)
        .map_err(|e| format!("corrupt document blob: {e}"))?;
    if out.len() != expected_size {
        return Err(format!(
            "document size mismatch: expected {expected_size}, got {}",
            out.len()
        ));
    }
    String::from_utf8(out).map_err(|_| "document is not valid UTF-8".to_string())
}

fn make_preview(text: &str) -> String {
    let mut out = String::new();
    let mut lines = 0;
    for line in text.lines() {
        if lines >= 50 {
            break;
        }
        lines += 1;
        let l = line.trim();
        if l.is_empty() {
            continue;
        }
        // A leading heading marker is noise in a one-line preview.
        let l = l.trim_start_matches('#').trim_start();
        if l.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(l);
        if out.chars().count() >= PREVIEW_LEN {
            break;
        }
    }
    truncate_str(&out, PREVIEW_LEN).to_string()
}

fn word_count(text: &str) -> usize {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .count()
}

/// FNV-1a, 64-bit. Stable across Rust versions (unlike DefaultHasher), so a
/// database written by one build stays comparable after an update.
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn hash_shingle(w1: &str, w2: &str, w3: &str) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    let feed = |bytes: &[u8], h: &mut u64| {
        for &b in bytes {
            // ASCII case folding keeps hashes stable for "Word" vs "word"
            // without allocating a lowercased String per token.
            let c = if b.is_ascii_uppercase() { b + 32 } else { b };
            *h ^= c as u64;
            *h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    feed(w1.as_bytes(), &mut h);
    h ^= 0x20;
    h = h.wrapping_mul(0x0000_0100_0000_01b3);
    feed(w2.as_bytes(), &mut h);
    h ^= 0x20;
    h = h.wrapping_mul(0x0000_0100_0000_01b3);
    feed(w3.as_bytes(), &mut h);
    h
}

fn add_hash(v: &mut [i32; 64], h: u64) {
    for b in 0..64 {
        if (h >> b) & 1 == 1 {
            v[b] += 1;
        } else {
            v[b] -= 1;
        }
    }
}

/// 64-bit SimHash over lowercased word 3-grams (unigrams for very short text).
/// Two documents with a small Hamming distance share enough phrasing to be
/// worth offering to the user as a possible match.
pub fn simhash(text: &str) -> u64 {
    let words: Vec<&str> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    if words.is_empty() {
        return 0;
    }
    let mut v = [0i32; 64];
    if words.len() >= 3 {
        for i in 0..words.len() - 2 {
            add_hash(&mut v, hash_shingle(words[i], words[i + 1], words[i + 2]));
        }
    } else {
        for w in &words {
            add_hash(&mut v, hash_shingle(w, "", ""));
        }
    }
    let mut out = 0u64;
    for b in 0..64 {
        if v[b] > 0 {
            out |= 1 << b;
        }
    }
    out
}

/* ---------------- tests ---------------- */

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open_in_memory().unwrap()
    }

    fn save(
        s: &Store,
        title: &str,
        text: &str,
        source_ref: &str,
    ) -> SavedDoc {
        s.save(None, title, text, "", "paste", source_ref).unwrap()
    }

    #[test]
    fn save_and_open_roundtrip() {
        let s = store();
        let doc = s
            .save(
                None,
                "Notes",
                "# Hello\n\nbody text",
                "C:/notes",
                "file",
                "C:/notes/a.md",
            )
            .unwrap();
        assert!(doc.id > 0);
        assert_eq!(doc.title, "Notes");

        let opened = s.open_document(doc.id).unwrap().expect("doc exists");
        assert_eq!(opened.title, "Notes");
        assert_eq!(opened.text, "# Hello\n\nbody text");
        assert_eq!(opened.base, "C:/notes");
        assert_eq!(opened.source_kind, "file");
        assert_eq!(opened.source_ref, "C:/notes/a.md");
        assert_eq!(opened.open_count, 1);
        // Opening again bumps the counter.
        let again = s.open_document(doc.id).unwrap().unwrap();
        assert_eq!(again.open_count, 2);
    }

    #[test]
    fn compression_roundtrip_edge_cases() {
        let s = store();
        for text in ["", "a", "héllo 世界", &"x".repeat(1024 * 1024)] {
            let doc = save(&s, "t", text, "");
            let opened = s.open_document(doc.id).unwrap().unwrap();
            assert_eq!(opened.text, text, "roundtrip failed for len {}", text.len());
        }
    }

    #[test]
    fn save_by_id_updates_not_duplicates() {
        let s = store();
        let a = save(&s, "A", "first", "");
        let b = s
            .save(Some(a.id), "A2", "second", "", "paste", "")
            .unwrap();
        assert_eq!(a.id, b.id);
        let recent = s.recent(10).unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].title, "A2");
        assert_eq!(s.open_document(a.id).unwrap().unwrap().text, "second");
    }

    #[test]
    fn save_matches_source_ref() {
        let s = store();
        let a = s
            .save(None, "A", "first", "C:/notes", "file", "C:/notes/a.md")
            .unwrap();
        let b = s
            .save(None, "A", "second", "C:/notes", "file", "C:/notes/a.md")
            .unwrap();
        assert_eq!(a.id, b.id, "same source must update, not duplicate");
        assert_eq!(s.recent(10).unwrap().len(), 1);
    }

    #[test]
    fn identical_content_different_source_is_not_merged() {
        let s = store();
        let a = s
            .save(None, "A", "identical body", "C:/a", "file", "C:/a/a.md")
            .unwrap();
        let b = s
            .save(None, "B", "identical body", "C:/b", "file", "C:/b/b.md")
            .unwrap();
        assert_ne!(a.id, b.id, "different sources must stay separate");
        assert_eq!(s.recent(10).unwrap().len(), 2);
    }

    #[test]
    fn search_finds_title_and_content() {
        let s = store();
        save(&s, "Quarterly report", "revenue grew steadily", "");
        save(&s, "Random", "the quarterly report is due", "");
        let hits = s.search("quarterly report", 10).unwrap();
        assert_eq!(hits.len(), 2, "both title and content matches");
        // The title match should outrank the body-only match.
        assert_eq!(hits[0].title, "Quarterly report");
        let body = s.search("revenue grew", 10).unwrap();
        assert_eq!(body.len(), 1);
        assert!(body[0].snippet.contains("revenue"));
    }

    #[test]
    fn search_is_fuzzy_substring() {
        let s = store();
        save(&s, "Field notes", "the quick brown fox jumps over the lazy dog", "");
        // Substring across a word boundary, and a partial word.
        assert_eq!(s.search("brown fox", 10).unwrap().len(), 1);
        assert_eq!(s.search("quick br", 10).unwrap().len(), 1);
        // A shared trigram phrase that is not present must not match.
        assert_eq!(s.search("purple elephant", 10).unwrap().len(), 0);
    }

    #[test]
    fn search_handles_short_query_fallback() {
        let s = store();
        save(&s, "ab", "xxabxx", "");
        let hits = s.search("ab", 10).unwrap();
        assert_eq!(hits.len(), 1, "2-char query uses the LIKE/instr fallback");
        assert_eq!(s.search("a", 10).unwrap().len(), 1);
    }

    #[test]
    fn search_is_case_insensitive_and_unicode() {
        let s = store();
        save(&s, "Café notes", "naïve café résumé", "");
        assert_eq!(s.search("café", 10).unwrap().len(), 1);
        assert_eq!(s.search("CAFÉ", 10).unwrap().len(), 1);
        assert_eq!(s.search("résumé", 10).unwrap().len(), 1);
        assert_eq!(s.search("missing", 10).unwrap().len(), 0);
    }

    #[test]
    fn recent_orders_by_updated_desc_and_limits() {
        let s = store();
        let a = s.save_internal(None, "A", "a", "", "paste", "", 1000).unwrap();
        let b = s.save_internal(None, "B", "b", "", "paste", "", 2000).unwrap();
        let c = s.save_internal(None, "C", "c", "", "paste", "", 3000).unwrap();
        let recent = s.recent(2).unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].id, c.id);
        assert_eq!(recent[1].id, b.id);
        assert!(recent.iter().all(|d| d.id != a.id));
    }

    #[test]
    fn rename_updates_title_and_fts() {
        let s = store();
        let doc = save(&s, "Old title", "unique content zebra", "");
        assert!(s.rename(doc.id, "New title").unwrap());
        assert_eq!(s.search("new title", 10).unwrap().len(), 1);
        assert_eq!(s.search("old title", 10).unwrap().len(), 0);
        // Content search still works after the FTS rebuild.
        assert_eq!(s.search("zebra", 10).unwrap().len(), 1);
        // Renaming a missing document is a no-op, not an error.
        assert!(!s.rename(99999, "nobody").unwrap());
    }

    #[test]
    fn find_similar_exact_hash() {
        let s = store();
        let doc = save(&s, "Original", "the same body text", "");
        let m = s.find_similar("the same body text", "").unwrap().unwrap();
        assert_eq!(m.id, doc.id);
        assert!(m.exact);
        assert_eq!(m.score, 1.0);
    }

    #[test]
    fn find_similar_near_duplicate() {
        let s = store();
        let original = "\
            The quick brown fox jumps over the lazy dog. Quick brown foxes are \
            a common example in typography and testing. The lazy dog, for its \
            part, rarely moves at all. This sentence exists to give the \
            document enough phrasing for a stable signature.";
        let doc = save(&s, "Original", original, "");
        // A light edit: a few words changed, most phrasing intact.
        let edited = original.replace("quick brown fox", "fast red fox").replace("rarely moves", "seldom moves");
        let m = s.find_similar(&edited, "").unwrap().expect("should match");
        assert_eq!(m.id, doc.id);
        assert!(!m.exact);
        assert!(m.score > 0.8, "score {}", m.score);
    }

    #[test]
    fn find_similar_ignores_unrelated_and_short() {
        let s = store();
        save(
            &s,
            "Cooking",
            "simmer the sauce and toast the bread until the cheese melts",
            "",
        );
        assert!(s
            .find_similar(
                "quantum entanglement challenges our classical intuition about locality",
                ""
            )
            .unwrap()
            .is_none());
        // Short text never fuzzy-matches (exact hash aside).
        assert!(s.find_similar("simmer the sauce", "").unwrap().is_none());
    }

    #[test]
    fn find_similar_prefers_same_source() {
        let s = store();
        let body = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu";
        let a = s
            .save(None, "A", body, "C:/a", "file", "C:/a/a.md")
            .unwrap();
        let b = s
            .save(None, "B", body, "C:/b", "file", "C:/b/b.md")
            .unwrap();
        let m = s.find_similar(body, "C:/b/b.md").unwrap().unwrap();
        assert_eq!(m.id, b.id, "same source must win over an identical other row");
        let _ = a;
    }

    #[test]
    fn auto_save_is_idempotent_by_source() {
        let s = store();
        let a = s
            .auto_save("Notes", "first", "C:/n", "file", "C:/n/a.md")
            .unwrap();
        let b = s
            .auto_save("Notes", "second", "C:/n", "file", "C:/n/a.md")
            .unwrap();
        assert_eq!(a.id, b.id);
        assert_eq!(s.recent(10).unwrap().len(), 1);
        assert_eq!(s.open_document(a.id).unwrap().unwrap().text, "second");
    }

    #[test]
    fn auto_save_paste_reuses_identical_row() {
        // Unsourced (pasted) content auto-saves on every annotation; repeat
        // calls must land on one row, not spawn duplicates.
        let s = store();
        let a = s.auto_save("Notes", "identical body paste", "", "paste", "").unwrap();
        let b = s.auto_save("Notes", "identical body paste", "", "paste", "").unwrap();
        assert_eq!(a.id, b.id);
        assert_eq!(s.recent(10).unwrap().len(), 1);
    }

    #[test]
    fn paste_auto_save_never_reuses_file_rows() {
        // Identical markdown saved as a file must stay a different document
        // from a paste of the same text — no silent cross-source attach.
        let s = store();
        let f = s
            .save(None, "File", "identical body paste", "C:/n", "file", "C:/n/f.md")
            .unwrap();
        let p = s.auto_save("Notes", "identical body paste", "", "paste", "").unwrap();
        assert_ne!(f.id, p.id, "a paste must not attach to a saved file row");
        assert_eq!(s.recent(10).unwrap().len(), 2);
    }

    #[test]
    fn delete_removes_document_and_fts_entry() {
        let s = store();
        let doc = save(&s, "Doomed", "unique marker zorblax", "");
        assert_eq!(s.search("zorblax", 10).unwrap().len(), 1);
        assert!(s.delete(doc.id).unwrap());
        // Gone from every read path, and FTS does not hold it either.
        assert!(s.open_document(doc.id).unwrap().is_none());
        assert_eq!(s.search("zorblax", 10).unwrap().len(), 0);
        assert!(s.recent(10).unwrap().is_empty());
        // Deleting the same id again is a no-op, not an error.
        assert!(!s.delete(doc.id).unwrap());
    }

    #[test]
    fn delete_cascades_to_annotations() {
        let s = store();
        let doc = save(&s, "Doomed", "body", "");
        s.conn
            .execute(
                "INSERT INTO annotations (document_id, anchor, color, note, created_at, updated_at) VALUES (?1, '{}', 'yellow', 'n', 1, 1)",
                params![doc.id],
            )
            .unwrap();
        assert!(s.delete(doc.id).unwrap());
        let n: i64 = s
            .conn
            .query_row("SELECT count(*) FROM annotations WHERE document_id = ?1", params![doc.id], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn corrupt_blob_is_an_error() {
        let s = store();
        let doc = save(&s, "Good", "payload", "");
        s.conn
            .execute(
                "UPDATE documents SET content_blob = ?1, content_size = 7 WHERE id = ?2",
                params![b"garbage".to_vec(), doc.id],
            )
            .unwrap();
        let err = s.open_document(doc.id).unwrap_err().to_string();
        assert!(err.contains("corrupt") || err.contains("size mismatch"), "{err}");
    }

    #[test]
    fn oversize_content_is_rejected() {
        let s = store();
        let big = "x".repeat(MAX_MD_BYTES + 1);
        let err = s
            .save(None, "Big", &big, "", "paste", "")
            .unwrap_err()
            .to_string();
        assert!(err.contains("larger than"), "{err}");
        assert!(s.recent(10).unwrap().is_empty());
    }

    #[test]
    fn empty_title_defaults_and_preview_is_bounded() {
        let s = store();
        let doc = save(&s, "   ", &"# Heading\n\nbody line\n".repeat(100), "");
        assert_eq!(doc.title, "untitled");
        let recent = s.recent(10).unwrap();
        assert_eq!(recent[0].title, "untitled");
        assert!(recent[0].preview.len() <= PREVIEW_LEN);
        assert!(!recent[0].preview.starts_with('#'));
    }

    #[test]
    fn migration_from_v0_creates_schema() {
        let dir = std::env::temp_dir().join(format!("mdview-migrate-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("docs.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA user_version = 0;").unwrap();
        }
        let s = Store::open(&path).unwrap();
        let doc = save(&s, "Migrated", "body", "");
        assert!(s.open_document(doc.id).unwrap().is_some());
        let v: i64 = s
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, DB_VERSION);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn future_db_version_is_rejected() {
        let dir = std::env::temp_dir().join(format!("mdview-future-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("docs.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA user_version = 999;").unwrap();
        }
        let err = Store::open(&path).unwrap_err().to_string();
        assert!(err.contains("newer than this app"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fts5_trigram_available() {
        let s = store();
        save(&s, "smoke", "hello world", "");
        assert_eq!(s.search("ell", 10).unwrap().len(), 1);
    }
}
