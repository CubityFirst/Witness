import { useEffect, useState } from "preact/hooks";
import type { RefObject } from "preact";
import {
  DownloadSimple,
  FastForward,
  Pause,
  Play,
  Rewind,
  SpeakerHigh,
} from "../lib/icons";

const SPEEDS = [0.75, 1, 1.25, 1.5, 1.75, 2];

function fmtClock(seconds: number): string {
  if (!isFinite(seconds) || seconds < 0) seconds = 0;
  const s = Math.floor(seconds);
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = String(s % 60).padStart(2, "0");
  return h > 0 ? `${h}:${String(m).padStart(2, "0")}:${sec}` : `${m}:${sec}`;
}

/**
 * Custom audio player wrapping a hidden <audio> element. The element ref is
 * shared with the transcript view so click-to-seek and follow-along keep
 * working against the same engine.
 */
export function AudioPlayer(props: {
  src: string;
  audioRef: RefObject<HTMLAudioElement>;
  onTimeUpdate: () => void;
  onDownload: () => void;
}) {
  const [playing, setPlaying] = useState(false);
  const [duration, setDuration] = useState(0);
  const [current, setCurrent] = useState(0);
  const [volume, setVolume] = useState(() => {
    const v = parseFloat(localStorage.getItem("witness-volume") ?? "1");
    return isFinite(v) ? Math.min(1, Math.max(0, v)) : 1;
  });
  const [speed, setSpeed] = useState(() => {
    const v = parseFloat(localStorage.getItem("witness-speed") ?? "1");
    return SPEEDS.includes(v) ? v : 1;
  });

  // Keep the element in sync with persisted volume/speed (also after the
  // src changes, which resets playbackRate in some engines).
  useEffect(() => {
    const a = props.audioRef.current;
    if (a) {
      a.volume = volume;
      a.playbackRate = speed;
    }
  }, [volume, speed, props.src]);

  const toggle = () => {
    const a = props.audioRef.current;
    if (!a) return;
    if (a.paused) a.play().catch(() => {});
    else a.pause();
  };

  const seek = (t: number) => {
    const a = props.audioRef.current;
    if (a) a.currentTime = t;
  };

  const skip = (delta: number) => {
    const a = props.audioRef.current;
    if (a) a.currentTime = Math.max(0, a.currentTime + delta);
  };

  // Keyboard: space = play/pause, ←/→ = ±5 s (ignored while typing).
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const t = e.target as HTMLElement | null;
      if (
        t &&
        (t.tagName === "INPUT" ||
          t.tagName === "TEXTAREA" ||
          t.tagName === "SELECT" ||
          t.isContentEditable)
      ) {
        return;
      }
      const a = props.audioRef.current;
      if (!a) return;
      if (e.key === " ") {
        e.preventDefault();
        if (a.paused) a.play().catch(() => {});
        else a.pause();
      } else if (e.key === "ArrowLeft") {
        e.preventDefault();
        a.currentTime = Math.max(0, a.currentTime - 5);
      } else if (e.key === "ArrowRight") {
        e.preventDefault();
        a.currentTime += 5;
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  return (
    <div class="player">
      <audio
        ref={props.audioRef}
        src={props.src}
        preload="metadata"
        onPlay={() => setPlaying(true)}
        onPause={() => setPlaying(false)}
        onEnded={() => setPlaying(false)}
        onLoadedMetadata={(e) => {
          const a = e.target as HTMLAudioElement;
          setDuration(a.duration);
          a.volume = volume;
          a.playbackRate = speed;
        }}
        onTimeUpdate={(e) => {
          setCurrent((e.target as HTMLAudioElement).currentTime);
          props.onTimeUpdate();
        }}
      />
      <div class="player-row">
        <span class="player-time">{fmtClock(current)}</span>
        <input
          class="player-seek"
          type="range"
          min={0}
          max={duration || 0}
          step={0.1}
          value={current}
          onInput={(e) => seek(parseFloat((e.target as HTMLInputElement).value))}
        />
        <span class="player-time player-time-total">{fmtClock(duration)}</span>
      </div>
      <div class="player-row player-controls">
        <div class="player-cluster">
          <button class="player-btn" title="Back 10 s (←: 5 s)" onClick={() => skip(-10)}>
            <Rewind size={15} />
          </button>
          <button
            class="player-btn player-play"
            title={playing ? "Pause (space)" : "Play (space)"}
            onClick={toggle}
          >
            {playing ? <Pause /> : <Play />}
          </button>
          <button class="player-btn" title="Forward 10 s (→: 5 s)" onClick={() => skip(10)}>
            <FastForward size={15} />
          </button>
        </div>
        <div class="player-cluster player-cluster-right">
          <select
            class="player-speed"
            title="Playback speed"
            value={String(speed)}
            onChange={(e) => {
              const v = parseFloat((e.target as HTMLSelectElement).value);
              setSpeed(v);
              localStorage.setItem("witness-speed", String(v));
            }}
          >
            {SPEEDS.map((s) => (
              <option value={String(s)} key={s}>
                {s}×
              </option>
            ))}
          </select>
          <span class="player-volume" title="Volume">
            <SpeakerHigh />
            <input
              type="range"
              min={0}
              max={1}
              step={0.05}
              value={volume}
              onInput={(e) => {
                const v = parseFloat((e.target as HTMLInputElement).value);
                setVolume(v);
                localStorage.setItem("witness-volume", String(v));
              }}
            />
          </span>
          <button class="player-btn" title="Save a copy…" onClick={props.onDownload}>
            <DownloadSimple />
          </button>
        </div>
      </div>
    </div>
  );
}
