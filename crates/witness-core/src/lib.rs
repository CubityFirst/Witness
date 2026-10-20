//! Witness core — local meeting recorder & transcriber, minus any UI.
//!
//! Everything the app does that is not drawing or OS-shell plumbing lives
//! here: capture, the processing pipeline, the database, models, and the
//! service layer ([`service::AppCore`]) that the native UI drives. Nothing in
//! this crate depends on egui, winit, a tray or dialogs; the UI receives
//! [`CoreEvent`]s through an [`EventSink`] and OS toasts go through a
//! [`Notifier`].

pub mod ai_client;
pub mod asr;
pub mod asr_parakeet;
pub mod asr_whisper;
pub mod audio_capture;
pub mod backup;
pub mod credentials;
pub mod db;
pub mod diarize;
pub mod encoder;
pub mod events;
pub mod gpu;
pub mod gpu_libs;
pub mod live_transcribe;
pub mod meeting_title;
pub mod meeting_watcher;
pub mod ml_scheduler;
pub mod models;
pub mod notify;
pub mod pipeline;
pub mod player;
pub mod recorder;
pub mod resample;
pub mod service;
pub mod settings;
pub mod speaker_id;
pub mod state;
pub mod transcript_export;
pub mod vad;

#[cfg(test)]
mod test_support;

pub use service::{
    AppCore, CoreEvent, CoreResult, EventSink, ExitGuard, ExitPurpose, Notifier, RecCmd,
    RecOutcome, Trigger,
};
