//! Helpers shared by the ignored real-world smoke tests.

/// Where the smoke tests keep downloaded models: `WITNESS_MODELS_DIR`, or
/// `<target>/<profile>/models` next to the test binary. That keeps today's
/// meaning (`target/debug/models`) wherever the target dir lives — the cwd
/// is the package dir now and `CARGO_TARGET_DIR` may point anywhere.
pub(crate) fn models_dir() -> std::path::PathBuf {
    if let Some(dir) = std::env::var_os("WITNESS_MODELS_DIR") {
        return dir.into();
    }
    // test binary = <target>/<profile>/deps/<name>.exe → <target>/<profile>/models
    let exe = std::env::current_exe().expect("test exe path");
    exe.parent()
        .and_then(|deps| deps.parent())
        .expect("target profile dir")
        .join("models")
}
