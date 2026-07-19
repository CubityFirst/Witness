// Typed wrappers around Tauri listen() — one function per backend event.
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type { WatcherStatus } from "./api";

export interface RecordingStarted {
  meeting_id: number;
  trigger: "auto" | "manual";
  started_at: string;
}

export interface RecordingLevel {
  mic_rms: number; // 0..1
  loopback_rms: number; // 0..1
  elapsed_ms: number;
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
