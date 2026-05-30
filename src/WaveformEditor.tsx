import { useEffect, useRef, useState } from "react";
import { convertFileSrc } from "@tauri-apps/api/core";
import WaveSurfer from "wavesurfer.js";
import Timeline from "wavesurfer.js/dist/plugins/timeline.esm.js";
import RegionsPlugin, { type Region } from "wavesurfer.js/dist/plugins/regions.esm.js";
import { invoke } from "@tauri-apps/api/core";
import { save as saveDialog } from "@tauri-apps/plugin-dialog";
import { Stretch } from "./types";
import StretchModal from "./StretchModal";
import { repositionBeats, originalToStretched } from "./timeMapping";

interface Props {
  mp3Path: string;
  beats: number[];
  onBeatsChange: (beats: number[]) => void;
  stretches: Stretch[];
  onStretchesChange: (stretches: Stretch[]) => void;
  bakedWavPath?: string;
  onBakedWavPathChange: (path: string) => void;
}

function formatTime(seconds: number): string {
  const m = Math.floor(seconds / 60);
  const s = Math.floor(seconds % 60);
  const cs = Math.floor((seconds % 1) * 10);
  return `${m}:${String(s).padStart(2, "0")}.${cs}`;
}

const ZOOM_STEP = 1.5;
const ZOOM_MAX_MULTIPLIER = 500;
const ZOOM_DEBOUNCE_MS = 80;
const PLAYBACK_RATES = [0.25, 0.5, 0.75, 1.0];
const BEAT_WIDTH_S = 0.002;
const BEAT_COLOR = "rgba(251, 191, 36, 0.80)";
const BEAT_COLOR_SELECTED = "rgba(255, 100, 60, 0.90)";
const ANCHOR_REGION_ID = "stretch-anchor";

export default function WaveformEditor({
  mp3Path, beats, onBeatsChange, stretches, onStretchesChange,
  bakedWavPath, onBakedWavPathChange,
}: Props) {
  // ── DOM / WaveSurfer refs ──────────────────────────────────────────────────
  const containerRef = useRef<HTMLDivElement>(null);
  const wsRef = useRef<WaveSurfer | null>(null);
  const regionsRef = useRef<RegionsPlugin | null>(null);

  // ── Zoom refs ──────────────────────────────────────────────────────────────
  const fitPxPerSecRef = useRef<number>(0);
  const zoomPxPerSecRef = useRef<number>(0);
  const zoomTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  // ── Stable refs for event handlers ────────────────────────────────────────
  const beatsRef = useRef<number[]>(beats);
  const stretchesRef = useRef<Stretch[]>(stretches);
  const onBeatsChangeRef = useRef(onBeatsChange);
  const onStretchesChangeRef = useRef(onStretchesChange);
  const recordingRef = useRef(false);
  const recordingStartTimeRef = useRef(0);
  const pendingTapsRef = useRef<number[]>([]);
  const selectedBeatTimeRef = useRef<number | null>(null);
  const shiftHeldRef = useRef(false);
  const stretchModeRef = useRef(false);
  const stretchAnchorRef = useRef<number | null>(null);
  const regionJustClickedRef = useRef(false);
  // Latest bakedWavPath for use inside the WaveSurfer creation effect
  const bakedWavPathRef = useRef(bakedWavPath);

  // ── State ──────────────────────────────────────────────────────────────────
  const [loadProgress, setLoadProgress] = useState(0);
  const [ready, setReady] = useState(false);
  const [playing, setPlaying] = useState(false);
  const [currentTime, setCurrentTime] = useState(0);
  const [duration, setDuration] = useState(0);
  const [zoomPxPerSec, setZoomPxPerSec] = useState(0);
  const [playbackRate, setPlaybackRate] = useState(1.0);
  const [recording, setRecording] = useState(false);
  const [selectedBeatTime, setSelectedBeatTime] = useState<number | null>(null);
  const [stretchMode, setStretchMode] = useState(false);
  const [stretchAnchor, setStretchAnchor] = useState<number | null>(null);
  const [stretchModal, setStretchModal] = useState<{ start: number; end: number } | null>(null);
  const [rerendering, setRerendering] = useState(false);
  const [exporting, setExporting] = useState(false);
  const [exportError, setExportError] = useState<string | null>(null);

  // Keep refs in sync
  useEffect(() => { beatsRef.current = beats; }, [beats]);
  useEffect(() => { stretchesRef.current = stretches; }, [stretches]);
  useEffect(() => { onBeatsChangeRef.current = onBeatsChange; }, [onBeatsChange]);
  useEffect(() => { onStretchesChangeRef.current = onStretchesChange; }, [onStretchesChange]);
  useEffect(() => { selectedBeatTimeRef.current = selectedBeatTime; }, [selectedBeatTime]);
  useEffect(() => { stretchModeRef.current = stretchMode; }, [stretchMode]);
  useEffect(() => { stretchAnchorRef.current = stretchAnchor; }, [stretchAnchor]);
  useEffect(() => { bakedWavPathRef.current = bakedWavPath; }, [bakedWavPath]);

  // ── WaveSurfer + Regions creation ──────────────────────────────────────────
  useEffect(() => {
    if (!containerRef.current) return;

    const regions = RegionsPlugin.create();
    regionsRef.current = regions;

    const ws = WaveSurfer.create({
      container: containerRef.current,
      waveColor: "#5a4fcf",
      progressColor: "#9b8eff",
      cursorColor: "#ffffff",
      cursorWidth: 2,
      height: 128,
      normalize: true,
      autoScroll: true,
      autoCenter: true,
      interact: true,
      plugins: [
        Timeline.create({ style: { color: "#a0a0b0", fontSize: "11px" } }),
        regions,
      ],
    });

    ws.on("loading", (pct) => setLoadProgress(pct));
    ws.on("ready", (dur) => {
      setReady(true);
      setDuration(dur);
      const fit = containerRef.current!.clientWidth / dur;
      fitPxPerSecRef.current = fit;
      zoomPxPerSecRef.current = fit;
      setZoomPxPerSec(fit);
    });
    ws.on("play", () => setPlaying(true));
    ws.on("pause", () => {
      setPlaying(false);
      if (recordingRef.current) stopRecording(ws);
    });
    ws.on("timeupdate", (t) => setCurrentTime(t));
    ws.on("finish", () => {
      setPlaying(false);
      if (recordingRef.current) stopRecording(ws);
    });

    ws.on("interaction", () => {
      // Ignore if the click landed on a region element
      if (regionJustClickedRef.current) return;

      const t = ws.getCurrentTime();

      if (stretchModeRef.current) {
        if (shiftHeldRef.current && stretchAnchorRef.current !== null) {
          // Second point → open modal
          const a = stretchAnchorRef.current;
          const start = Math.min(a, t);
          const end = Math.max(a, t);
          if (end - start > 0.01) {
            setStretchModal({ start, end });
          }
        } else if (!shiftHeldRef.current) {
          // First point → set anchor
          stretchAnchorRef.current = t;
          setStretchAnchor(t);
        }
        return;
      }

      // Normal mode: shift+click adds a beat
      if (shiftHeldRef.current) {
        const updated = [...beatsRef.current, t].sort((a, b) => a - b);
        onBeatsChangeRef.current(updated);
      }
    });

    // Ctrl+wheel zoom
    function onWheel(e: WheelEvent) {
      if (!e.ctrlKey) return;
      e.preventDefault();
      const fit = fitPxPerSecRef.current;
      if (fit === 0) return;
      const factor = e.deltaY < 0 ? ZOOM_STEP : 1 / ZOOM_STEP;
      const next = Math.max(fit, Math.min(zoomPxPerSecRef.current * factor, fit * ZOOM_MAX_MULTIPLIER));
      zoomPxPerSecRef.current = next;
      setZoomPxPerSec(next);
      if (zoomTimerRef.current) clearTimeout(zoomTimerRef.current);
      zoomTimerRef.current = setTimeout(() => ws.zoom(zoomPxPerSecRef.current), ZOOM_DEBOUNCE_MS);
    }
    containerRef.current.addEventListener("wheel", onWheel, { passive: false });

    const initialSrc = bakedWavPathRef.current
      ? convertFileSrc(bakedWavPathRef.current)
      : convertFileSrc(mp3Path);
    ws.load(initialSrc);
    wsRef.current = ws;

    return () => {
      ws.destroy();
      wsRef.current = null;
      regionsRef.current = null;
      containerRef.current?.removeEventListener("wheel", onWheel);
    };
  }, [mp3Path]);

  // ── Keyboard handlers ──────────────────────────────────────────────────────
  useEffect(() => {
    function onKeyDown(e: KeyboardEvent) {
      const tag = (e.target as HTMLElement).tagName;
      const isInput = tag === "INPUT" || tag === "TEXTAREA";

      if (e.key === "Shift") { shiftHeldRef.current = true; return; }
      if (isInput) return;

      // Cmd+R → re-render waveform with stretches baked in
      if (e.code === "KeyR" && (e.metaKey || e.ctrlKey)) {
        e.preventDefault();
        rerenderWaveform();
        return;
      }

      // Space → play/pause
      if (e.code === "Space") {
        e.preventDefault();
        wsRef.current?.playPause();
        return;
      }

      // Escape → cancel stretch anchor / exit stretch mode
      if (e.code === "Escape") {
        if (stretchAnchorRef.current !== null) {
          stretchAnchorRef.current = null;
          setStretchAnchor(null);
        } else if (stretchModeRef.current) {
          stretchModeRef.current = false;
          setStretchMode(false);
        }
        return;
      }

      // T → toggle stretch mode
      if (e.code === "KeyT") {
        e.preventDefault();
        const next = !stretchModeRef.current;
        stretchModeRef.current = next;
        setStretchMode(next);
        if (!next) {
          stretchAnchorRef.current = null;
          setStretchAnchor(null);
        }
        return;
      }

      // S → open stretch modal using anchor → current cursor
      if (e.code === "KeyS" && stretchModeRef.current && stretchAnchorRef.current !== null) {
        e.preventDefault();
        const t = wsRef.current?.getCurrentTime() ?? 0;
        const a = stretchAnchorRef.current;
        const start = Math.min(a, t);
        const end = Math.max(a, t);
        if (end - start > 0.01) setStretchModal({ start, end });
        return;
      }

      // R → toggle beat recording
      if (e.code === "KeyR") {
        e.preventDefault();
        if (recordingRef.current) stopRecording(wsRef.current!);
        else startRecording();
        return;
      }

      // B → tap beat while recording
      if (e.code === "KeyB" && recordingRef.current) {
        e.preventDefault();
        const t = wsRef.current?.getCurrentTime();
        if (t !== undefined) pendingTapsRef.current.push(t);
        return;
      }

      // Delete / Backspace → remove selected beat
      if ((e.code === "Delete" || e.code === "Backspace") && selectedBeatTimeRef.current !== null) {
        e.preventDefault();
        const sel = selectedBeatTimeRef.current;
        const updated = beatsRef.current.filter(t => Math.abs(t - sel) > 0.001);
        selectedBeatTimeRef.current = null;
        setSelectedBeatTime(null);
        onBeatsChangeRef.current(updated);
        return;
      }
    }

    function onKeyUp(e: KeyboardEvent) {
      if (e.key === "Shift") shiftHeldRef.current = false;
    }

    window.addEventListener("keydown", onKeyDown);
    window.addEventListener("keyup", onKeyUp);
    return () => {
      window.removeEventListener("keydown", onKeyDown);
      window.removeEventListener("keyup", onKeyUp);
    };
  }, []);

  // ── Recording helpers ──────────────────────────────────────────────────────
  function startRecording() {
    const ws = wsRef.current;
    if (!ws) return;
    recordingStartTimeRef.current = ws.getCurrentTime();
    pendingTapsRef.current = [];
    recordingRef.current = true;
    setRecording(true);
    if (!ws.isPlaying()) ws.play();
  }

  function stopRecording(ws: WaveSurfer) {
    recordingRef.current = false;
    setRecording(false);
    const startTime = recordingStartTimeRef.current;
    const endTime = ws.getCurrentTime();
    const taps = pendingTapsRef.current;
    pendingTapsRef.current = [];
    const kept = beatsRef.current.filter(t => t < startTime || t > endTime);
    const merged = [...kept, ...taps].sort((a, b) => a - b);
    onBeatsChangeRef.current(merged);
  }

  // ── Bake (Cmd+R) — Rubber Band offline, pitch-correct ─────────────────────
  async function rerenderWaveform() {
    const ws = wsRef.current;
    if (!ws || rerendering || stretches.length === 0) return;

    // First bake: prompt for save path. Re-bake: silently overwrite.
    let savePath = bakedWavPathRef.current;
    if (!savePath) {
      savePath = await saveDialog({
        title: "Save Baked Audio (WAV)",
        filters: [{ name: "WAV Audio", extensions: ["wav"] }],
        defaultPath: mp3Path.replace(/\.mp3$/i, "_baked.wav"),
      }) ?? undefined;
      if (!savePath) return;
    }

    setRerendering(true);
    ws.pause();
    try {
      // Rust: decode original MP3, apply Rubber Band stretches, write WAV
      await invoke("bake_audio", {
        mp3Path,
        stretches,
        outputPath: savePath,
      });

      onBakedWavPathChange(savePath);
      setReady(false);
      await ws.load(convertFileSrc(savePath));
      onBeatsChangeRef.current(repositionBeats(beats, stretches));
    } finally {
      setRerendering(false);
    }
  }

  // ── Export to MP3 — Rubber Band + ffmpeg ──────────────────────────────────
  async function handleExport() {
    if (exporting) return;
    setExportError(null);
    const outPath = await saveDialog({
      title: "Export as MP3",
      filters: [{ name: "MP3 Audio", extensions: ["mp3"] }],
      defaultPath: mp3Path.replace(/\.mp3$/i, "") + "_stretched.mp3",
    });
    if (!outPath) return;

    setExporting(true);
    try {
      await invoke("export_mp3", { mp3Path, stretches, outputPath: outPath });
    } catch (e) {
      setExportError(String(e));
    } finally {
      setExporting(false);
    }
  }

  // ── Stretch modal confirm ──────────────────────────────────────────────────
  function handleStretchConfirm(factor: number) {
    if (!stretchModal) return;
    const { start, end } = stretchModal;
    // Merge with existing, replacing any that overlap this range
    const kept = stretchesRef.current.filter(s => s.end <= start || s.start >= end);
    const updated: Stretch[] = [...kept, { start, end, factor }]
      .sort((a, b) => a.start - b.start);
    onStretchesChangeRef.current(updated);
    setStretchModal(null);
    stretchAnchorRef.current = null;
    setStretchAnchor(null);
  }

  // ── Beat region sync (only touches beat-* regions) ────────────────────────
  useEffect(() => {
    const rp = regionsRef.current;
    if (!rp || !ready) return;

    // Remove only beat regions, leaving anchor/stretch regions intact
    rp.getRegions().filter(r => r.id.startsWith("beat-")).forEach(r => r.remove());

    beats.forEach((t, i) => {
      const bpm = i < beats.length - 1 ? 60 / (beats[i + 1] - t) : null;
      const isSelected = selectedBeatTime !== null && Math.abs(t - selectedBeatTime) < 0.001;

      const labelEl = document.createElement("div");
      labelEl.className = "beat-label";
      if (bpm !== null) labelEl.textContent = bpm.toFixed(1);

      const region: Region = rp.addRegion({
        id: `beat-${t}`,
        start: t,
        end: t + BEAT_WIDTH_S,
        color: isSelected ? BEAT_COLOR_SELECTED : BEAT_COLOR,
        drag: true,
        resize: false,
        content: labelEl,
      });

      region.on("click", (e) => {
        e.stopPropagation();
        regionJustClickedRef.current = true;
        setTimeout(() => { regionJustClickedRef.current = false; }, 50);
        selectedBeatTimeRef.current = t;
        setSelectedBeatTime(t);
      });

      region.on("update-end", () => {
        const newTime = region.start;
        const updated = beatsRef.current
          .map(bt => Math.abs(bt - t) < 0.001 ? newTime : bt)
          .sort((a, b) => a - b);
        selectedBeatTimeRef.current = newTime;
        onBeatsChangeRef.current(updated);
      });
    });
  }, [beats, ready, selectedBeatTime]);

  // ── Stretch anchor region sync ─────────────────────────────────────────────
  useEffect(() => {
    const rp = regionsRef.current;
    if (!rp || !ready) return;

    rp.getRegions().filter(r => r.id === ANCHOR_REGION_ID).forEach(r => r.remove());

    if (stretchAnchor !== null && stretchMode) {
      rp.addRegion({
        id: ANCHOR_REGION_ID,
        start: stretchAnchor,
        end: stretchAnchor + 0.001,
        color: "rgba(59, 130, 246, 0.9)",
        drag: false,
        resize: false,
      });
    }
  }, [stretchAnchor, stretchMode, ready]);

  // ── Stretch overlay regions sync ───────────────────────────────────────────
  useEffect(() => {
    const rp = regionsRef.current;
    if (!rp || !ready) return;

    rp.getRegions().filter(r => r.id.startsWith("stretch-")).forEach(r => r.remove());

    const isBaked = !!bakedWavPath;

    stretches.forEach((s, i) => {
      const pct = Math.round((s.factor - 1) * 100);

      // When showing baked WAV, map overlay coordinates to baked time
      const displayStart = isBaked
        ? originalToStretched(s.start, stretches)
        : s.start;
      const displayEnd = isBaked
        ? displayStart + (s.end - s.start) * s.factor
        : s.end;

      const label = document.createElement("div");
      label.className = `stretch-label${isBaked ? " stretch-label-baked" : ""}`;
      label.textContent = isBaked
        ? `✓ ${pct >= 0 ? "+" : ""}${pct}%`
        : `${pct >= 0 ? "+" : ""}${pct}%`;

      rp.addRegion({
        id: `stretch-${i}`,
        start: displayStart,
        end: displayEnd,
        color: isBaked
          ? "rgba(148, 163, 184, 0.10)"   // muted slate — baked
          : s.factor >= 1
            ? "rgba(34, 197, 94, 0.12)"   // green — live slow
            : "rgba(239, 68, 68, 0.12)",  // red — live fast
        drag: false,
        resize: false,
        content: label,
      });
    });
  }, [stretches, ready, bakedWavPath]);

  // ── Zoom helpers ───────────────────────────────────────────────────────────
  function applyZoom(next: number) {
    zoomPxPerSecRef.current = next;
    setZoomPxPerSec(next);
    if (zoomTimerRef.current) clearTimeout(zoomTimerRef.current);
    zoomTimerRef.current = setTimeout(() => wsRef.current?.zoom(zoomPxPerSecRef.current), ZOOM_DEBOUNCE_MS);
  }

  const handleZoomIn = () => applyZoom(Math.min(zoomPxPerSec * ZOOM_STEP, fitPxPerSecRef.current * ZOOM_MAX_MULTIPLIER));
  const handleZoomOut = () => applyZoom(Math.max(zoomPxPerSec / ZOOM_STEP, fitPxPerSecRef.current));
  const handleZoomFit = () => applyZoom(fitPxPerSecRef.current);

  function handleRateChange(rate: number) {
    setPlaybackRate(rate);
    wsRef.current?.setPlaybackRate(rate);
  }

  // ── Derived display values ─────────────────────────────────────────────────
  const atFit = fitPxPerSecRef.current > 0 && Math.abs(zoomPxPerSec - fitPxPerSecRef.current) < 0.01;
  const zoomMultiplier = fitPxPerSecRef.current > 0 ? zoomPxPerSec / fitPxPerSecRef.current : 1;
  const zoomLabel = zoomMultiplier < 1.05 ? "Fit"
    : zoomMultiplier >= 10 ? `${Math.round(zoomMultiplier)}×`
    : `${zoomMultiplier.toFixed(1)}×`;

  const minBeatGap = beats.length > 1
    ? Math.min(...beats.slice(1).map((t, i) => t - beats[i]))
    : Infinity;
  const showLabels = ready && minBeatGap * zoomPxPerSec > 40;

  // ── Render ─────────────────────────────────────────────────────────────────
  return (
    <div className="waveform-editor">
      {!ready && (
        <div className="waveform-loading">
          <div className="waveform-loading-bar">
            <div className="waveform-loading-fill" style={{ width: `${loadProgress}%` }} />
          </div>
          <span className="waveform-loading-label">
            {loadProgress < 100 ? `Loading ${loadProgress}%` : "Rendering…"}
          </span>
        </div>
      )}

      <div
        ref={containerRef}
        className={[
          "waveform-canvas",
          showLabels ? "show-beat-labels" : "",
          stretchMode ? "stretch-mode-active" : "",
        ].filter(Boolean).join(" ")}
        style={{ visibility: ready ? "visible" : "hidden" }}
      />

      {ready && (
        <div className={[
          "transport-bar",
          recording ? "transport-bar-recording" : "",
          stretchMode ? "transport-bar-stretch" : "",
        ].filter(Boolean).join(" ")}>

          {/* Playback */}
          <button className="transport-btn" onClick={() => wsRef.current?.seekTo(0)} title="Skip to start">⏮</button>
          <button
            className="transport-btn transport-play"
            onClick={() => wsRef.current?.playPause()}
            title={playing ? "Pause (Space)" : "Play (Space)"}
          >
            {playing ? "⏸" : "▶"}
          </button>
          <span className="transport-time">
            {formatTime(currentTime)}
            <span className="transport-duration"> / {formatTime(duration)}</span>
          </span>

          {/* Rate */}
          <div className="rate-selector">
            {PLAYBACK_RATES.map((r) => (
              <button
                key={r}
                className={`rate-btn${playbackRate === r ? " rate-btn-active" : ""}`}
                onClick={() => handleRateChange(r)}
                title={r < 1 ? "Pitch shifts at non-1× speeds" : undefined}
              >
                {r === 1 ? "1×" : `${r * 100}%`}
              </button>
            ))}
          </div>

          {/* Record */}
          <button
            className={`transport-btn record-btn${recording ? " record-btn-active" : ""}`}
            onClick={() => recording ? stopRecording(wsRef.current!) : startRecording()}
            title={recording ? "Stop recording (R)" : "Record beats (R), tap B on each beat"}
          >
            {recording ? "■ Stop" : "● Rec"}
          </button>
          {recording && <span className="rec-indicator">REC · tap B</span>}

          {/* Stretch mode */}
          <button
            className={`transport-btn stretch-btn${stretchMode ? " stretch-btn-active" : ""}`}
            onClick={() => {
              const next = !stretchMode;
              setStretchMode(next);
              stretchModeRef.current = next;
              if (!next) { setStretchAnchor(null); stretchAnchorRef.current = null; }
            }}
            title={stretchMode ? "Exit stretch mode (T or Esc)" : "Stretch mode (T): click anchor, Shift+click end"}
          >
            ⇔ Stretch
          </button>
          {stretchMode && (
            <span className="stretch-indicator">
              {stretchAnchor !== null
                ? `${formatTime(stretchAnchor)} → Shift+click end (or S)`
                : "Click to set anchor"}
            </span>
          )}

          <div className="transport-spacer" />

          {beats.length > 0 && <span className="beat-count">{beats.length} beats</span>}
          {stretches.length > 0 && (
            <>
              <span className="beat-count">{stretches.length} stretch{stretches.length !== 1 ? "es" : ""}</span>
              <button
                className="transport-btn rerender-btn"
                onClick={rerenderWaveform}
                disabled={rerendering}
                title={bakedWavPath
                  ? "Re-bake: reprocess original MP3 with current stretches (Cmd+R)"
                  : "Bake: save stretched audio to WAV, load into waveform (Cmd+R)"}
              >
                {rerendering ? "Rendering…" : bakedWavPath ? "⌘R Re-bake" : "⌘R Bake"}
              </button>
            </>
          )}

          {/* Export */}
          <button
            className="transport-btn export-btn"
            onClick={handleExport}
            disabled={exporting}
            title="Export to MP3 with all stretches applied (requires ffmpeg)"
          >
            {exporting ? "Exporting…" : "↓ Export MP3"}
          </button>
          {exportError && (
            <span className="export-error" title={exportError}>⚠ export failed</span>
          )}

          {/* Zoom */}
          <span className="zoom-hint">Ctrl+scroll</span>
          <button className="transport-btn" onClick={handleZoomOut} title="Zoom out">−</button>
          <span className="zoom-label">{zoomLabel}</span>
          <button className="transport-btn" onClick={handleZoomIn} title="Zoom in">+</button>
          <button
            className={`transport-btn${atFit ? " transport-btn-active" : ""}`}
            onClick={handleZoomFit}
            title="Fit to window"
          >⊡</button>
        </div>
      )}

      {stretchModal && (
        <StretchModal
          start={stretchModal.start}
          end={stretchModal.end}
          beats={beats}
          onConfirm={handleStretchConfirm}
          onCancel={() => setStretchModal(null)}
        />
      )}
    </div>
  );
}
