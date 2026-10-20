//! Complete, checksummed recovery snapshots.
//!
//! A backup is assembled in a newly-created sibling staging directory and is
//! only made visible under its final name after every payload file and the
//! manifest have been written. Failures clean up only that staging directory.

use crate::db::Db;
use crate::models::ModelInfo;
use crate::settings::Settings;
use anyhow::{bail, ensure, Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};

const FORMAT_VERSION: u32 = 1;

pub struct BackupInput<'a> {
    pub db: &'a Db,
    pub settings: &'a Settings,
    pub data_dir: &'a Path,
    pub model_status: &'a [ModelInfo],
    pub app_version: &'a str,
}

#[derive(Debug, Clone, Serialize)]
pub struct BackupSummary {
    pub path: String,
    pub created_at: String,
    pub file_count: usize,
    pub total_bytes: u64,
}

#[derive(Serialize)]
struct Manifest<'a> {
    format_version: u32,
    created_at: &'a str,
    app_version: &'a str,
    database: &'static str,
    settings: &'static str,
    audio_directory: &'static str,
    recovery_recordings_directory: &'static str,
    model_metadata: &'static str,
    model_binaries_included: bool,
    gpu_libraries_included: bool,
    files: &'a [ManifestFile],
}

#[derive(Debug, Serialize)]
struct ManifestFile {
    path: String,
    size: u64,
    sha256: String,
}

struct StagingDirectory {
    path: PathBuf,
    published: bool,
}

impl Drop for StagingDirectory {
    fn drop(&mut self) {
        if !self.published {
            if let Err(error) = std::fs::remove_dir_all(&self.path) {
                log::warn!(
                    "could not remove incomplete backup {}: {error}",
                    self.path.display()
                );
            }
        }
    }
}

pub fn create(parent: &Path, input: &BackupInput<'_>) -> Result<BackupSummary> {
    let parent = validate_parent(parent, input.data_dir)?;
    let created_at = chrono::Local::now().to_rfc3339();
    let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let (mut staging, final_path) = allocate_directories(&parent, &timestamp)?;
    let mut files = Vec::new();

    let audio_destination = staging.path.join("audio");
    std::fs::create_dir(&audio_destination).context("creating backup audio directory")?;
    copy_tree(
        &input.data_dir.join("audio"),
        &audio_destination,
        Path::new("audio"),
        &mut files,
    )?;

    let recovery_destination = staging.path.join("rec-tmp");
    std::fs::create_dir(&recovery_destination)
        .context("creating backup recovery-recordings directory")?;
    copy_tree(
        &input.data_dir.join("rec-tmp"),
        &recovery_destination,
        Path::new("rec-tmp"),
        &mut files,
    )?;

    let settings_path = staging.path.join("witness-settings.toml");
    input
        .settings
        .save_to_path(&settings_path)
        .map_err(anyhow::Error::msg)?;
    files.push(describe_file(&staging.path, &settings_path)?);

    let model_status_path = staging.path.join("model-status.json");
    write_json(&model_status_path, input.model_status)?;
    files.push(describe_file(&staging.path, &model_status_path)?);

    // Snapshot the DB after copying audio. With recording starts and pipeline
    // enqueues gated by the caller, the DB can never reference a newly-created
    // archive that was not copied. Concurrent deletion can only leave a safe,
    // unreferenced extra audio file in the backup.
    let database_path = staging.path.join("witness.db");
    input.db.backup_to(&database_path)?;
    files.push(describe_file(&staging.path, &database_path)?);

    files.sort_by(|left, right| left.path.cmp(&right.path));
    let total_bytes = files.iter().map(|file| file.size).sum();
    let file_count = files.len();
    let manifest = Manifest {
        format_version: FORMAT_VERSION,
        created_at: &created_at,
        app_version: input.app_version,
        database: "witness.db",
        settings: "witness-settings.toml",
        audio_directory: "audio/",
        recovery_recordings_directory: "rec-tmp/",
        model_metadata: "model-status.json",
        model_binaries_included: false,
        gpu_libraries_included: false,
        files: &files,
    };
    write_json(&staging.path.join("backup-manifest-v1.json"), &manifest)?;

    std::fs::rename(&staging.path, &final_path)
        .with_context(|| format!("publishing completed backup {}", final_path.display()))?;
    staging.published = true;

    Ok(BackupSummary {
        path: final_path.display().to_string(),
        created_at,
        file_count,
        total_bytes,
    })
}

fn validate_parent(parent: &Path, data_dir: &Path) -> Result<PathBuf> {
    ensure!(
        parent.is_dir(),
        "backup destination must be an existing folder"
    );
    let parent = parent
        .canonicalize()
        .with_context(|| format!("resolving backup destination {}", parent.display()))?;
    let data_dir = data_dir
        .canonicalize()
        .with_context(|| format!("resolving Witness data directory {}", data_dir.display()))?;
    ensure!(
        !is_same_or_descendant(&parent, &data_dir),
        "choose a backup destination outside the Witness data directory"
    );
    Ok(parent)
}

#[cfg(windows)]
fn is_same_or_descendant(path: &Path, ancestor: &Path) -> bool {
    let path = path.to_string_lossy().replace('/', "\\");
    let ancestor = ancestor.to_string_lossy().replace('/', "\\");
    let path = path.trim_end_matches('\\');
    let ancestor = ancestor.trim_end_matches('\\');
    path.eq_ignore_ascii_case(ancestor)
        || path
            .get(..ancestor.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(ancestor))
            && path.as_bytes().get(ancestor.len()) == Some(&b'\\')
}

#[cfg(not(windows))]
fn is_same_or_descendant(path: &Path, ancestor: &Path) -> bool {
    path.starts_with(ancestor)
}

fn allocate_directories(parent: &Path, timestamp: &str) -> Result<(StagingDirectory, PathBuf)> {
    for suffix in 0..100u8 {
        let suffix = if suffix == 0 {
            String::new()
        } else {
            format!("-{suffix}")
        };
        let final_path = parent.join(format!("Witness-backup-{timestamp}{suffix}"));
        let staging_path = parent.join(format!(".Witness-backup-{timestamp}{suffix}.incomplete"));
        if final_path.exists() || staging_path.exists() {
            continue;
        }
        match std::fs::create_dir(&staging_path) {
            Ok(()) => {
                return Ok((
                    StagingDirectory {
                        path: staging_path,
                        published: false,
                    },
                    final_path,
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).context("creating incomplete backup directory");
            }
        }
    }
    bail!("could not allocate a unique backup directory")
}

fn copy_tree(
    source: &Path,
    destination: &Path,
    relative_destination: &Path,
    files: &mut Vec<ManifestFile>,
) -> Result<()> {
    if !source.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(source)
        .with_context(|| format!("reading source directory {}", source.display()))?
    {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let relative_path = relative_destination.join(entry.file_name());
        if file_type.is_symlink() {
            bail!(
                "backup source contains an unsupported link: {}",
                source_path.display()
            );
        }
        if file_type.is_dir() {
            std::fs::create_dir(&destination_path).with_context(|| {
                format!("creating backup directory {}", destination_path.display())
            })?;
            copy_tree(&source_path, &destination_path, &relative_path, files)?;
        } else if file_type.is_file() {
            copy_file(&source_path, &destination_path)?;
            files.push(describe_file_from_relative(
                &destination_path,
                &relative_path,
            )?);
        } else {
            bail!(
                "backup source contains an unsupported filesystem entry: {}",
                source_path.display()
            );
        }
    }
    Ok(())
}

fn copy_file(source: &Path, destination: &Path) -> Result<()> {
    let mut source = BufReader::new(
        File::open(source).with_context(|| format!("opening {}", source.display()))?,
    );
    let mut destination_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .with_context(|| format!("creating {}", destination.display()))?;
    io::copy(&mut source, &mut destination_file)
        .with_context(|| format!("copying backup payload to {}", destination.display()))?;
    destination_file
        .sync_all()
        .with_context(|| format!("flushing {}", destination.display()))?;
    Ok(())
}

fn write_json<T: Serialize + ?Sized>(path: &Path, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.write_all(&bytes)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

fn describe_file(root: &Path, path: &Path) -> Result<ManifestFile> {
    let relative = path.strip_prefix(root).with_context(|| {
        format!(
            "backup payload {} escaped staging directory {}",
            path.display(),
            root.display()
        )
    })?;
    describe_file_from_relative(path, relative)
}

fn describe_file_from_relative(path: &Path, relative: &Path) -> Result<ManifestFile> {
    let size = path.metadata()?.len();
    let mut reader = BufReader::new(File::open(path)?);
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(ManifestFile {
        path: relative.to_string_lossy().replace('\\', "/"),
        size,
        sha256: hex::encode(digest.finalize()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "witness-backup-{label}-{}-{id}",
                std::process::id()
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn publishes_complete_recoverable_snapshot_with_manifest() {
        let data = TestDir::new("data");
        let destination = TestDir::new("destination");
        std::fs::create_dir(data.0.join("audio")).unwrap();
        std::fs::create_dir(data.0.join("rec-tmp")).unwrap();
        std::fs::create_dir(data.0.join("models")).unwrap();
        std::fs::write(data.0.join("audio/7.opus"), b"archived audio").unwrap();
        std::fs::write(data.0.join("rec-tmp/8.meta.toml"), b"meeting_id = 8").unwrap();
        let db = Db::open(&data.0.join("witness.db")).unwrap();
        let meeting_id = db
            .create_meeting("Recovery test", "2026-08-01T10:00:00Z", "manual")
            .unwrap();
        db.replace_transcript(
            meeting_id,
            &[crate::db::NewSpeaker {
                label: "S1".into(),
                display_name: "Speaker 1".into(),
                embedding: Some(vec![0.6, 0.8]),
                emb_seconds: 12.0,
                person_id: None,
                auto_labeled: false,
            }],
            &[],
            "parakeet",
        )
        .unwrap();
        let speaker_id = db.get_speakers(meeting_id).unwrap()[0].id;
        db.rename_speaker(speaker_id, "Alice", true).unwrap();
        let settings = Settings {
            data_dir: Some(data.0.display().to_string()),
            ..Settings::default()
        };

        let summary = create(
            &destination.0,
            &BackupInput {
                db: &db,
                settings: &settings,
                data_dir: &data.0,
                model_status: &[],
                app_version: "test",
            },
        )
        .unwrap();
        let backup = PathBuf::from(&summary.path);

        assert!(backup.join("backup-manifest-v1.json").is_file());
        assert_eq!(
            std::fs::read(backup.join("audio/7.opus")).unwrap(),
            b"archived audio"
        );
        assert!(backup.join("rec-tmp/8.meta.toml").is_file());
        assert!(backup.join("witness-settings.toml").is_file());
        let restored = Db::open(&backup.join("witness.db")).unwrap();
        assert_eq!(
            restored.list_meetings(0, 10).unwrap()[0].title,
            "Recovery test"
        );
        let restored_people = restored.list_people().unwrap();
        assert_eq!(restored_people[0].name, "Alice");
        assert_eq!(restored_people[0].embedding, vec![0.6, 0.8]);
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(backup.join("backup-manifest-v1.json")).unwrap())
                .unwrap();
        assert_eq!(manifest["format_version"], 1);
        assert_eq!(manifest["model_binaries_included"], false);
        assert_eq!(manifest["gpu_libraries_included"], false);
        assert!(manifest["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|file| file["path"] == "witness.db"));
        assert!(destination.0.read_dir().unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains("incomplete")));
    }

    #[test]
    fn rejects_destination_inside_data_directory() {
        let data = TestDir::new("nested-data");
        let nested = data.0.join("backups");
        std::fs::create_dir(&nested).unwrap();
        let error = validate_parent(&nested, &data.0).unwrap_err().to_string();
        assert!(error.contains("outside the Witness data directory"));
    }

    #[test]
    fn refuses_links_in_payload_instead_of_following_them() {
        let data = TestDir::new("links");
        let destination = TestDir::new("link-destination");
        std::fs::create_dir(data.0.join("audio")).unwrap();
        let source = data.0.join("source.opus");
        std::fs::write(&source, b"private").unwrap();
        #[cfg(windows)]
        if let Err(error) =
            std::os::windows::fs::symlink_file(&source, data.0.join("audio/link.opus"))
        {
            // Creating symlinks normally needs Developer Mode or an elevated
            // token on Windows. The production rejection still applies; skip
            // only when this machine cannot construct the test fixture.
            if error.raw_os_error() == Some(1314) {
                return;
            }
            panic!("could not create symlink fixture: {error}");
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&source, data.0.join("audio/link.opus")).unwrap();
        let mut files = Vec::new();
        let error = copy_tree(
            &data.0.join("audio"),
            &destination.0,
            Path::new("audio"),
            &mut files,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("unsupported link"));
    }
}
