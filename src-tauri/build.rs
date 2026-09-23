// Keep this list in sync with the `generate_handler!` call in `src/main.rs`.
const COMMANDS: &[&str] = &[
    "get_status",
    "get_diagnostics",
    "export_diagnostics",
    "create_backup",
    "start_recording",
    "stop_recording",
    "show_captions_overlay",
    "list_meetings",
    "get_meeting",
    "delete_meeting",
    "restore_meeting",
    "list_deleted_meetings",
    "purge_meeting",
    "empty_recycle_bin",
    "rename_meeting",
    "set_meeting_notes",
    "bookmark_now",
    "add_bookmark",
    "set_bookmark_note",
    "delete_bookmark",
    "rename_speaker",
    "list_people",
    "delete_person",
    "get_people_stats",
    "search",
    "retranscribe",
    "get_settings",
    "update_settings",
    "pick_data_dir",
    "get_model_status",
    "download_models",
    "get_gpu_status",
    "get_gpu_libs_status",
    "download_gpu_libs",
    "list_audio_devices",
    "get_audio_url",
    "export_audio",
    "get_transcript_text",
    "export_transcript",
    "confirm_dialog",
    "get_hotkey",
    "get_bookmark_hotkey",
    "get_autostart",
    "set_autostart",
    "restart_app",
    "check_for_update",
    "install_update",
];

fn main() {
    // Register every application command with Tauri's ACL generator. Without an
    // app manifest, invoke-handler commands are implicitly available to every
    // webview, including the deliberately low-privilege captions overlay.

    tauri_build::try_build(
        tauri_build::Attributes::new()
            .app_manifest(tauri_build::AppManifest::new().commands(COMMANDS)),
    )
    .expect("failed to build Tauri application metadata");
}
