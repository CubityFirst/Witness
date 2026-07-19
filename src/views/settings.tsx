import { useEffect, useState } from "preact/hooks";
import {
  deletePerson,
  downloadModels,
  getAutostart,
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

export function SettingsView(props: { watcher: WatcherStatus | null }) {
  const [settings, setSettings] = useState<SettingsData | null>(null);
  const [models, setModels] = useState<ModelInfo[]>([]);
  const [gpu, setGpu] = useState<GpuStatus | null>(null);
  const [dl, setDl] = useState<ModelDownloadProgress | null>(null);
  const [patternsText, setPatternsText] = useState("");
  const [saved, setSaved] = useState(false);
  const [people, setPeople] = useState<Person[]>([]);
  const [devices, setDevices] = useState<AudioDevices | null>(null);
  const [autostart, setAutostartState] = useState<boolean | null>(null);
  const [needsRestart, setNeedsRestart] = useState(false);
  const [hotkey, setHotkey] = useState<string | null>(null);

  const refreshModels = () => getModelStatus().then(setModels).catch(() => {});

  useEffect(() => {
    getSettings().then((s) => {
      setSettings(s);
      setPatternsText(s.watch_patterns.join(", "));
    });
    refreshModels();
    getGpuStatus().then(setGpu).catch(() => {});
    listPeople().then(setPeople).catch(() => {});
    listAudioDevices().then(setDevices).catch(() => {});
    getAutostart().then(setAutostartState).catch(() => {});
    getHotkey().then(setHotkey).catch(() => {});
    const un = onModelDownloadProgress((p) => {
      setDl(p.done && !p.error ? null : p);
      if (p.done) refreshModels();
    });
    return () => {
      un.then((u) => u());
    };
  }, []);

  if (!settings) return <div class="empty">Loading…</div>;

  const save = (next: SettingsData) => {
    setSettings(next);
    updateSettings(next)
      .then(() => {
        setSaved(true);
        setTimeout(() => setSaved(false), 1500);
      })
      .catch((e) => alert(String(e)));
  };

  const commitPatterns = () => {
    const patterns = patternsText
      .split(",")
      .map((p) => p.trim())
      .filter(Boolean);
    save({ ...settings, watch_patterns: patterns });
  };

  const pickDir = () =>
    pickDataDir().then((dir) => {
      if (dir && dir !== settings.data_dir) {
        save({ ...settings, data_dir: dir });
        setNeedsRestart(true);
      }
    });

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
            <button class="btn" onClick={() => restartApp()}>
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
                  .catch((err) => alert(String(err)));
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
          <label>Microphone</label>
          <select
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
            {(devices?.capture ?? []).map((d) => (
              <option value={d} key={d}>
                {d}
              </option>
            ))}
          </select>
        </div>
        <div class="setting-row">
          <label>Meeting audio</label>
          <select
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
            {(devices?.render ?? []).map((d) => (
              <option value={d} key={d}>
                {d}
              </option>
            ))}
          </select>
          <span class="muted">
            the output Teams plays through — pick a dedicated one (e.g.
            "Chat") to keep music &amp; game audio out of recordings
          </span>
        </div>
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
              <span class="badge badge-transcribed">
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
              <button class="btn btn-ghost" onClick={() => downloadModels(mo.engine)}>
                Download
              </button>
            )}
          </div>
        ))}
        <p class="muted">
          Recording works without models — they're only needed for
          transcription.
        </p>
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
              title="Forget this voice (shift-click: no confirm)"
              onClick={async (e) => {
                if (
                  !e.shiftKey &&
                  !(await appConfirm(`Forget ${p.name}'s voice?\nExisting transcripts keep their names.`, "Forget voice"))
                )
                  return;
                deletePerson(p.id).then(() =>
                  setPeople((prev) => prev.filter((x) => x.id !== p.id)),
                );
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
