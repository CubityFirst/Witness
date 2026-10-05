//! The archive: meetings and the recycle bin, bookmarks, speakers, people,
//! search, transcript export and archived audio.

use super::{
    assert_off_ui_thread, err, AppCore, CoreEvent, CoreResult, DeletedMeeting, MeetingDetail,
    PersonInfo, SearchQuery,
};
use crate::db::{Meeting, PersonMeetingStat, SearchHit};
use crate::pipeline::Job;
use crate::recorder;
use crate::settings::Engine;
use crate::state::AppState;
use crate::transcript_export::ExportFormat;
use std::path::{Path, PathBuf};

/// Search results per page.
const SEARCH_PAGE: i64 = 40;

/// Permanent removal: audio + WAVs + rows. Shared by the bin UI and the
/// 30-day auto-purge.
fn remove_if_exists(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("deleting {}: {e}", path.display())),
    }
}

/// Resolve a meeting's stored archive path under the audio directory,
/// refusing anything that could escape it.
pub(crate) fn safe_archive_path(root: &Path, relative: &str) -> Result<PathBuf, String> {
    use std::path::Component;

    let relative = Path::new(relative);
    if relative.as_os_str().is_empty()
        || relative.is_absolute()
        || relative
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err("meeting has an invalid archived-audio path".into());
    }
    Ok(root.join(relative))
}

pub(crate) fn purge_meeting_data(state: &AppState, id: i64) -> Result<(), String> {
    let is_binned = state
        .db
        .list_deleted_meetings()
        .map_err(err)?
        .iter()
        .any(|(meeting, _)| meeting.id == id);
    if !is_binned {
        return Err("Only meetings in the recycle bin can be permanently deleted".into());
    }
    state
        .pipeline
        .cancel(id)
        .map_err(|error| format!("Cannot purge this meeting yet: {error:#}"))?;
    let meeting = state
        .db
        .get_meeting(id)
        .map_err(err)?
        .ok_or_else(|| format!("meeting {id} not found"))?;
    // Copy the dirs out: the settings mutex is [ui-ok] only because no
    // thread holds it across I/O, and deleting files can be slow (AV scans,
    // network data dirs).
    let (audio_dir, rec_tmp_dir) = {
        let s = state.settings.lock().unwrap();
        (s.audio_dir(), s.rec_tmp_dir())
    };
    if let Some(rel) = meeting.audio_path {
        remove_if_exists(&safe_archive_path(&audio_dir, &rel)?)?;
    }
    let (mic, lop, meta) = recorder::wav_paths(&rec_tmp_dir, id);
    for p in [mic, lop, meta] {
        remove_if_exists(&p)?;
    }
    // Only forget the row after every known copy is gone.  If Windows has a
    // file locked, the binned row remains available for a later retry rather
    // than falsely reporting a permanent deletion.
    state.db.delete_meeting(id).map_err(err)
}

/// Names that shouldn't create voice-print enrollments.
fn is_generic_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower == "me"
        || lower == "them"
        || lower == "unknown"
        || (lower.starts_with("speaker") && lower[7..].trim().chars().all(|c| c.is_ascii_digit()))
}

/// `<>:"/\|?*` → `_`, for save-dialog default names.
fn sanitize_file_stem(title: &str) -> String {
    title
        .chars()
        .map(|c| if r#"<>:"/\|?*"#.contains(c) { '_' } else { c })
        .collect()
}

impl AppCore {
    fn meeting_or_not_found(&self, id: i64) -> CoreResult<Meeting> {
        self.state
            .db
            .get_meeting(id)
            .map_err(err)?
            .ok_or_else(|| format!("meeting {id} not found"))
    }

    /// Retroactive voice re-match after any enrollment change ("rematch"
    /// thread); emits `MeetingsChanged` only when labels changed.
    fn rematch_speakers(&self) {
        let state = self.state.clone();
        let sink = self.sink.clone();
        std::thread::Builder::new()
            .name("rematch".into())
            .spawn(move || {
                let threshold = state.settings.lock().unwrap().speaker_match_threshold;
                match crate::speaker_id::rematch_all(&state.db, threshold) {
                    Ok(0) => {}
                    Ok(n) => {
                        log::info!("retroactive voice re-match updated {n} speaker label(s)");
                        sink.emit(CoreEvent::MeetingsChanged);
                    }
                    Err(e) => log::warn!("retroactive re-match failed: {e:#}"),
                }
            })
            .ok();
    }

    /// Live meetings, newest first; `limit` is clamped to 1..=500.
    pub fn list_meetings(&self, offset: i64, limit: i64) -> CoreResult<Vec<Meeting>> {
        self.state
            .db
            .list_meetings(offset, limit.clamp(1, 500))
            .map_err(err)
    }

    /// Live meetings started at or after an RFC 3339 instant or a
    /// `YYYY-MM-DD` date (People weekly trend).
    pub fn meetings_started_since(&self, from_rfc3339_or_date: &str) -> CoreResult<Vec<Meeting>> {
        self.state
            .db
            .list_meetings_started_since(from_rfc3339_or_date)
            .map_err(err)
    }

    pub fn meeting_detail(&self, id: i64) -> CoreResult<MeetingDetail> {
        let meeting = self.meeting_or_not_found(id)?;
        let db = &self.state.db;
        Ok(MeetingDetail {
            speakers: db.get_speakers(id).map_err(err)?,
            segments: db.get_segments(id).map_err(err)?,
            bookmarks: db.list_bookmarks(id).map_err(err)?,
            last_error: db.pipeline_job_error(id).map_err(err)?,
            pipeline_queued: db.pipeline_job_queued(id).map_err(err)?,
            meeting,
        })
    }

    /// Trims; rejects an empty title. Emits nothing (the caller refreshes).
    pub fn rename_meeting(&self, id: i64, title: &str) -> CoreResult<()> {
        let title = title.trim();
        if title.is_empty() {
            return Err("Title cannot be empty".into());
        }
        self.state.db.rename_meeting(id, title).map_err(err)
    }

    /// Used only by the notes writer (`queue_notes` is the UI's path). ⏳
    pub(crate) fn set_meeting_notes(&self, id: i64, notes: &str) -> CoreResult<()> {
        assert_off_ui_thread("set_meeting_notes");
        self.state.db.set_notes(id, notes).map_err(err)
    }

    /// "Delete" = move to the recycle bin (auto-purged after 30 days). ⏳
    pub fn delete_meeting(&self, id: i64) -> CoreResult<()> {
        assert_off_ui_thread("delete_meeting");
        let state = &self.state;
        if state.recorder.lock().unwrap().meeting_id() == Some(id) {
            return Err("Stop the recording before deleting this meeting".into());
        }
        state.db.soft_delete_meeting(id).map_err(err)?;
        if let Err(error) = state.pipeline.cancel(id) {
            // A worker already owns this meeting. Keep it visible and let the
            // user retry deletion after processing reaches a safe boundary.
            let _ = state.db.restore_meeting(id);
            return Err(format!("Cannot delete this meeting yet: {error:#}"));
        }
        self.rematch_speakers();
        self.emit(CoreEvent::MeetingsChanged);
        Ok(())
    }

    pub fn restore_meeting(&self, id: i64) -> CoreResult<()> {
        self.state.db.restore_meeting(id).map_err(err)?;
        self.rematch_speakers();
        self.emit(CoreEvent::MeetingsChanged);
        Ok(())
    }

    pub fn list_deleted_meetings(&self) -> CoreResult<Vec<DeletedMeeting>> {
        Ok(self
            .state
            .db
            .list_deleted_meetings()
            .map_err(err)?
            .into_iter()
            .map(|(meeting, deleted_at)| DeletedMeeting {
                meeting,
                deleted_at,
            })
            .collect())
    }

    /// Permanently delete one binned meeting. ⏳
    pub fn purge_meeting(&self, id: i64) -> CoreResult<()> {
        assert_off_ui_thread("purge_meeting");
        purge_meeting_data(&self.state, id)?;
        self.emit(CoreEvent::MeetingsChanged);
        Ok(())
    }

    /// Permanently delete everything in the bin. ⏳
    pub fn empty_recycle_bin(&self) -> CoreResult<()> {
        assert_off_ui_thread("empty_recycle_bin");
        for (meeting, _) in self.state.db.list_deleted_meetings().map_err(err)? {
            purge_meeting_data(&self.state, meeting.id)?;
        }
        self.emit(CoreEvent::MeetingsChanged);
        Ok(())
    }

    pub fn add_bookmark(&self, meeting_id: i64, at_ms: i64, note: &str) -> CoreResult<i64> {
        let id = self
            .state
            .db
            .add_bookmark(meeting_id, at_ms, note)
            .map_err(err)?;
        self.emit(CoreEvent::MeetingsChanged);
        Ok(id)
    }

    pub fn set_bookmark_note(&self, bookmark_id: i64, note: &str) -> CoreResult<()> {
        self.state
            .db
            .set_bookmark_note(bookmark_id, note)
            .map_err(err)
    }

    pub fn delete_bookmark(&self, bookmark_id: i64) -> CoreResult<()> {
        self.state.db.delete_bookmark(bookmark_id).map_err(err)
    }

    /// Renaming a remote speaker is also the enrollment gesture. Its person's
    /// print is rebuilt from every explicit link in the same transaction, so
    /// retries and reassignment cannot count the same speaker twice.
    pub fn rename_speaker(&self, speaker_id: i64, name: &str) -> CoreResult<()> {
        let name = name.trim();
        if name.is_empty() {
            return Err("Name cannot be empty".into());
        }
        self.state
            .db
            .rename_speaker(speaker_id, name, !is_generic_name(name))
            .map_err(err)?;
        self.rematch_speakers();
        Ok(())
    }

    pub fn list_people(&self) -> CoreResult<Vec<PersonInfo>> {
        Ok(self
            .state
            .db
            .list_people()
            .map_err(err)?
            .into_iter()
            .map(|p| PersonInfo {
                id: p.id,
                name: p.name,
                sample_seconds: p.sample_seconds,
                created_at: p.created_at,
            })
            .collect())
    }

    pub fn delete_person(&self, person_id: i64) -> CoreResult<()> {
        self.state.db.delete_person(person_id).map_err(err)?;
        // Revert or reassign automatic labels that pointed at the deleted person.
        self.rematch_speakers();
        Ok(())
    }

    /// Per-person meeting statistics (aggregate query). ⏳
    pub fn people_stats(&self) -> CoreResult<Vec<PersonMeetingStat>> {
        assert_off_ui_thread("people_stats");
        self.state.db.people_meeting_stats().map_err(err)
    }

    /// One page of full-text search; the track filter only accepts
    /// "mic" / "loopback" and empty dates are ignored. ⏳
    pub fn search(&self, q: &SearchQuery) -> CoreResult<Vec<SearchHit>> {
        assert_off_ui_thread("search");
        self.state
            .db
            .search(
                &q.query,
                q.offset,
                SEARCH_PAGE,
                q.person_id,
                q.track.filter(|t| *t == "mic" || *t == "loopback"),
                q.date_from.as_deref().filter(|s| !s.is_empty()),
                q.date_to.as_deref().filter(|s| !s.is_empty()),
            )
            .map_err(err)
    }

    /// Queue a full transcription (optionally with another engine).
    pub fn retranscribe(&self, meeting_id: i64, engine: Option<Engine>) -> CoreResult<()> {
        let state = &self.state;
        let backup_in_progress = state.backup_in_progress.lock().unwrap();
        if *backup_in_progress {
            return Err("A backup is in progress".into());
        }
        let meeting = self.meeting_or_not_found(meeting_id)?;
        if meeting.status == "recording" {
            return Err("Meeting is still recording".into());
        }
        state
            .pipeline
            .enqueue(Job {
                meeting_id,
                transcribe: true,
                engine,
            })
            .map_err(err)?;
        drop(backup_in_progress);
        Ok(())
    }

    /// Rendered transcript text (the UI copies it to the clipboard).
    pub fn transcript_text(&self, meeting_id: i64, format: ExportFormat) -> CoreResult<String> {
        crate::transcript_export::render(&self.state.db, meeting_id, format).map_err(err)
    }

    /// Render and write the transcript to a path the user picked. ⏳
    pub fn export_transcript(
        &self,
        meeting_id: i64,
        format: ExportFormat,
        dest: &Path,
    ) -> CoreResult<()> {
        assert_off_ui_thread("export_transcript");
        self.meeting_or_not_found(meeting_id)?;
        let text =
            crate::transcript_export::render(&self.state.db, meeting_id, format).map_err(err)?;
        std::fs::write(dest, text).map_err(err)
    }

    /// The meeting title with `<>:"/\|?*` replaced by `_`, for save-dialog
    /// default file names.
    pub fn suggested_file_stem(&self, meeting_id: i64) -> CoreResult<String> {
        Ok(sanitize_file_stem(
            &self.meeting_or_not_found(meeting_id)?.title,
        ))
    }

    /// Absolute path of the archived Opus file (player source).
    pub fn audio_path(&self, meeting_id: i64) -> CoreResult<PathBuf> {
        let meeting = self.meeting_or_not_found(meeting_id)?;
        let rel = meeting.audio_path.ok_or("No audio for this meeting yet")?;
        let audio_dir = self.state.settings.lock().unwrap().audio_dir();
        let path = safe_archive_path(&audio_dir, &rel)?;
        if !path.exists() {
            return Err("Audio file is missing".into());
        }
        Ok(path)
    }

    /// Copy the archived audio to a path the user picked. ⏳
    pub fn export_audio(&self, meeting_id: i64, dest: &Path) -> CoreResult<()> {
        assert_off_ui_thread("export_audio");
        let src = self.audio_path(meeting_id)?;
        std::fs::copy(&src, dest).map_err(err)?;
        log::info!("exported meeting {meeting_id} audio to {}", dest.display());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archived_audio_paths_cannot_escape_the_audio_directory() {
        let root = Path::new("audio-root");
        assert_eq!(
            safe_archive_path(root, "2026/meeting.opus").unwrap(),
            root.join("2026/meeting.opus")
        );
        for invalid in [
            "",
            ".",
            "../private.txt",
            "folder/../private.txt",
            "/absolute.opus",
            r"C:\\absolute.opus",
        ] {
            assert!(
                safe_archive_path(root, invalid).is_err(),
                "accepted invalid archive path {invalid:?}"
            );
        }
    }

    #[test]
    fn generic_names_do_not_enroll() {
        for name in ["Me", "them", "UNKNOWN", "Speaker 3", "speaker", "speaker12"] {
            assert!(is_generic_name(name), "{name}");
        }
        for name in ["Alice", "Speakerphone", "Speaker A"] {
            assert!(!is_generic_name(name), "{name}");
        }
    }

    #[test]
    fn file_stems_replace_reserved_characters() {
        assert_eq!(
            sanitize_file_stem(r#"Q3: plan <draft> "v2" a/b\c|d?e*"#),
            "Q3_ plan _draft_ _v2_ a_b_c_d_e_"
        );
    }
}
