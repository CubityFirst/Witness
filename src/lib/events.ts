// Typed wrappers around Tauri listen() — one function per backend event.
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type { WatcherStatus } from "./api";

export interface RecordingStarted {
  meeting_id: number;
  trigger: "auto" | "manual";
  started_at: string;
  live_captions: boolean;
}

export interface RecordingLevel {
  mic_rms: number; // 0..1
  loopback_rms: number; // 0..1
  elapsed_ms: number;
}

export interface TrackHealth {
  connected: boolean;
  device_loss_count: number;
  capture_overflow_count: number;
  capture_dropped_ms: number;
  live_dropped_ms: number;
  live_disconnected: boolean;
  fatal_error: string | null;
}

export interface RecordingHealth {
  mic: TrackHealth;
  loopback: TrackHealth;
  writer_error: string | null;
}

export type Stage = "vad" | "asr" | "diarize" | "encode";

export interface TranscriptionProgress {
  meeting_id: number;
  stage: Stage;
  pct: number; // 0..100
}

export interface TranscriptionDone {
  meeting_id: number;
  error: string | null;
}

export interface ModelDownloadProgress {
  model_id: string;
  file: string;
  file_index: number; // 1-based position within the download job
  file_count: number;
  downloaded_bytes: number;
  total_bytes: number | null;
  done: boolean;
  error: string | null;
}

const on = <T,>(event: string) => (cb: (payload: T) => void): Promise<UnlistenFn> =>
  listen<T>(event, (e) => cb(e.payload));

export const onRecordingStarted = on<RecordingStarted>("recording-started");
export const onRecordingStopped = on<{ meeting_id: number }>("recording-stopped");
export const onRecordingLevel = on<RecordingLevel>("recording-level");
export const onRecordingHealth = on<RecordingHealth>("recording-health");
export const onWatcherStatus = on<WatcherStatus>("watcher-status");
export const onTranscriptionProgress = on<TranscriptionProgress>("transcription-progress");
export const onTranscriptionComplete = on<TranscriptionDone>("transcription-complete");
export const onTranscriptionFailed = on<TranscriptionDone>("transcription-failed");
export const onModelDownloadProgress = on<ModelDownloadProgress>("model-download-progress");
export const onMeetingsChanged = on<void>("meetings-changed");

export interface LiveTranscript {
  meeting_id: number;
  track: "mic" | "loopback";
  start_ms: number;
  end_ms: number;
  text: string;
}

export const onLiveTranscript = on<LiveTranscript>("live-transcript");

export interface LiveCaptionsStatus {
  meeting_id: number;
  active: boolean;
  error: string | null;
}

export const onLiveCaptionsStatus =
  on<LiveCaptionsStatus>("live-captions-status");

/** True when the window was just shown as a tray flyout (blur dismisses). */
export const onTrayPopup = on<boolean>("tray-popup");

export interface UpdateProgress {
  downloaded: number;
  total: number | null;
}
/** Byte progress while an app update downloads (install follows on its own). */
export const onUpdateProgress = on<UpdateProgress>("update-progress");
