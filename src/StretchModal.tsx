import { useState, useEffect, useRef } from "react";

interface Props {
  start: number;
  end: number;
  beats: number[];
  onConfirm: (factor: number) => void;
  onCancel: () => void;
  initialFactor?: number; // pre-fill when editing an existing stretch
  onRemove?: () => void;  // present when editing an existing stretch
}

function formatTime(s: number): string {
  const m = Math.floor(s / 60);
  const sec = Math.floor(s % 60);
  const ms = Math.round((s % 1) * 1000);
  return `${m}:${String(sec).padStart(2, "0")}.${String(ms).padStart(3, "0")}`;
}

function computeBpm(beats: number[], start: number, end: number): number | null {
  const inRange = beats.filter(t => t >= start && t <= end);
  if (inRange.length >= 2) {
    return 60 * (inRange.length - 1) / (inRange[inRange.length - 1] - inRange[0]);
  }
  // Fall back to surrounding beats if only 0–1 in range
  const beforeArr = beats.filter(t => t < start);
  const before = beforeArr.length > 0 ? beforeArr[beforeArr.length - 1] : undefined;
  const after = beats.find(t => t > end);
  if (before !== undefined && after !== undefined) {
    return 60 / (after - before);
  }
  return null;
}

export default function StretchModal({ start, end, beats, onConfirm, onCancel, initialFactor, onRemove }: Props) {
  const [factorPct, setFactorPct] = useState(
    initialFactor != null ? String(Math.round(initialFactor * 100)) : "100"
  );
  const inputRef = useRef<HTMLInputElement>(null);
  const isEditing = onRemove != null;

  useEffect(() => {
    inputRef.current?.select();
  }, []);

  // Close on Escape
  useEffect(() => {
    function onKey(e: KeyboardEvent) {
      if (e.key === "Escape") onCancel();
      if (e.key === "Enter") handleConfirm();
    }
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [factorPct]);

  function handleConfirm() {
    const pct = parseFloat(factorPct);
    if (!isFinite(pct) || pct <= 0) return;
    onConfirm(pct / 100);
  }

  const duration = end - start;
  const currentBpm = computeBpm(beats, start, end);
  const factor = parseFloat(factorPct) / 100;
  const newDuration = isFinite(factor) && factor > 0 ? duration * factor : null;
  const newBpm = currentBpm && isFinite(factor) && factor > 0 ? currentBpm / factor : null;
  const isValid = isFinite(factor) && factor > 0;

  return (
    <div className="modal-backdrop" onClick={(e) => e.target === e.currentTarget && onCancel()}>
      <div className="modal">
        <div className="modal-header">
          <span className="modal-title">{isEditing ? "Edit Stretch" : "Time Stretch"}</span>
          <button className="modal-close" onClick={onCancel}>✕</button>
        </div>

        <div className="modal-body">
          <div className="modal-row">
            <span className="modal-label">Selection</span>
            <span className="modal-value mono">
              {formatTime(start)} → {formatTime(end)}
            </span>
          </div>
          <div className="modal-row">
            <span className="modal-label">Duration</span>
            <span className="modal-value mono">{duration.toFixed(3)}s</span>
          </div>
          <div className="modal-row">
            <span className="modal-label">Current BPM</span>
            <span className="modal-value mono">
              {currentBpm !== null ? currentBpm.toFixed(1) : <span className="modal-na">N/A — no beats in range</span>}
            </span>
          </div>

          <div className="modal-divider" />

          <div className="modal-row modal-row-input">
            <label className="modal-label" htmlFor="stretch-factor">
              Stretch factor
            </label>
            <div className="modal-input-group">
              <input
                id="stretch-factor"
                ref={inputRef}
                className="modal-input"
                type="number"
                min="10"
                max="1000"
                step="1"
                value={factorPct}
                onChange={(e) => setFactorPct(e.target.value)}
              />
              <span className="modal-input-unit">%</span>
            </div>
          </div>
          <p className="modal-hint">
            &gt;100% = slower &nbsp;·&nbsp; &lt;100% = faster
          </p>

          {isValid && (
            <div className="modal-preview">
              <div className="modal-row">
                <span className="modal-label">New duration</span>
                <span className="modal-value mono">{newDuration!.toFixed(3)}s</span>
              </div>
              <div className="modal-row">
                <span className="modal-label">New BPM</span>
                <span className="modal-value mono">
                  {newBpm !== null ? newBpm.toFixed(1) : <span className="modal-na">N/A</span>}
                </span>
              </div>
            </div>
          )}
        </div>

        <div className="modal-footer">
          {isEditing && (
            <button className="modal-btn modal-btn-remove" onClick={onRemove}>
              Remove
            </button>
          )}
          <button className="modal-btn modal-btn-cancel" onClick={onCancel}>Cancel</button>
          <button
            className="modal-btn modal-btn-confirm"
            onClick={handleConfirm}
            disabled={!isValid}
          >
            {isEditing ? "Apply Changes" : "Apply Stretch"}
          </button>
        </div>
      </div>
    </div>
  );
}
