//! SQLite storage (rusqlite, WAL, external-content FTS5). All queries live
//! here; the rest of the app passes plain data types in and out.

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::path::Path;
use std::sync::Mutex;

/// Match markers used in FTS snippets; the frontend converts them to <mark>
/// elements. Control characters so transcript text can never contain them.
pub const SNIPPET_OPEN: &str = "\u{1}";
pub const SNIPPET_CLOSE: &str = "\u{2}";

#[derive(Debug, Clone, Serialize)]
pub struct Meeting {
    pub id: i64,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub title: String,
    pub audio_path: Option<String>,
    pub duration_ms: Option<i64>,
    pub status: String,
    pub engine: Option<String>,
    pub trigger: String,
    pub notes: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Bookmark {
    pub id: i64,
    pub meeting_id: i64,
    pub at_ms: i64,
    pub note: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Speaker {
    pub id: i64,
    pub meeting_id: i64,
    pub label: String,
    pub display_name: String,
    /// Enrolled person this speaker was linked to (auto-match or rename).
    pub person_id: Option<i64>,
    /// True when display_name came from voice matching, not the user.
    pub auto_labeled: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Person {
    pub id: i64,
    pub name: String,
    /// Seconds of speech folded into this voice print so far.
    pub sample_seconds: f64,
    pub created_at: String,
    #[serde(skip)]
    pub embedding: Vec<f32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PersonMeetingStat {
    pub person_id: Option<i64>, // None = the local user ("Me")
    pub person_name: String,
    pub meeting_id: i64,
    pub meeting_title: String,
    pub started_at: String,
    pub duration_ms: i64,
    pub talk_ms: i64,
    pub words: i64,
    pub segments: i64,
}

/// A speaker row ready for insertion alongside a new transcript.
#[derive(Debug, Clone)]
pub struct NewSpeaker {
    pub label: String,
    pub display_name: String,
    /// L2-normalized voice print of this speaker's speech in this meeting.
    pub embedding: Option<Vec<f32>>,
    /// Seconds of speech the embedding was computed from.
    pub emb_seconds: f64,
    pub person_id: Option<i64>,
    pub auto_labeled: bool,
}

pub type SpeakerEmbeddingRecord = (String, Option<Vec<f32>>, f64);

fn embedding_to_blob(e: &[f32]) -> Vec<u8> {
    e.iter().flat_map(|v| v.to_le_bytes()).collect()
}

fn blob_to_embedding(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

#[derive(Debug, Clone, Serialize)]
pub struct Segment {
    pub id: i64,
    pub meeting_id: i64,
    pub speaker_id: Option<i64>,
    pub track: String,
    pub start_ms: i64,
    pub end_ms: i64,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    pub meeting_id: i64,
    pub meeting_title: String,
    pub started_at: String,
    pub segment_id: i64,
    pub start_ms: i64,
    pub speaker_name: Option<String>,
    pub snippet: String,
}

/// A segment ready for insertion (no id yet); speaker is referenced by label.
#[derive(Debug, Clone)]
pub struct NewSegment {
    pub speaker_label: Option<String>,
    pub track: String,
    pub start_ms: i64,
    pub end_ms: i64,
    pub text: String,
}

pub struct Db {
    conn: Mutex<Connection>,
}

const SCHEMA_VERSION: i32 = 4;

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening database {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        let db = Db {
            conn: Mutex::new(conn),
        };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let version: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        anyhow::ensure!(
            version <= SCHEMA_VERSION,
            "database schema version {version} is newer than this Witness build supports ({SCHEMA_VERSION})"
        );
        if version < 1 {
            conn.execute_batch(
                r#"
                BEGIN;
                CREATE TABLE meetings (
                  id INTEGER PRIMARY KEY,
                  started_at TEXT NOT NULL,
                  ended_at TEXT,
                  title TEXT NOT NULL,
                  audio_path TEXT,
                  duration_ms INTEGER,
                  status TEXT NOT NULL DEFAULT 'recording',
                  engine TEXT,
                  trigger TEXT NOT NULL DEFAULT 'auto'
                );
                CREATE TABLE speakers (
                  id INTEGER PRIMARY KEY,
                  meeting_id INTEGER NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
                  label TEXT NOT NULL,
                  display_name TEXT NOT NULL,
                  UNIQUE(meeting_id, label)
                );
                CREATE TABLE segments (
                  id INTEGER PRIMARY KEY,
                  meeting_id INTEGER NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
                  speaker_id INTEGER REFERENCES speakers(id),
                  track TEXT NOT NULL,
                  start_ms INTEGER NOT NULL,
                  end_ms INTEGER NOT NULL,
                  text TEXT NOT NULL
                );
                CREATE INDEX idx_segments_meeting ON segments(meeting_id, start_ms);
                CREATE VIRTUAL TABLE segments_fts USING fts5(
                  text,
                  content='segments',
                  content_rowid='id',
                  tokenize='porter unicode61 remove_diacritics 2'
                );
                CREATE TRIGGER segments_ai AFTER INSERT ON segments BEGIN
                  INSERT INTO segments_fts(rowid, text) VALUES (new.id, new.text);
                END;
                CREATE TRIGGER segments_ad AFTER DELETE ON segments BEGIN
                  INSERT INTO segments_fts(segments_fts, rowid, text) VALUES ('delete', old.id, old.text);
                END;
                CREATE TRIGGER segments_au AFTER UPDATE ON segments BEGIN
                  INSERT INTO segments_fts(segments_fts, rowid, text) VALUES ('delete', old.id, old.text);
                  INSERT INTO segments_fts(rowid, text) VALUES (new.id, new.text);
                END;
                PRAGMA user_version = 1;
                COMMIT;
                "#,
            )?;
        }
        if version < 2 {
            conn.execute_batch(
                r#"
                BEGIN;
                CREATE TABLE IF NOT EXISTS people (
                  id INTEGER PRIMARY KEY,
                  name TEXT NOT NULL UNIQUE,
                  embedding BLOB NOT NULL,        -- f32 LE, L2-normalized
                  sample_seconds REAL NOT NULL DEFAULT 0,
                  created_at TEXT NOT NULL
                );
                ALTER TABLE speakers ADD COLUMN person_id INTEGER REFERENCES people(id) ON DELETE SET NULL;
                ALTER TABLE speakers ADD COLUMN auto_labeled INTEGER NOT NULL DEFAULT 0;
                ALTER TABLE speakers ADD COLUMN embedding BLOB;
                ALTER TABLE speakers ADD COLUMN emb_seconds REAL NOT NULL DEFAULT 0;
                PRAGMA user_version = 2;
                COMMIT;
                "#,
            )?;
        }
        if version < 3 {
            conn.execute_batch(
                r#"
                BEGIN;
                ALTER TABLE meetings ADD COLUMN deleted_at TEXT;
                PRAGMA user_version = 3;
                COMMIT;
                "#,
            )?;
        }
        if version < 4 {
            conn.execute_batch(
                r#"
                BEGIN;
                ALTER TABLE meetings ADD COLUMN notes TEXT NOT NULL DEFAULT '';
                CREATE TABLE bookmarks (
                  id INTEGER PRIMARY KEY,
                  meeting_id INTEGER NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
                  at_ms INTEGER NOT NULL,
                  note TEXT NOT NULL DEFAULT ''
                );
                CREATE INDEX idx_bookmarks_meeting ON bookmarks(meeting_id, at_ms);
                CREATE VIRTUAL TABLE meetings_notes_fts USING fts5(
                  notes,
                  content='meetings',
                  content_rowid='id',
                  tokenize='porter unicode61 remove_diacritics 2'
                );
                INSERT INTO meetings_notes_fts(rowid, notes) SELECT id, notes FROM meetings;
                CREATE TRIGGER meetings_notes_ai AFTER INSERT ON meetings BEGIN
                  INSERT INTO meetings_notes_fts(rowid, notes) VALUES (new.id, new.notes);
                END;
                CREATE TRIGGER meetings_notes_ad AFTER DELETE ON meetings BEGIN
                  INSERT INTO meetings_notes_fts(meetings_notes_fts, rowid, notes) VALUES ('delete', old.id, old.notes);
                END;
                CREATE TRIGGER meetings_notes_au AFTER UPDATE OF notes ON meetings BEGIN
                  INSERT INTO meetings_notes_fts(meetings_notes_fts, rowid, notes) VALUES ('delete', old.id, old.notes);
                  INSERT INTO meetings_notes_fts(rowid, notes) VALUES (new.id, new.notes);
                END;
                PRAGMA user_version = 4;
                COMMIT;
                "#,
            )?;
        }
        Ok(())
    }

    // ---------- meetings ----------

    pub fn create_meeting(&self, title: &str, started_at: &str, trigger: &str) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO meetings (started_at, title, status, trigger) VALUES (?1, ?2, 'recording', ?3)",
            params![started_at, title, trigger],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn finish_recording(&self, id: i64, ended_at: &str, duration_ms: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE meetings SET ended_at = ?2, duration_ms = ?3, status = 'recorded' WHERE id = ?1",
            params![id, ended_at, duration_ms],
        )?;
        Ok(())
    }

    pub fn set_status(&self, id: i64, status: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE meetings SET status = ?2 WHERE id = ?1",
            params![id, status],
        )?;
        Ok(())
    }

    pub fn set_audio_path(&self, id: i64, audio_path: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE meetings SET audio_path = ?2 WHERE id = ?1",
            params![id, audio_path],
        )?;
        Ok(())
    }

    pub fn rename_meeting(&self, id: i64, title: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE meetings SET title = ?2 WHERE id = ?1",
            params![id, title],
        )?;
        Ok(())
    }

    pub fn delete_meeting(&self, id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM meetings WHERE id = ?1", params![id])?;
        Ok(())
    }

    fn row_to_meeting(row: &rusqlite::Row<'_>) -> rusqlite::Result<Meeting> {
        Ok(Meeting {
            id: row.get(0)?,
            started_at: row.get(1)?,
            ended_at: row.get(2)?,
            title: row.get(3)?,
            audio_path: row.get(4)?,
            duration_ms: row.get(5)?,
            status: row.get(6)?,
            engine: row.get(7)?,
            trigger: row.get(8)?,
            notes: row.get(9)?,
        })
    }

    const MEETING_COLS: &'static str =
        "id, started_at, ended_at, title, audio_path, duration_ms, status, engine, trigger, notes";

    pub fn list_meetings(&self, offset: i64, limit: i64) -> Result<Vec<Meeting>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM meetings WHERE deleted_at IS NULL
             ORDER BY started_at DESC LIMIT ?1 OFFSET ?2",
            Self::MEETING_COLS
        ))?;
        let rows = stmt
            .query_map(params![limit, offset], Self::row_to_meeting)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Recycle-bin contents: (meeting, deleted_at), newest first.
    pub fn list_deleted_meetings(&self) -> Result<Vec<(Meeting, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {}, deleted_at FROM meetings WHERE deleted_at IS NOT NULL
             ORDER BY deleted_at DESC",
            Self::MEETING_COLS
        ))?;
        let rows = stmt
            .query_map([], |row| {
                Ok((Self::row_to_meeting(row)?, row.get::<_, String>(10)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn set_notes(&self, meeting_id: i64, notes: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE meetings SET notes = ?2 WHERE id = ?1",
            params![meeting_id, notes],
        )?;
        Ok(())
    }

    // ---------- bookmarks ----------

    pub fn add_bookmark(&self, meeting_id: i64, at_ms: i64, note: &str) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO bookmarks (meeting_id, at_ms, note) VALUES (?1, ?2, ?3)",
            params![meeting_id, at_ms, note],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn list_bookmarks(&self, meeting_id: i64) -> Result<Vec<Bookmark>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, meeting_id, at_ms, note FROM bookmarks WHERE meeting_id = ?1 ORDER BY at_ms",
        )?;
        let rows = stmt
            .query_map(params![meeting_id], |row| {
                Ok(Bookmark {
                    id: row.get(0)?,
                    meeting_id: row.get(1)?,
                    at_ms: row.get(2)?,
                    note: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn set_bookmark_note(&self, bookmark_id: i64, note: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE bookmarks SET note = ?2 WHERE id = ?1",
            params![bookmark_id, note],
        )?;
        Ok(())
    }

    pub fn delete_bookmark(&self, bookmark_id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM bookmarks WHERE id = ?1", params![bookmark_id])?;
        Ok(())
    }

    pub fn soft_delete_meeting(&self, id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE meetings SET deleted_at = ?2 WHERE id = ?1",
            params![id, chrono::Local::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn restore_meeting(&self, id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE meetings SET deleted_at = NULL WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    pub fn get_meeting(&self, id: i64) -> Result<Option<Meeting>> {
        let conn = self.conn.lock().unwrap();
        let m = conn
            .query_row(
                &format!("SELECT {} FROM meetings WHERE id = ?1", Self::MEETING_COLS),
                params![id],
                Self::row_to_meeting,
            )
            .optional()?;
        Ok(m)
    }

    /// Meetings whose recordings finished but were never transcribed/encoded
    /// (used at startup to re-enqueue orphaned work), oldest first.
    pub fn meetings_with_status(&self, statuses: &[&str]) -> Result<Vec<Meeting>> {
        let conn = self.conn.lock().unwrap();
        let placeholders = statuses.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM meetings WHERE status IN ({}) AND deleted_at IS NULL
             ORDER BY started_at ASC",
            Self::MEETING_COLS,
            placeholders
        ))?;
        let rows = stmt
            .query_map(
                rusqlite::params_from_iter(statuses.iter()),
                Self::row_to_meeting,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ---------- speakers & segments ----------

    pub fn get_speakers(&self, meeting_id: i64) -> Result<Vec<Speaker>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, meeting_id, label, display_name, person_id, auto_labeled
             FROM speakers WHERE meeting_id = ?1 ORDER BY label",
        )?;
        let rows = stmt
            .query_map(params![meeting_id], |row| {
                Ok(Speaker {
                    id: row.get(0)?,
                    meeting_id: row.get(1)?,
                    label: row.get(2)?,
                    display_name: row.get(3)?,
                    person_id: row.get(4)?,
                    auto_labeled: row.get::<_, i64>(5)? != 0,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Every speaker row that has a stored voice print, for retroactive
    /// re-matching after enrollments change.
    pub fn speakers_with_embeddings(&self) -> Result<Vec<Speaker>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, meeting_id, label, display_name, person_id, auto_labeled
             FROM speakers WHERE embedding IS NOT NULL AND label != 'me'",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(Speaker {
                    id: row.get(0)?,
                    meeting_id: row.get(1)?,
                    label: row.get(2)?,
                    display_name: row.get(3)?,
                    person_id: row.get(4)?,
                    auto_labeled: row.get::<_, i64>(5)? != 0,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn get_embedding_only(&self, speaker_id: i64) -> Result<Option<Vec<f32>>> {
        let conn = self.conn.lock().unwrap();
        let blob: Option<Option<Vec<u8>>> = conn
            .query_row(
                "SELECT embedding FROM speakers WHERE id = ?1",
                params![speaker_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(blob.flatten().map(|b| blob_to_embedding(&b)))
    }

    /// Apply (or clear) an automatic voice-match label. Never touches rows
    /// the user named manually.
    pub fn set_auto_match(
        &self,
        speaker_id: i64,
        display_name: &str,
        person_id: Option<i64>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE speakers SET display_name = ?2, person_id = ?3, auto_labeled = ?4 WHERE id = ?1",
            params![speaker_id, display_name, person_id, person_id.is_some() as i64],
        )?;
        Ok(())
    }

    /// One speaker row with its stored voice print (for enrollment on rename).
    pub fn get_speaker_embedding(&self, speaker_id: i64) -> Result<Option<SpeakerEmbeddingRecord>> {
        let conn = self.conn.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT label, embedding, emb_seconds FROM speakers WHERE id = ?1",
                params![speaker_id],
                |row| {
                    let blob: Option<Vec<u8>> = row.get(1)?;
                    Ok((
                        row.get::<_, String>(0)?,
                        blob.map(|b| blob_to_embedding(&b)),
                        row.get::<_, f64>(2)?,
                    ))
                },
            )
            .optional()?;
        Ok(row)
    }

    pub fn get_segments(&self, meeting_id: i64) -> Result<Vec<Segment>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, meeting_id, speaker_id, track, start_ms, end_ms, text
             FROM segments WHERE meeting_id = ?1 ORDER BY start_ms, id",
        )?;
        let rows = stmt
            .query_map(params![meeting_id], |row| {
                Ok(Segment {
                    id: row.get(0)?,
                    meeting_id: row.get(1)?,
                    speaker_id: row.get(2)?,
                    track: row.get(3)?,
                    start_ms: row.get(4)?,
                    end_ms: row.get(5)?,
                    text: row.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// User-driven rename: clears the auto flag and (optionally) links a person.
    pub fn rename_speaker(
        &self,
        speaker_id: i64,
        display_name: &str,
        person_id: Option<i64>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE speakers SET display_name = ?2, person_id = ?3, auto_labeled = 0 WHERE id = ?1",
            params![speaker_id, display_name, person_id],
        )?;
        Ok(())
    }

    // ---------- people (enrolled voice prints) ----------

    pub fn list_people(&self) -> Result<Vec<Person>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, name, embedding, sample_seconds, created_at FROM people ORDER BY name",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let blob: Vec<u8> = row.get(2)?;
                Ok(Person {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    embedding: blob_to_embedding(&blob),
                    sample_seconds: row.get(3)?,
                    created_at: row.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn get_person_by_name(&self, name: &str) -> Result<Option<Person>> {
        let conn = self.conn.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT id, name, embedding, sample_seconds, created_at
                 FROM people WHERE name = ?1 COLLATE NOCASE",
                params![name],
                |row| {
                    let blob: Vec<u8> = row.get(2)?;
                    Ok(Person {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        embedding: blob_to_embedding(&blob),
                        sample_seconds: row.get(3)?,
                        created_at: row.get(4)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    pub fn create_person(&self, name: &str, embedding: &[f32], sample_seconds: f64) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO people (name, embedding, sample_seconds, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![
                name,
                embedding_to_blob(embedding),
                sample_seconds,
                chrono::Local::now().to_rfc3339()
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn update_person_embedding(
        &self,
        person_id: i64,
        embedding: &[f32],
        sample_seconds: f64,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE people SET embedding = ?2, sample_seconds = ?3 WHERE id = ?1",
            params![person_id, embedding_to_blob(embedding), sample_seconds],
        )?;
        Ok(())
    }

    pub fn delete_person(&self, person_id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM people WHERE id = ?1", params![person_id])?;
        Ok(())
    }

    /// Replace a meeting's transcript in one transaction: wipes prior
    /// speakers/segments, inserts the new ones (FTS follows via triggers),
    /// and stamps status + engine.
    pub fn replace_transcript(
        &self,
        meeting_id: i64,
        speakers: &[NewSpeaker],
        segments: &[NewSegment],
        engine: &str,
    ) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM segments WHERE meeting_id = ?1",
            params![meeting_id],
        )?;
        tx.execute(
            "DELETE FROM speakers WHERE meeting_id = ?1",
            params![meeting_id],
        )?;
        {
            let mut ins_speaker = tx.prepare(
                "INSERT INTO speakers (meeting_id, label, display_name, person_id, auto_labeled, embedding, emb_seconds)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            let mut speaker_ids = std::collections::HashMap::new();
            for sp in speakers {
                ins_speaker.execute(params![
                    meeting_id,
                    sp.label,
                    sp.display_name,
                    sp.person_id,
                    sp.auto_labeled as i64,
                    sp.embedding.as_deref().map(embedding_to_blob),
                    sp.emb_seconds,
                ])?;
                speaker_ids.insert(sp.label.clone(), tx.last_insert_rowid());
            }
            let mut ins_seg = tx.prepare(
                "INSERT INTO segments (meeting_id, speaker_id, track, start_ms, end_ms, text)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for seg in segments {
                let speaker_id = seg
                    .speaker_label
                    .as_ref()
                    .and_then(|l| speaker_ids.get(l))
                    .copied();
                ins_seg.execute(params![
                    meeting_id,
                    speaker_id,
                    seg.track,
                    seg.start_ms,
                    seg.end_ms,
                    seg.text
                ])?;
            }
        }
        tx.execute(
            "UPDATE meetings SET status = 'transcribed', engine = ?2 WHERE id = ?1",
            params![meeting_id, engine],
        )?;
        tx.commit()?;
        Ok(())
    }

    // ---------- stats ----------

    /// One row per (person, meeting) with talk time and (approximate,
    /// space-counted) word totals, plus the same for the local user as a
    /// virtual "Me" person (person_id = None). Ordered newest-first.
    pub fn people_meeting_stats(&self) -> Result<Vec<PersonMeetingStat>> {
        const WORDS: &str =
            "COALESCE(SUM(LENGTH(TRIM(seg.text)) - LENGTH(REPLACE(TRIM(seg.text), ' ', '')) + 1), 0)";
        let conn = self.conn.lock().unwrap();
        let sql = format!(
            "SELECT p.id, p.name, m.id, m.title, m.started_at, COALESCE(m.duration_ms, 0),
                    COALESCE(SUM(seg.end_ms - seg.start_ms), 0), {WORDS}, COUNT(seg.id)
             FROM people p
             JOIN speakers sp ON sp.person_id = p.id
             JOIN meetings m ON m.id = sp.meeting_id
             LEFT JOIN segments seg ON seg.speaker_id = sp.id
             WHERE m.deleted_at IS NULL
             GROUP BY p.id, m.id
             UNION ALL
             SELECT NULL, 'Me', m.id, m.title, m.started_at, COALESCE(m.duration_ms, 0),
                    COALESCE(SUM(seg.end_ms - seg.start_ms), 0), {WORDS}, COUNT(seg.id)
             FROM speakers sp
             JOIN meetings m ON m.id = sp.meeting_id
             LEFT JOIN segments seg ON seg.speaker_id = sp.id
             WHERE sp.label = 'me' AND m.deleted_at IS NULL
             GROUP BY m.id
             ORDER BY 5 DESC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map([], |row| {
                Ok(PersonMeetingStat {
                    person_id: row.get(0)?,
                    person_name: row.get(1)?,
                    meeting_id: row.get(2)?,
                    meeting_title: row.get(3)?,
                    started_at: row.get(4)?,
                    duration_ms: row.get(5)?,
                    talk_ms: row.get(6)?,
                    words: row.get(7)?,
                    segments: row.get(8)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ---------- search ----------

    /// Sanitize a user query into an FTS5 MATCH expression: each token is
    /// quoted; the last gets a `*` for type-ahead. A query that already
    /// parses as valid FTS5 syntax is passed through unchanged.
    fn build_match_expr(conn: &Connection, raw: &str) -> Option<String> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return None;
        }
        // Try raw passthrough for power users (phrase quotes, AND/OR/NEAR).
        // Must actually step the query (LIMIT 1, not 0 â€” LIMIT 0 returns
        // before the FTS5 vtab ever parses the MATCH expression).
        let is_valid = conn
            .prepare("SELECT 1 FROM segments_fts WHERE segments_fts MATCH ?1 LIMIT 1")
            .and_then(|mut s| {
                s.query(params![trimmed])
                    .and_then(|mut rows| rows.next().map(|_| ()))
            })
            .is_ok();
        let has_syntax = trimmed.contains('"')
            || trimmed.contains(" OR ")
            || trimmed.contains(" AND ")
            || trimmed.contains(" NOT ")
            || trimmed.contains("NEAR(");
        if is_valid && has_syntax {
            return Some(trimmed.to_string());
        }
        let tokens: Vec<String> = trimmed
            .split_whitespace()
            .map(|t| t.replace('"', ""))
            // A quoted phrase with zero tokenizable chars ("(" etc.) is an
            // FTS5 syntax error â€” drop punctuation-only tokens.
            .filter(|t| t.chars().any(|c| c.is_alphanumeric()))
            .collect();
        if tokens.is_empty() {
            return None;
        }
        let last = tokens.len() - 1;
        Some(
            tokens
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    if i == last {
                        format!("\"{}\"*", t)
                    } else {
                        format!("\"{}\"", t)
                    }
                })
                .collect::<Vec<_>>()
                .join(" "),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn search(
        &self,
        query: &str,
        offset: i64,
        limit: i64,
        person_id: Option<i64>,
        track: Option<&str>,
        date_from: Option<&str>,
        date_to: Option<&str>,
    ) -> Result<Vec<SearchHit>> {
        let conn = self.conn.lock().unwrap();
        let Some(expr) = Self::build_match_expr(&conn, query) else {
            return Ok(vec![]);
        };
        let mut sql = String::from(
            "SELECT m.id, m.title, m.started_at, s.id, s.start_ms, sp.display_name,
                    snippet(segments_fts, 0, ?1, ?2, ' â€¦ ', 12)
             FROM segments_fts
             JOIN segments s ON s.id = segments_fts.rowid
             JOIN meetings m ON m.id = s.meeting_id
             LEFT JOIN speakers sp ON sp.id = s.speaker_id
             WHERE segments_fts MATCH ?3 AND m.deleted_at IS NULL",
        );
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = vec![
            Box::new(SNIPPET_OPEN),
            Box::new(SNIPPET_CLOSE),
            Box::new(expr.clone()),
        ];
        if let Some(pid) = person_id {
            sql.push_str(&format!(" AND sp.person_id = ?{}", args.len() + 1));
            args.push(Box::new(pid));
        }
        if let Some(track) = track {
            sql.push_str(&format!(" AND s.track = ?{}", args.len() + 1));
            args.push(Box::new(track.to_string()));
        }
        if let Some(from) = date_from {
            sql.push_str(&format!(" AND m.started_at >= ?{}", args.len() + 1));
            args.push(Box::new(from.to_string()));
        }
        if let Some(to) = date_to {
            // Inclusive day: anything on the 'to' date still matches.
            sql.push_str(&format!(
                " AND substr(m.started_at, 1, 10) <= ?{}",
                args.len() + 1
            ));
            args.push(Box::new(to.to_string()));
        }
        sql.push_str(&format!(
            " ORDER BY bm25(segments_fts) LIMIT ?{} OFFSET ?{}",
            args.len() + 1,
            args.len() + 2
        ));
        args.push(Box::new(limit));
        args.push(Box::new(offset));

        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt
            .query_map(
                rusqlite::params_from_iter(args.iter().map(|a| a.as_ref())),
                |row| {
                    Ok(SearchHit {
                        meeting_id: row.get(0)?,
                        meeting_title: row.get(1)?,
                        started_at: row.get(2)?,
                        segment_id: row.get(3)?,
                        start_ms: row.get(4)?,
                        speaker_name: row.get(5)?,
                        snippet: row.get(6)?,
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        // Meeting-notes hits (segment_id = -1 → open the meeting, no seek).
        // Speaker/track filters don't apply to notes, so skip them then;
        // also only on the first page to avoid duplicate appends.
        if person_id.is_none() && track.is_none() && offset == 0 {
            let mut sql = String::from(
                "SELECT m.id, m.title, m.started_at,
                        snippet(meetings_notes_fts, 0, ?1, ?2, ' … ', 12)
                 FROM meetings_notes_fts
                 JOIN meetings m ON m.id = meetings_notes_fts.rowid
                 WHERE meetings_notes_fts MATCH ?3 AND m.deleted_at IS NULL",
            );
            let mut nargs: Vec<Box<dyn rusqlite::ToSql>> = vec![
                Box::new(SNIPPET_OPEN),
                Box::new(SNIPPET_CLOSE),
                Box::new(expr),
            ];
            if let Some(from) = date_from {
                sql.push_str(&format!(" AND m.started_at >= ?{}", nargs.len() + 1));
                nargs.push(Box::new(from.to_string()));
            }
            if let Some(to) = date_to {
                sql.push_str(&format!(
                    " AND substr(m.started_at, 1, 10) <= ?{}",
                    nargs.len() + 1
                ));
                nargs.push(Box::new(to.to_string()));
            }
            sql.push_str(" ORDER BY bm25(meetings_notes_fts) LIMIT 10");
            let mut stmt = conn.prepare(&sql)?;
            let notes_hits = stmt
                .query_map(
                    rusqlite::params_from_iter(nargs.iter().map(|a| a.as_ref())),
                    |row| {
                        Ok(SearchHit {
                            meeting_id: row.get(0)?,
                            meeting_title: row.get(1)?,
                            started_at: row.get(2)?,
                            segment_id: -1,
                            start_ms: 0,
                            speaker_name: Some("Notes".into()),
                            snippet: row.get(3)?,
                        })
                    },
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows.extend(notes_hits);
        }
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_transcript_and_search_roundtrip() {
        let dir = std::env::temp_dir().join(format!("witness-db-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(&dir.join("t.db")).unwrap();

        let id = db
            .create_meeting("Test meeting", "2026-07-18T10:00:00+01:00", "manual")
            .unwrap();
        db.finish_recording(id, "2026-07-18T10:30:00+01:00", 1_800_000)
            .unwrap();
        assert_eq!(db.get_meeting(id).unwrap().unwrap().status, "recorded");

        let speakers = vec![
            NewSpeaker {
                label: "me".into(),
                display_name: "Me".into(),
                embedding: None,
                emb_seconds: 0.0,
                person_id: None,
                auto_labeled: false,
            },
            NewSpeaker {
                label: "S1".into(),
                display_name: "Speaker 1".into(),
                embedding: Some(vec![0.6, 0.8]),
                emb_seconds: 12.5,
                person_id: None,
                auto_labeled: false,
            },
        ];
        let segments = vec![
            NewSegment {
                speaker_label: Some("me".into()),
                track: "mic".into(),
                start_ms: 0,
                end_ms: 2000,
                text: "hello budget meeting".into(),
            },
            NewSegment {
                speaker_label: Some("S1".into()),
                track: "loopback".into(),
                start_ms: 2000,
                end_ms: 4000,
                text: "quarterly forecast numbers".into(),
            },
        ];
        db.replace_transcript(id, &speakers, &segments, "parakeet")
            .unwrap();

        let m = db.get_meeting(id).unwrap().unwrap();
        assert_eq!(m.status, "transcribed");
        assert_eq!(m.engine.as_deref(), Some("parakeet"));
        assert_eq!(db.get_speakers(id).unwrap().len(), 2);
        assert_eq!(db.get_segments(id).unwrap().len(), 2);

        // FTS: whole word, prefix (type-ahead), stemming via porter.
        let hits = db
            .search("forecast", 0, 10, None, None, None, None)
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].snippet.contains(SNIPPET_OPEN));
        assert_eq!(hits[0].speaker_name.as_deref(), Some("Speaker 1"));
        assert_eq!(
            db.search("quart", 0, 10, None, None, None, None)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            db.search("meetings", 0, 10, None, None, None, None)
                .unwrap()
                .len(),
            1
        ); // stemmed
        assert!(db
            .search("nonexistentword", 0, 10, None, None, None, None)
            .unwrap()
            .is_empty());
        // Hostile input must not error, just return no rows.
        assert!(db
            .search("\"unbalanced AND ( NEAR", 0, 10, None, None, None, None)
            .unwrap()
            .is_empty());

        // Stored speaker embedding survives the roundtrip.
        let s1 = db
            .get_speakers(id)
            .unwrap()
            .into_iter()
            .find(|s| s.label == "S1")
            .unwrap();
        let (label, emb, secs) = db.get_speaker_embedding(s1.id).unwrap().unwrap();
        assert_eq!(label, "S1");
        assert_eq!(emb.unwrap(), vec![0.6, 0.8]);
        assert!((secs - 12.5).abs() < 1e-9);

        // People: create, case-insensitive lookup, link on rename, delete.
        let alice = db.create_person("Alice", &[0.6, 0.8], 12.5).unwrap();
        assert_eq!(db.get_person_by_name("alice").unwrap().unwrap().id, alice);
        db.rename_speaker(s1.id, "Alice", Some(alice)).unwrap();
        let s1 = db
            .get_speakers(id)
            .unwrap()
            .into_iter()
            .find(|s| s.label == "S1")
            .unwrap();
        assert_eq!(s1.display_name, "Alice");
        assert_eq!(s1.person_id, Some(alice));
        assert!(!s1.auto_labeled);
        db.update_person_embedding(alice, &[1.0, 0.0], 30.0)
            .unwrap();
        assert_eq!(db.list_people().unwrap()[0].embedding, vec![1.0, 0.0]);
        db.delete_person(alice).unwrap();
        assert!(db.list_people().unwrap().is_empty());
        // FK ON DELETE SET NULL cleared the link.
        let s1 = db
            .get_speakers(id)
            .unwrap()
            .into_iter()
            .find(|s| s.label == "S1")
            .unwrap();
        assert_eq!(s1.person_id, None);

        // Notes: FTS-searchable through the same search(), bookmarks CRUD.
        db.set_notes(id, "remember to send the follow-up invoice")
            .unwrap();
        let hits = db.search("invoice", 0, 10, None, None, None, None).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].segment_id, -1);
        assert_eq!(hits[0].speaker_name.as_deref(), Some("Notes"));
        let bm = db.add_bookmark(id, 12_000, "").unwrap();
        db.set_bookmark_note(bm, "key decision").unwrap();
        let bms = db.list_bookmarks(id).unwrap();
        assert_eq!(bms.len(), 1);
        assert_eq!(bms[0].note, "key decision");
        db.delete_bookmark(bm).unwrap();
        assert!(db.list_bookmarks(id).unwrap().is_empty());

        // Recycle bin: soft delete hides, restore brings back.
        db.soft_delete_meeting(id).unwrap();
        assert!(db.list_meetings(0, 10).unwrap().is_empty());
        assert!(db
            .search("budget", 0, 10, None, None, None, None)
            .unwrap()
            .is_empty());
        let binned = db.list_deleted_meetings().unwrap();
        assert_eq!(binned.len(), 1);
        assert_eq!(binned[0].0.id, id);
        db.restore_meeting(id).unwrap();
        assert_eq!(db.list_meetings(0, 10).unwrap().len(), 1);
        assert!(db.list_deleted_meetings().unwrap().is_empty());

        // Delete cascades through speakers/segments/FTS.
        db.delete_meeting(id).unwrap();
        assert!(db
            .search("forecast", 0, 10, None, None, None, None)
            .unwrap()
            .is_empty());
        assert!(db.get_segments(id).unwrap().is_empty());

        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
