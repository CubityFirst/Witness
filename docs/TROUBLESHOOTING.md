# Troubleshooting

**Settings > About > Diagnostics** shows the active paths, database health,
capture loss and model integrity. Its copyable report holds no meeting
content, but does include local paths — review it before sharing.

- **Transcription is slow.** It is probably running on the CPU. Check the GPU
  line in Settings > Transcription; if libraries are missing, click **GPU
  libraries > Download** (~1 GB, no admin rights, takes effect immediately).
  An existing cuDNN 9 for CUDA 13 install is picked up from `CUDNN_PATH` or
  the standard `NVIDIA\CUDNN` folder. `witness.log` names any missing DLLs.
- **The call's side of the recording is silent.** Teams isn't playing
  through the output device Witness captures. Route Teams to that device.
- **Other sounds were recorded.** Witness captures everything sent to the
  selected output device. Give Teams its own (virtual or hardware) output.
- **A configured audio device is missing.** Reconnect it or pick another in
  Settings. Witness never silently falls back to a different device.
- **Model integrity check failed.** Click **Repair** in Settings > Storage.
- **A meeting failed to transcribe.** Use **Retranscribe**; don't purge it
  until that works.
- **The new data directory is empty.** Changing it doesn't move existing
  data. Switch back, or restore a backup into it
  ([Backup and recovery](BACKUP_AND_RECOVERY.md)).
