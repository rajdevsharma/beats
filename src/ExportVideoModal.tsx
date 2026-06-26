import { useState } from "react";

export interface VideoExportOpts {
  orientation: "vertical" | "horizontal";
  start: number;
  end: number;
  speed: number; // fraction: 1 = normal, 0.8 = 80% for slow practice
  beatPulse: boolean;
  orchestraBars: boolean;
  tempoPendulum: boolean;
}

const CUES: { key: "beatPulse" | "orchestraBars" | "tempoPendulum"; label: string; desc: string }[] = [
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
  const [cues, setCues] = useState<Record<string, boolean>>(() => {
    try {
      const raw = localStorage.getItem("beats_video_cues");
      if (raw) return JSON.parse(raw);
    } catch { /* ignore */ }
    return { beatPulse: true, orchestraBars: true, tempoPendulum: false };
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

  function handleConfirm() {
    if (!valid) return;
    localStorage.setItem("beats_video_cues", JSON.stringify(cues));
    onConfirm({
      speed: speedPct / 100,
      orientation, start: start!, end: Math.min(end!, totalDuration),
      beatPulse: !!cues.beatPulse,
      orchestraBars: !!cues.orchestraBars,
      tempoPendulum: !!cues.tempoPendulum,
    });
  }

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
