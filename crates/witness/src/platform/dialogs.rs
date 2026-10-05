//! Native save / folder dialogs (`rfd`), parented to the main window. They
//! block, so they only ever run inside a worker closure (`Tasks::spawn` /
//! `spawn_op`), never on the UI thread (design §7).
//!
//! STUB (F0.2): the signatures are final; the OS-services unit (U9)
//! implements them. Until then every dialog reports "cancelled".

use super::OwnerHwnd;
use std::path::PathBuf;

/// A save-file dialog request, e.g. `{stem}.opus` / "Ogg Opus audio" / ["opus"].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveFileRequest {
    /// Suggested file name, including the extension.
    pub file_name: String,
    /// Filter label shown in the type drop-down.
    pub filter_name: String,
    /// Extensions without the dot.
    pub extensions: Vec<String>,
}

/// Show a modal save dialog; None when cancelled. TODO(U9)
pub fn save_file_blocking(_owner: OwnerHwnd, _request: SaveFileRequest) -> Option<PathBuf> {
    None
}

/// Show a modal folder picker; None when cancelled. TODO(U9)
pub fn pick_folder_blocking(_owner: OwnerHwnd, _title: Option<&str>) -> Option<PathBuf> {
    None
}
