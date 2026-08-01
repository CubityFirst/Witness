import { useEffect, useState } from "preact/hooks";
import {
  deletePerson,
  downloadModels,
  exportDiagnostics,
  getAutostart,
  getDiagnostics,
  getGpuStatus,
  getHotkey,
  getModelStatus,
  getSettings,
  listAudioDevices,
  listPeople,
  pickDataDir,
  restartApp,
  setAutostart,
  updateSettings,
  type AudioDevices,
  type Diagnostics,
  type Engine,
  type GpuStatus,
  type ModelInfo,
  type Person,
  type SettingsData,
  type WatcherStatus,
} from "../lib/api";
import { onModelDownloadProgress, type ModelDownloadProgress } from "../lib/events";
import { Trash } from "../lib/icons";
import { appConfirm } from "../lib/confirm";
import { notifyError, notifySuccess } from "../lib/notify";

function configuredDeviceOption(
  configured: string | null | undefined,
  devices: AudioDevices["capture"],
) {
  if (!configured) return null;
  const byId = devices.find((device) => device.id === configured);
  if (byId) return { kind: "id" as const, device: byId };
  const lower = configured.toLowerCase();
  const byLegacyName = devices.find((device) => device.name.toLowerCase() === lower);
  return byLegacyName
    ? { kind: "legacy" as const, device: byLegacyName }
    : { kind: "missing" as const, device: null };
}

async function copyDiagnosticText(text: string) {
  if (navigator.clipboard?.writeText) {
    await navigator.clipboard.writeText(text);
    return;
  }
  const textarea = document.createElement("textarea");
  textarea.value = text;
  textarea.setAttribute("readonly", "");
  textarea.style.position = "fixed";
  textarea.style.opacity = "0";
  document.body.appendChild(textarea);
  textarea.select();
  const copied = document.execCommand("copy");
  textarea.remove();
  if (!copied) throw new Error("Clipboard access is unavailable");
}

export function SettingsView(props: { watcher: WatcherStatus | null }) {
  const [settings, setSettings] = useState<SettingsData | null>(null);
  const [settingsError, setSettingsError] = useState<string | null>(null);
  const [models, setModels] = useState<ModelInfo[]>([]);
  const [gpu, setGpu] = useState<GpuStatus | null>(null);
  const [dl, setDl] = useState<ModelDownloadProgress | null>(null);
  const [patternsText, setPatternsText] = useState("");
  const [junkPhrasesText, setJunkPhrasesText] = useState("");
  const [saved, setSaved] = useState(false);
  const [people, setPeople] = useState<Person[]>([]);
  const [devices, setDevices] = useState<AudioDevices | null>(null);
  const [devicesError, setDevicesError] = useState<string | null>(null);
  const [autostart, setAutostartState] = useState<boolean | null>(null);
  const [needsRestart, setNeedsRestart] = useState(false);
  const [hotkey, setHotkey] = useState<string | null>(null);
  const [diagnostics, setDiagnostics] = useState<Diagnostics | null>(null);
  const [diagnosticsError, setDiagnosticsError] = useState<string | null>(null);
  const [diagnosticsCopied, setDiagnosticsCopied] = useState(false);

  const refreshModels = () =>
    getModelStatus()
      .then(setModels)
      .catch((error) => notifyError("Could not load model status", error));
  const loadSettings = () => {
    setSettingsError(null);
    getSettings()
      .then((loaded) => {
        setSettings(loaded);
        setPatternsText(loaded.watch_patterns.join(", "));
        setJunkPhrasesText(loaded.junk_phrases.join("\n"));
      })
      .catch((error) => {
        const message = String(error);
        setSettingsError(message);
        notifyError("Could not load settings", error);
      });
  };
  const refreshDiagnostics = () => {
    setDiagnosticsError(null);
    getDiagnostics()
      .then(setDiagnostics)
      .catch((error) => setDiagnosticsError(String(error)));
  };

  useEffect(() => {
    loadSettings();
    refreshModels();
    getGpuStatus()
      .then(setGpu)
      .catch((error) => notifyError("Could not check GPU status", error));
    listPeople()
      .then(setPeople)
      .catch((error) => notifyError("Could not load enrolled people", error));
    listAudioDevices()
      .then(setDevices)
      .catch((error) => setDevicesError(String(error)));
    getAutostart()
      .then(setAutostartState)
      .catch((error) => notifyError("Could not read autostart status", error));
    getHotkey()
      .then(setHotkey)
      .catch((error) => notifyError("Could not read hotkey status", error));
    refreshDiagnostics();
    const un = onModelDownloadProgress((p) => {
      setDl(p.done && !p.error ? null : p);
      if (p.done) refreshModels();
    }).catch((error) => {
      notifyError("Could not connect to model download updates", error);
      return () => {};
    });
    return () => {
      un.then((u) => u()).catch((error) =>
        console.error("Could not detach model download updates", error),
      );
    };
  }, []);

  if (!settings) {
    return settingsError ? (
      <div class="error-state" role="alert">
        <p>Could not load settings: {settingsError}</p>
        <button class="btn" onClick={loadSettings}>Try again</button>
      </div>
    ) : (
      <div class="empty" role="status">Loading…</div>
    );
  }

  const micDevice = configuredDeviceOption(settings.mic_device, devices?.capture ?? []);
  const loopbackDevice = configuredDeviceOption(
    settings.loopback_device,
    devices?.render ?? [],
  );

  const save = (next: SettingsData) => {
    const previous = settings;
    setSettings(next);
    updateSettings(next)
      .then(() => {
        setSaved(true);
        setTimeout(() => setSaved(false), 1500);
      })
      .catch((error) => {
        setSettings(previous);
        notifyError("Could not save settings", error);
      });
  };

  const commitPatterns = () => {
    const patterns = patternsText
      .split(",")
      .map((p) => p.trim())
      .filter(Boolean);
    save({ ...settings, watch_patterns: patterns });
  };

  const commitJunkPhrases = () => {
    const phrases = junkPhrasesText
      .split("\n")
      .map((phrase) => phrase.trim())
      .filter(Boolean);
    save({ ...settings, junk_phrases: phrases });
  };

  const pickDir = () =>
    pickDataDir()
      .then((dir) => {
        if (dir && dir !== settings.data_dir) {
          save({ ...settings, data_dir: dir });
          setNeedsRestart(true);
        }
      })
      .catch((error) => notifyError("Could not choose a data directory", error));

  return (
    <div class="settings-view">
      <h2>Settings {saved && <span class="saved-flash">saved ✓</span>}</h2>

      <section>
        <h3>Storage</h3>
        <div class="setting-row">
          <label>Data directory</label>
          <code class="path">{settings.data_dir ?? "(next to witness.exe)"}</code>
          <button class="btn btn-ghost" onClick={pickDir}>
            Change…
          </button>
        </div>
        <p class="muted">
          Holds the database, recordings and models. Changing it does not move
          existing data.
        </p>
        {needsRestart && (
          <div class="setting-row">
            <span class="muted">The new data directory applies after a restart.</span>
            <button
              class="btn"
              onClick={() => restartApp().catch((error) => notifyError("Could not restart Witness", error))}
            >
              Restart Witness
            </button>
          </div>
        )}
        <div class="setting-row">
          <label>
            <input
              type="checkbox"
              checked={autostart ?? false}
              disabled={autostart == null}
              onChange={(e) => {
                const enabled = (e.target as HTMLInputElement).checked;
                setAutostart(enabled)
                  .then(() => setAutostartState(enabled))
                  .catch((error) => notifyError("Could not update autostart", error));
              }}
            />{" "}
            Start Witness with Windows (minimized to tray)
          </label>
        </div>
      </section>

      <section>
        <h3>Recording</h3>
        <div class="setting-row">
          <label>
            <input
              type="checkbox"
              checked={settings.auto_record}
              onChange={(e) =>
                save({ ...settings, auto_record: (e.target as HTMLInputElement).checked })
              }
            />{" "}
            Auto-record when a meeting is detected
          </label>
        </div>
        <div class="setting-row">
          <label>Watch patterns</label>
          <input
            class="text-input"
            value={patternsText}
            onInput={(e) => setPatternsText((e.target as HTMLInputElement).value)}
            onBlur={commitPatterns}
            onKeyDown={(e) => e.key === "Enter" && commitPatterns()}
          />
          <span class="muted">comma-separated, matched against mic-using app names</span>
        </div>
        <div class="setting-row">
          <label for="microphone-device">Microphone</label>
          <select
            id="microphone-device"
            class="text-input"
            value={settings.mic_device ?? ""}
            onChange={(e) =>
              save({
                ...settings,
                mic_device: (e.target as HTMLSelectElement).value || null,
              })
            }
          >
            <option value="">Default input device</option>
            {settings.mic_device && micDevice?.kind !== "id" && (
              <option value={settings.mic_device}>
                {micDevice?.kind === "legacy"
                  ? `${micDevice.device.name} (legacy name — reselect to pin this endpoint)`
                  : devices
                    ? `Missing configured device — ${settings.mic_device}`
                    : `Configured device — checking availability…`}
              </option>
            )}
            {(devices?.capture ?? []).map((d) => (
              <option value={d.id} key={d.id}>
                {d.name}
              </option>
            ))}
          </select>
          {devices && micDevice?.kind === "missing" && (
            <span class="badge badge-failed">configured microphone is not connected</span>
          )}
          {devices && micDevice?.kind === "legacy" && (
            <span class="muted">Stored by its old friendly name; reselect it to use the stable ID.</span>
          )}
        </div>
        <div class="setting-row">
          <label for="loopback-device">Meeting audio</label>
          <select
            id="loopback-device"
            class="text-input"
            value={settings.loopback_device ?? ""}
            onChange={(e) =>
              save({
                ...settings,
                loopback_device: (e.target as HTMLSelectElement).value || null,
              })
            }
          >
            <option value="">Default output device</option>
            {settings.loopback_device && loopbackDevice?.kind !== "id" && (
              <option value={settings.loopback_device}>
                {loopbackDevice?.kind === "legacy"
                  ? `${loopbackDevice.device.name} (legacy name — reselect to pin this endpoint)`
                  : devices
                    ? `Missing configured device — ${settings.loopback_device}`
                    : `Configured device — checking availability…`}
              </option>
            )}
            {(devices?.render ?? []).map((d) => (
              <option value={d.id} key={d.id}>
                {d.name}
              </option>
            ))}
          </select>
          {devices && loopbackDevice?.kind === "missing" && (
            <span class="badge badge-failed">configured output is not connected</span>
          )}
          {devices && loopbackDevice?.kind === "legacy" && (
            <span class="muted">Stored by its old friendly name; reselect it to use the stable ID.</span>
          )}
          <span class="muted">
            the output Teams plays through — pick a dedicated one (e.g.
            "Chat") to keep music &amp; game audio out of recordings
          </span>
        </div>
        {devicesError && <p class="badge badge-failed">Audio devices could not be listed: {devicesError}</p>}
        <div class="setting-row muted">
          Watcher: {props.watcher
            ? `Teams key found: ${props.watcher.teams_key_found ? "yes" : "no"} · mic in use: ${props.watcher.mic_in_use ? "yes" : "no"}${props.watcher.suppressed ? " · auto-restart suppressed" : ""}`
            : "…"}
        </div>
      </section>

      <section>
        <h3>Transcription</h3>
        <div class="setting-row">
          <label>
            <input
              type="checkbox"
              checked={settings.auto_transcribe}
              onChange={(e) =>
                save({ ...settings, auto_transcribe: (e.target as HTMLInputElement).checked })
              }
            />{" "}
            Auto-transcribe after each recording
          </label>
        </div>
        <div class="setting-row">
          <label>
            <input
              type="checkbox"
              checked={settings.live_transcribe}
              onChange={(e) =>
                save({ ...settings, live_transcribe: (e.target as HTMLInputElement).checked })
              }
            />{" "}
            Live captions while recording (Parakeet; provisional — the final
            pass replaces them)
          </label>
        </div>
        <div class="setting-row">
          <label>
            <input
              type="checkbox"
              checked={settings.caption_overlay}
              disabled={!settings.live_transcribe}
              onChange={(e) =>
                save({ ...settings, caption_overlay: (e.target as HTMLInputElement).checked })
              }
            />{" "}
            Floating caption window on top of other apps while recording
          </label>
        </div>
        <div class="setting-row">
          <label>Voice match strictness</label>
          <input
            type="range"
            min={0.4}
            max={0.8}
            step={0.05}
            value={settings.speaker_match_threshold}
            onChange={(e) =>
              save({
                ...settings,
                speaker_match_threshold: parseFloat((e.target as HTMLInputElement).value),
              })
            }
          />
          <span class="muted">
            {settings.speaker_match_threshold.toFixed(2)} — lower = more
            auto-labels (more mistakes), higher = fewer
          </span>
        </div>
        <div class="setting-row setting-row-top">
          <label for="junk-phrases">Discarded noise phrases</label>
          <textarea
            id="junk-phrases"
            class="text-input"
            rows={4}
            value={junkPhrasesText}
            onInput={(event) =>
              setJunkPhrasesText((event.target as HTMLTextAreaElement).value)
            }
            onBlur={commitJunkPhrases}
          />
          <span class="muted">
            one exact phrase per line; clear the list to disable phrase filtering
          </span>
        </div>
        <p class="muted">
          {hotkey
            ? `Global hotkey: ${hotkey.toUpperCase().replaceAll("+", " + ")} starts/stops recording.`
            : "Global record hotkey unavailable (all candidate combos are taken by other apps)."}
        </p>
        <div class="setting-row">
          <label>Engine</label>
          {(["parakeet", "whisper"] as Engine[]).map((eng) => (
            <label key={eng}>
              <input
                type="radio"
                name="engine"
                checked={settings.engine === eng}
                onChange={() => save({ ...settings, engine: eng })}
              />{" "}
              {eng === "parakeet" ? "Parakeet TDT v3 (fast, GPU)" : "Whisper large-v3-turbo"}
            </label>
          ))}
        </div>
        <div class="setting-row muted">
          GPU: {gpu ? gpu.detail : "checking…"}
        </div>
        <p class="muted">Speaker separation covers up to 4 remote speakers.</p>
      </section>

      <section>
        <h3>Models</h3>
        {models.map((mo) => (
          <div class="setting-row" key={mo.id}>
            <label>{mo.display_name}</label>
            {mo.present ? (
              <span
                class="badge badge-transcribed"
                title={`Verified revision ${mo.installed_revision}`}
              >
                installed{mo.size_mb != null ? ` · ${Math.round(mo.size_mb)} MB` : ""}
              </span>
            ) : dl && dl.model_id === mo.id ? (
              <span class="badge badge-processing">
                {dl.error
                  ? `failed: ${dl.error}`
                  : dl.total_bytes
                    ? `${Math.round((dl.downloaded_bytes / dl.total_bytes) * 100)}% of ${Math.round(dl.total_bytes / 1e6)} MB`
                    : `${Math.round(dl.downloaded_bytes / 1e6)} MB…`}
              </span>
            ) : (
              <>
                {mo.integrity_error && (
                  <span class="badge badge-failed" title={mo.integrity_error}>
                    integrity check failed
                  </span>
                )}
                <button
                  class="btn btn-ghost"
                  onClick={() =>
                    downloadModels(mo.engine).catch((error) =>
                      notifyError(`Could not download ${mo.display_name}`, error),
                    )
                  }
                >
                  {mo.integrity_error ? "Repair" : "Download"}
                </button>
              </>
            )}
            <code class="muted" title={`Expected revision ${mo.expected_revision}`}>
              rev {mo.expected_revision.slice(0, 8)}
            </code>
          </div>
        ))}
        <p class="muted">
          Recording works without models — they're only needed for
          transcription.
        </p>
      </section>

      <section>
        <h3>Diagnostics</h3>
        <p class="muted">
          This report contains operational state and local paths, but no audio,
          transcript text, notes, voice prints, or private settings.
        </p>
        {diagnosticsError && (
          <p class="badge badge-failed">Diagnostics unavailable: {diagnosticsError}</p>
        )}
        {diagnostics ? (
          <>
            <div class="setting-row">
              <label>Build</label>
              <span>
                Witness {diagnostics.app_version} · {diagnostics.platform}
              </span>
            </div>
            <div class="setting-row">
              <label>Runtime data</label>
              <code class="path">{diagnostics.runtime_data_dir}</code>
              {diagnostics.restart_required && (
                <span class="badge badge-processing">restart pending</span>
              )}
            </div>
            <div class="setting-row">
              <label>Database</label>
              <code class="path">{diagnostics.database.path}</code>
              <span
                class={`badge ${diagnostics.database.healthy ? "badge-transcribed" : "badge-failed"}`}
                title={diagnostics.database.detail}
              >
                {diagnostics.database.healthy ? "healthy" : "check failed"}
              </span>
            </div>
            <div class="setting-row">
              <label>Log</label>
              <code class="path">{diagnostics.log_path}</code>
            </div>
            <div class="setting-row">
              <label>Recording</label>
              <span>
                {diagnostics.recording_state}
                {diagnostics.recording_meeting_id != null
                  ? ` · meeting ${diagnostics.recording_meeting_id}`
                  : ""}
              </span>
            </div>
            <div class="setting-row">
              <label>Processing</label>
              <span>
                {diagnostics.processing_meeting_id == null
                  ? "idle"
                  : `meeting ${diagnostics.processing_meeting_id} · ${diagnostics.processing_stage ?? "starting"}${diagnostics.processing_pct == null ? "" : ` · ${diagnostics.processing_pct.toFixed(1)}%`}`}
                {` · ${diagnostics.queue_len} queued`}
              </span>
            </div>
            <div class="setting-row">
              <label>Capture health</label>
              <span>
                {diagnostics.capture_health
                  ? `${diagnostics.capture_health_is_current ? "current" : "last recording"}: mic ${diagnostics.capture_health.mic.connected ? "connected" : "disconnected"}, ${diagnostics.capture_health.mic.capture_dropped_ms} ms dropped · output ${diagnostics.capture_health.loopback.connected ? "connected" : "disconnected"}, ${diagnostics.capture_health.loopback.capture_dropped_ms} ms dropped`
                  : "not available in this session"}
              </span>
            </div>
            <div class="setting-row">
              <label>Model integrity</label>
              <span>
                {diagnostics.models.filter((model) => model.present).length}/
                {diagnostics.models.length} verified
                {diagnostics.models.some((model) => model.integrity_error)
                  ? " · one or more checks failed"
                  : ""}
              </span>
            </div>
            <div class="setting-row">
              <button class="btn btn-ghost" onClick={refreshDiagnostics}>
                Refresh
              </button>
              <button
                class="btn btn-ghost"
                onClick={() =>
                  copyDiagnosticText(diagnostics.report)
                    .then(() => {
                      setDiagnosticsCopied(true);
                      setTimeout(() => setDiagnosticsCopied(false), 1500);
                      notifySuccess("Diagnostic report copied");
                    })
                    .catch((error) => notifyError("Could not copy diagnostics", error))
                }
              >
                {diagnosticsCopied ? "Copied ✓" : "Copy report"}
              </button>
              <button
                class="btn btn-ghost"
                onClick={() =>
                  exportDiagnostics()
                    .then((exported) => {
                      if (exported) notifySuccess("Diagnostic report exported");
                    })
                    .catch((error) => notifyError("Could not export diagnostics", error))
                }
              >
                Export…
              </button>
              <span class="muted">
                generated {new Date(diagnostics.generated_at).toLocaleString()}
              </span>
            </div>
          </>
        ) : (
          !diagnosticsError && <p class="muted">Checking…</p>
        )}
      </section>

      <section>
        <h3>People (voice prints)</h3>
        {people.length === 0 && (
          <p class="muted">
            No one enrolled yet. Rename a speaker in any transcript to teach
            Witness their voice — future meetings will label them
            automatically.
          </p>
        )}
        {people.map((p) => (
          <div class="setting-row" key={p.id}>
            <label>{p.name}</label>
            <span class="muted">
              {Math.round(p.sample_seconds / 60)} min of speech learned
            </span>
            <button
              class="icon-btn"
              title="Forget this voice"
              onClick={async (e) => {
                if (
                  !e.shiftKey &&
                  !(await appConfirm(`Forget ${p.name}'s voice?\nExisting transcripts keep their names.`, "Forget voice"))
                )
                  return;
                deletePerson(p.id)
                  .then(() => setPeople((prev) => prev.filter((x) => x.id !== p.id)))
                  .catch((error) => notifyError(`Could not forget ${p.name}`, error));
              }}
            >
              <Trash size={15} />
            </button>
          </div>
        ))}
        {people.length > 0 && (
          <p class="muted">
            Speakers matched by voice show a ≈ before their name; renaming
            them confirms or corrects the match.
          </p>
        )}
      </section>
    </div>
  );
}
