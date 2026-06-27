import { useState } from "react";
import { open as openDialog } from "@tauri-apps/plugin-dialog";

type CueKey =
  | "beatPulse" | "orchestraBars" | "tempoPendulum"
  | "progressBar" | "nextNoteCue" | "countdownPips";

export interface VideoExportOpts extends Record<CueKey, boolean> {
  orientation: "vertical" | "horizontal";
  start: number;
  end: number;
  speed: number; // fraction: 1 = normal, 0.8 = 80% for slow practice
  bgVideoPath: string | null; // full-screen background video (any local file)
  bgBrightness: number;       // 0..1 dimmer for the background
}

const CUE_DEFAULTS: Record<CueKey, boolean> = {
  beatPulse: true, orchestraBars: true, tempoPendulum: false,
  progressBar: true, nextNoteCue: true, countdownPips: true,
};

const CUES: { key: CueKey; label: string; desc: string }[] = [
  { key: "progressBar", label: "Progress bar", desc: "Bar across the top tracking your position through the piece — where am I overall" },
  { key: "nextNoteCue", label: "Next-note cue", desc: "Rings the immediate upcoming piano note(s) with a guide line so you always see what you play next" },
  { key: "countdownPips", label: "Re-entry countdown", desc: "During a piano rest, pips count the beats until your next entrance so you come back in on time" },
  { key: "beatPulse", label: "Beat pulse", desc: "Full-screen edge glow that swells into every beat — easy to catch in peripheral vision" },
  { key: "orchestraBars", label: "Orchestra energy bars", desc: "Big edge bars showing how loud the orchestra is, colored by section, flashing on entrances" },
  { key: "tempoPendulum", label: "Tempo pendulum", desc: "Large metronome bob sweeping side to side, hitting an extreme on each beat" },
];

interface Props {
  totalDuration: number; // full length of the stretched output, seconds
  onConfirm: (opts: VideoExportOpts) => void;
  onCancel: () => void;
}

function formatTime(s: number): string {
  const m = Math.floor(s / 60);
  const sec = Math.floor(s % 60);
  return `${m}:${String(sec).padStart(2, "0")}`;
}

/** Accepts "m:ss", "h:mm:ss", or plain seconds. Returns null if unparseable. */
function parseTime(input: string): number | null {
  const parts = input.trim().split(":");
  if (parts.some(p => p === "" || !/^\d+(\.\d+)?$/.test(p))) return null;
  if (parts.length === 1) return parseFloat(parts[0]);
  if (parts.length === 2) return parseInt(parts[0]) * 60 + parseFloat(parts[1]);
  if (parts.length === 3)
    return parseInt(parts[0]) * 3600 + parseInt(parts[1]) * 60 + parseFloat(parts[2]);
  return null;
}

export default function ExportVideoModal({ totalDuration, onConfirm, onCancel }: Props) {
  const [orientation, setOrientation] = useState<"vertical" | "horizontal">("vertical");
  const [startStr, setStartStr] = useState("0:00");
  const [endStr, setEndStr] = useState(formatTime(totalDuration));
  const [speedStr, setSpeedStr] = useState("100");
  const [bgVideoPath, setBgVideoPath] = useState<string | null>(() =>
    localStorage.getItem("beats_video_bg_path") || null
  );
  const [bgBrightnessPct, setBgBrightnessPct] = useState(() => {
    const v = parseInt(localStorage.getItem("beats_video_bg_brightness") ?? "60");
    return isFinite(v) ? v : 60;
  });
  const [cues, setCues] = useState<Record<string, boolean>>(() => {
    try {
      const raw = localStorage.getItem("beats_video_cues");
      // Merge over defaults so newly-added cues appear for returning users.
      if (raw) return { ...CUE_DEFAULTS, ...JSON.parse(raw) };
    } catch { /* ignore */ }
    return { ...CUE_DEFAULTS };
  });

  const start = parseTime(startStr);
  const end = parseTime(endStr);
  const speedPct = parseFloat(speedStr);
  const speedValid = isFinite(speedPct) && speedPct >= 25 && speedPct <= 400;
  const valid =
    start !== null && end !== null &&
    start >= 0 && end > start && start < totalDuration && speedValid;
  const clipDur = valid ? Math.min(end!, totalDuration) - start! : null;
  const videoLen = clipDur !== null ? clipDur / (speedPct / 100) : null;

  async function pickBgVideo() {
    const sel = await openDialog({
      title: "Choose a background video",
      multiple: false,
      filters: [{ name: "Video", extensions: ["mp4", "mov", "mkv", "webm", "m4v", "avi"] }],
    });
    if (typeof sel === "string") {
      setBgVideoPath(sel);
      localStorage.setItem("beats_video_bg_path", sel);
    }
  }

  function clearBgVideo() {
    setBgVideoPath(null);
    localStorage.removeItem("beats_video_bg_path");
  }

  function handleConfirm() {
    if (!valid) return;
    localStorage.setItem("beats_video_cues", JSON.stringify(cues));
    localStorage.setItem("beats_video_bg_brightness", String(bgBrightnessPct));
    onConfirm({
      speed: speedPct / 100,
      orientation, start: start!, end: Math.min(end!, totalDuration),
      bgVideoPath,
      bgBrightness: bgBrightnessPct / 100,
      beatPulse: !!cues.beatPulse,
      orchestraBars: !!cues.orchestraBars,
      tempoPendulum: !!cues.tempoPendulum,
      progressBar: !!cues.progressBar,
      nextNoteCue: !!cues.nextNoteCue,
      countdownPips: !!cues.countdownPips,
    });
  }

  const bgName = bgVideoPath ? bgVideoPath.split("/").pop() : null;

  return (
    <div className="modal-backdrop" onClick={(e) => e.target === e.currentTarget && onCancel()}>
      <div className="modal">
        <div className="modal-header">
          <span className="modal-title">Export Video</span>
          <button className="modal-close" onClick={onCancel}>✕</button>
        </div>

        <div className="modal-body">
          <div className="modal-row">
            <span className="modal-label">Scroll style</span>
            <div className="video-seg">
              <button
                className={`video-seg-btn${orientation === "vertical" ? " video-seg-btn-active" : ""}`}
                onClick={() => setOrientation("vertical")}
                title="Notes fall from the top onto a keyboard at the bottom (classic Synthesia)"
              >
                ↓ Falling
              </button>
              <button
                className={`video-seg-btn${orientation === "horizontal" ? " video-seg-btn-active" : ""}`}
                onClick={() => setOrientation("horizontal")}
                title="Notes scroll right-to-left past a keyboard on the left (more read-ahead)"
              >
                ← Scrolling
              </button>
            </div>
          </div>

          <div className="modal-divider" />

          <div className="modal-row modal-row-input">
            <label className="modal-label" htmlFor="video-start">Start time</label>
            <div className="modal-input-group">
              <input
                id="video-start"
                className="modal-input"
                type="text"
                value={startStr}
                onChange={(e) => setStartStr(e.target.value)}
                placeholder="0:00"
              />
            </div>
          </div>
          <div className="modal-row modal-row-input">
            <label className="modal-label" htmlFor="video-end">End time</label>
            <div className="modal-input-group">
              <input
                id="video-end"
                className="modal-input"
                type="text"
                value={endStr}
                onChange={(e) => setEndStr(e.target.value)}
                placeholder={formatTime(totalDuration)}
              />
            </div>
          </div>
          <p className="modal-hint">
            Times are in the exported (stretched) timeline · full piece = {formatTime(totalDuration)}
          </p>

          <div className="modal-row modal-row-input">
            <label className="modal-label" htmlFor="video-speed">Speed</label>
            <div className="modal-input-group">
              <input
                id="video-speed"
                className="modal-input"
                type="number"
                min="25"
                max="400"
                step="5"
                value={speedStr}
                onChange={(e) => setSpeedStr(e.target.value)}
              />
              <span className="modal-input-unit">%</span>
            </div>
          </div>
          <p className="modal-hint">
            &lt;100% = slower for practice (pitch preserved) &nbsp;·&nbsp; &gt;100% = faster
          </p>

          <div className="modal-divider" />

          <div className="modal-row">
            <span className="modal-label">Background video</span>
            <div className="bg-video-pick">
              {bgName ? (
                <>
                  <span className="bg-video-name" title={bgVideoPath!}>{bgName}</span>
                  <button className="bg-video-btn" onClick={pickBgVideo}>Change</button>
                  <button className="bg-video-btn" onClick={clearBgVideo}>✕</button>
                </>
              ) : (
                <button className="bg-video-btn" onClick={pickBgVideo}>Choose video…</button>
              )}
            </div>
          </div>
          {bgVideoPath ? (
            <>
              <div className="modal-row modal-row-input">
                <label className="modal-label" htmlFor="bg-bright">Dimmer</label>
                <div className="modal-input-group" style={{ gap: 8 }}>
                  <input
                    id="bg-bright"
                    type="range"
                    min={10}
                    max={100}
                    step={5}
                    value={bgBrightnessPct}
                    onChange={(e) => setBgBrightnessPct(parseInt(e.target.value))}
                    style={{ width: 120, accentColor: "var(--accent)" }}
                  />
                  <span className="modal-value mono" style={{ minWidth: 38 }}>{bgBrightnessPct}%</span>
                </div>
              </div>
              <p className="modal-hint">
                Plays full-screen behind the visualization (first {formatTime(videoLen ?? 0)} of the clip). Lower = competes less.
              </p>
            </>
          ) : (
            <p className="modal-hint">
              Optional — overlay the visualization on any video as a memorable landmark backdrop.
            </p>
          )}

          <div className="modal-divider" />

          <div className="modal-row">
            <span className="modal-label">Performance cues</span>
          </div>
          <div className="video-cues">
            {CUES.map(c => (
              <label key={c.key} className="video-cue" title={c.desc}>
                <input
                  type="checkbox"
                  checked={!!cues[c.key]}
                  onChange={e => setCues(prev => ({ ...prev, [c.key]: e.target.checked }))}
                />
                <span className="video-cue-label">{c.label}</span>
                <span className="video-cue-desc">{c.desc}</span>
              </label>
            ))}
          </div>

          {videoLen !== null && (
            <div className="modal-preview">
              <div className="modal-row">
                <span className="modal-label">Video length</span>
                <span className="modal-value mono">
                  {formatTime(videoLen)}{speedPct !== 100 ? ` · ${speedPct}% speed` : ""}
                </span>
              </div>
              <div className="modal-row">
                <span className="modal-label">Format</span>
                <span className="modal-value mono">1920×1080 · 30 fps · H.264 + AAC</span>
              </div>
            </div>
          )}
        </div>

        <div className="modal-footer">
          <button className="modal-btn modal-btn-cancel" onClick={onCancel}>Cancel</button>
          <button className="modal-btn modal-btn-confirm" onClick={handleConfirm} disabled={!valid}>
            Export Video
          </button>
        </div>
      </div>
    </div>
  );
}
