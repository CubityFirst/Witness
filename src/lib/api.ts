// Typed wrappers around Tauri invoke() — one function per backend command.
import { invoke, convertFileSrc } from "@tauri-apps/api/core";

export type MeetingStatus =
  | "recording"
  | "recorded"
  | "processing"
  | "transcribed"
  | "failed";

export type Engine = "parakeet" | "whisper";

export interface Meeting {
  id: number;
  started_at: string; // RFC 3339 local time
  ended_at: string | null;
  title: string;
  audio_path: string | null;
  duration_ms: number | null;
  status: MeetingStatus;
  engine: string | null;
  trigger: "auto" | "manual";
  notes: string;
}

export interface Bookmark {
  id: number;
  meeting_id: number;
  at_ms: number;
  note: string;
}

export interface Speaker {
  id: number;
  meeting_id: number;
  label: string; // 'me' | 'S1'..'S8' (immutable)
  display_name: string;
  person_id: number | null;
  auto_labeled: boolean; // name came from voice matching, not the user
}

export interface Person {
  id: number;
  name: string;
  sample_seconds: number;
  created_at: string;
}

export interface Segment {
  id: number;
  meeting_id: number;
  speaker_id: number | null;
  track: "mic" | "loopback";
  start_ms: number;
  end_ms: number;
  text: string;
}

export interface MeetingDetail {
  meeting: Meeting;
  speakers: Speaker[];
  segments: Segment[];
  bookmarks: Bookmark[];
  last_error: string | null;
  pipeline_queued: boolean;
}

export interface WatcherStatus {
  enabled: boolean;
  teams_key_found: boolean;
  mic_in_use: boolean;
  suppressed: boolean;
}

export interface AppStatus {
  recording: boolean;
  meeting_id: number | null;
  recording_since: string | null;
  live_captions: boolean;
  transcribing_meeting_id: number | null;
  queue_len: number;
  processing_stage: string | null;
  processing_pct: number | null;
  watcher: WatcherStatus;
}

export interface SettingsData {
  data_dir: string | null;
  engine: Engine;
  auto_record: boolean;
  auto_transcribe: boolean;
  live_transcribe: boolean;
  caption_overlay: boolean;
  check_for_updates: boolean;
  speaker_match_threshold: number;
  watch_patterns: string[];
  junk_phrases: string[];
  opus_bitrate_kbps: number;
  mic_device?: string | null;
  loopback_device?: string | null;
}

export interface SearchFilters {
  personId?: number | null;
  track?: "mic" | "loopback" | null;
  dateFrom?: string | null; // YYYY-MM-DD
  dateTo?: string | null;
}

export interface PersonMeetingStat {
  person_id: number | null; // null = Me
  person_name: string;
  meeting_id: number;
  meeting_title: string;
  started_at: string;
  duration_ms: number;
  talk_ms: number;
  words: number;
  segments: number;
}

export interface AudioDevice {
  id: string;
  name: string;
}

export interface AudioDevices {
  render: AudioDevice[];
  capture: AudioDevice[];
}

export interface DiagnosticDatabase {
  path: string;
  healthy: boolean;
  detail: string;
}

export interface DiagnosticModel {
  id: string;
  present: boolean;
  expected_revision: string;
  installed_revision: string | null;
  integrity_error: string | null;
}

export interface DiagnosticGpu {
  driver_present: boolean;
  cuda_ready: boolean;
  missing_dlls: string[];
  managed_libraries_present: boolean;
  managed_libraries_integrity_error: string | null;
}

export interface DiagnosticTrackHealth {
  connected: boolean;
  device_loss_count: number;
  capture_overflow_count: number;
  capture_dropped_ms: number;
  live_dropped_ms: number;
  live_disconnected: boolean;
  fatal_error: string | null;
}

export interface Diagnostics {
  generated_at: string;
  app_version: string;
  platform: string;
  runtime_data_dir: string;
  configured_data_dir: string | null;
  restart_required: boolean;
  database: DiagnosticDatabase;
  log_path: string;
  recording_state: string;
  recording_meeting_id: number | null;
  recording_since: string | null;
  capture_health: {
    mic: DiagnosticTrackHealth;
    loopback: DiagnosticTrackHealth;
    writer_error: string | null;
  } | null;
  capture_health_is_current: boolean;
  processing_meeting_id: number | null;
  processing_stage: string | null;
  processing_pct: number | null;
  queue_len: number;
  models: DiagnosticModel[];
  gpu: DiagnosticGpu;
  report: string;
}

export interface BackupSummary {
  path: string;
  created_at: string;
  file_count: number;
  total_bytes: number;
}

export interface SearchHit {
  meeting_id: number;
  meeting_title: string;
  started_at: string;
  segment_id: number;
  start_ms: number;
  speaker_name: string | null;
  snippet: string; // contains <mark> spans
}

export interface ModelInfo {
  id: string; // "parakeet" | "sortformer" | "whisper"
  engine: Engine;
  display_name: string;
  present: boolean;
  size_mb: number | null;
  expected_revision: string;
  installed_revision: string | null;
  integrity_error: string | null;
}

export interface GpuStatus {
  cuda_available: boolean;
  detail: string;
}

export interface GpuLibsInfo {
  present: boolean;
  size_mb: number | null;
  integrity_error: string | null;
  driver_present: boolean;
  cuda_ready: boolean;
  download_mb: number;
}

export const getStatus = () => invoke<AppStatus>("get_status");
export const startRecording = () => invoke<number>("start_recording");
export const stopRecording = () => invoke<void>("stop_recording");

export const listMeetings = (offset: number, limit: number) =>
  invoke<Meeting[]>("list_meetings", { offset, limit });
export const getMeeting = (id: number) =>
  invoke<MeetingDetail>("get_meeting", { id });
export interface DeletedMeeting extends Meeting {
  deleted_at: string;
}

/** Moves a meeting to the recycle bin (auto-purged after 30 days). */
export const deleteMeeting = (id: number) =>
  invoke<void>("delete_meeting", { id });
export const restoreMeeting = (id: number) =>
  invoke<void>("restore_meeting", { id });
export const listDeletedMeetings = () =>
  invoke<DeletedMeeting[]>("list_deleted_meetings");
/** Permanent, unrecoverable delete (audio + transcript). */
export const purgeMeeting = (id: number) =>
  invoke<void>("purge_meeting", { id });
export const emptyRecycleBin = () => invoke<void>("empty_recycle_bin");
export const renameMeeting = (id: number, title: string) =>
  invoke<void>("rename_meeting", { id, title });
export const setMeetingNotes = (meetingId: number, notes: string) =>
  invoke<void>("set_meeting_notes", { meetingId, notes });

/** Flag the current moment of the active recording. */
export const bookmarkNow = () => invoke<void>("bookmark_now");
export const addBookmark = (meetingId: number, atMs: number, note: string) =>
  invoke<number>("add_bookmark", { meetingId, atMs, note });
export const setBookmarkNote = (bookmarkId: number, note: string) =>
  invoke<void>("set_bookmark_note", { bookmarkId, note });
export const deleteBookmark = (bookmarkId: number) =>
  invoke<void>("delete_bookmark", { bookmarkId });
export const renameSpeaker = (speakerId: number, name: string) =>
  invoke<void>("rename_speaker", { speakerId, name });
export const listPeople = () => invoke<Person[]>("list_people");
export const deletePerson = (personId: number) =>
  invoke<void>("delete_person", { personId });
export const getPeopleStats = () =>
  invoke<PersonMeetingStat[]>("get_people_stats");

export const search = (query: string, offset: number, filters?: SearchFilters) =>
  invoke<SearchHit[]>("search", {
    query,
    offset,
    personId: filters?.personId ?? null,
    track: filters?.track ?? null,
    dateFrom: filters?.dateFrom ?? null,
    dateTo: filters?.dateTo ?? null,
  });

export const retranscribe = (meetingId: number, engine?: Engine) =>
  invoke<void>("retranscribe", { meetingId, engine: engine ?? null });

export const getSettings = () => invoke<SettingsData>("get_settings");
export const updateSettings = (settings: SettingsData) =>
  invoke<void>("update_settings", { settings });
export const pickDataDir = () => invoke<string | null>("pick_data_dir");

export const listAudioDevices = () => invoke<AudioDevices>("list_audio_devices");
export const getDiagnostics = () => invoke<Diagnostics>("get_diagnostics");
export const exportDiagnostics = () => invoke<boolean>("export_diagnostics");
export const createBackup = () =>
  invoke<BackupSummary | null>("create_backup");
export const getModelStatus = () => invoke<ModelInfo[]>("get_model_status");
export const downloadModels = (engine: Engine) =>
  invoke<void>("download_models", { engine });
export const getGpuStatus = () => invoke<GpuStatus>("get_gpu_status");
export const getGpuLibsStatus = () =>
  invoke<GpuLibsInfo>("get_gpu_libs_status");
/** Installs the pinned CUDA runtime + cuDNN DLLs into the data directory.
 * Progress arrives on the model-download event stream as id "gpu-libs". */
export const downloadGpuLibs = () => invoke<void>("download_gpu_libs");

/** Opens a native save dialog and copies the meeting audio there. */
export const exportAudio = (meetingId: number) =>
  invoke<boolean>("export_audio", { meetingId });

export type TranscriptFormat = "md" | "txt" | "srt" | "vtt";
export const getTranscriptText = (meetingId: number, format: TranscriptFormat) =>
  invoke<string>("get_transcript_text", { meetingId, format });
export const exportTranscript = (meetingId: number, format: TranscriptFormat) =>
  invoke<boolean>("export_transcript", { meetingId, format });

/** Native yes/no dialog — window.confirm() is suppressed in the webview. */
export const confirmDialog = (message: string) =>
  invoke<boolean>("confirm_dialog", { message });

export const getHotkey = () => invoke<string | null>("get_hotkey");
export const getBookmarkHotkey = () => invoke<string | null>("get_bookmark_hotkey");
/** Reopens (or focuses) the floating caption overlay; errors when not recording. */
export const showCaptionsOverlay = () => invoke<void>("show_captions_overlay");
export const getAutostart = () => invoke<boolean>("get_autostart");
export const setAutostart = (enabled: boolean) =>
  invoke<void>("set_autostart", { enabled });
export const restartApp = () => invoke<void>("restart_app");

export async function getAudioUrl(meetingId: number): Promise<string> {
  const path = await invoke<string>("get_audio_url", { meetingId });
  return convertFileSrc(path);
}

export interface UpdateInfo {
  current_version: string;
  version: string;
  notes: string | null;
  date: string | null;
}
/** Asks the release feed for a newer signed build; null = up to date. */
export const checkForUpdate = () => invoke<UpdateInfo | null>("check_for_update");
/** Downloads and verifies the pending update, then hands over to the
 * installer — Witness exits and the installer relaunches it. */
export const installUpdate = () => invoke<void>("install_update");
