import { useState } from "react";

export interface VideoExportOpts {
  orientation: "vertical" | "horizontal";
  start: number;
  end: number;
}

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

  const start = parseTime(startStr);
  const end = parseTime(endStr);
  const valid =
    start !== null && end !== null &&
    start >= 0 && end > start && start < totalDuration;
  const clipDur = valid ? Math.min(end!, totalDuration) - start! : null;

  function handleConfirm() {
    if (!valid) return;
    onConfirm({ orientation, start: start!, end: Math.min(end!, totalDuration) });
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

          {clipDur !== null && (
            <div className="modal-preview">
              <div className="modal-row">
                <span className="modal-label">Video length</span>
                <span className="modal-value mono">{formatTime(clipDur)}</span>
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
