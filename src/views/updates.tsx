import { useEffect, useState } from "preact/hooks";
import { checkForUpdate, installUpdate, type SettingsData, type UpdateInfo } from "../lib/api";
import { onUpdateProgress, type UpdateProgress } from "../lib/events";
import { notifyError } from "../lib/notify";

type Phase =
  | { kind: "idle" }
  | { kind: "checking" }
  | { kind: "current" }
  | { kind: "available"; info: UpdateInfo }
  | { kind: "installing"; info: UpdateInfo; progress: UpdateProgress | null };

/** Settings → Updates: signed releases from GitHub, installed on a click.
 * The installer closes Witness, updates in place and relaunches it. */
export function UpdatesSection({
  settings,
  save,
  appVersion,
}: {
  settings: SettingsData;
  save: (next: SettingsData) => void;
  appVersion: string | null;
}) {
  const [phase, setPhase] = useState<Phase>({ kind: "idle" });

  useEffect(() => {
    const un = onUpdateProgress((progress) =>
      setPhase((current) =>
        current.kind === "installing" ? { ...current, progress } : current,
      ),
    ).catch((error) => {
      notifyError("Could not connect to update progress", error);
      return () => {};
    });
    return () => {
      un.then((u) => u()).catch((error) =>
        console.error("Could not detach update progress", error),
      );
    };
  }, []);

  const check = () => {
    setPhase({ kind: "checking" });
    checkForUpdate()
      .then((info) => setPhase(info ? { kind: "available", info } : { kind: "current" }))
      .catch((error) => {
        setPhase({ kind: "idle" });
        notifyError("Could not check for updates", error);
      });
  };

  const install = (info: UpdateInfo) => {
    setPhase({ kind: "installing", info, progress: null });
    // Resolves only on failure — on success the process exits.
    installUpdate().catch((error) => {
      setPhase({ kind: "available", info });
      notifyError("Could not install the update", error);
    });
  };

  return (
    <section>
      <h3>Updates</h3>
      <div class="setting-row">
        <label>
          Witness {appVersion ?? "…"}
          {phase.kind === "current" && " — up to date"}
        </label>
        {phase.kind === "available" ? (
          <button class="btn" onClick={() => install(phase.info)}>
            Install {phase.info.version} &amp; restart
          </button>
        ) : phase.kind === "installing" ? (
          <span class="badge badge-processing">
            {phase.progress?.total
              ? `downloading ${Math.round((phase.progress.downloaded / phase.progress.total) * 100)}%`
              : phase.progress
                ? `downloading ${Math.round(phase.progress.downloaded / 1e6)} MB…`
                : "starting…"}
          </span>
        ) : (
          <button class="btn btn-ghost" disabled={phase.kind === "checking"} onClick={check}>
            {phase.kind === "checking" ? "Checking…" : "Check for updates"}
          </button>
        )}
      </div>
      {(phase.kind === "available" || phase.kind === "installing") && phase.info.notes && (
        <p class="muted">{phase.info.notes}</p>
      )}
      <div class="setting-row">
        <label>
          <input
            type="checkbox"
            checked={settings.check_for_updates}
            onChange={(e) =>
              save({ ...settings, check_for_updates: (e.target as HTMLInputElement).checked })
            }
          />{" "}
          Check for updates at startup (no meeting data is sent)
        </label>
      </div>
    </section>
  );
}
