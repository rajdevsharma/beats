import { useEffect, useRef, useState } from "react";
import { convertFileSrc } from "@tauri-apps/api/core";
import WaveSurfer from "wavesurfer.js";
import Timeline from "wavesurfer.js/dist/plugins/timeline.esm.js";
import RegionsPlugin, { type Region } from "wavesurfer.js/dist/plugins/regions.esm.js";

interface Props {
  mp3Path: string;
  beats: number[];
  onBeatsChange: (beats: number[]) => void;
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

export default function WaveformEditor({ mp3Path, beats, onBeatsChange }: Props) {
  // ── DOM / WaveSurfer refs ──────────────────────────────────────────────────
  const containerRef = useRef<HTMLDivElement>(null);
  const wsRef = useRef<WaveSurfer | null>(null);
  const regionsRef = useRef<RegionsPlugin | null>(null);

  // ── Zoom refs ──────────────────────────────────────────────────────────────
  const fitPxPerSecRef = useRef<number>(0);
  const zoomPxPerSecRef = useRef<number>(0);
  const zoomTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  // ── Beat / recording refs (stable for use inside event handlers) ───────────
  const beatsRef = useRef<number[]>(beats);
  const onBeatsChangeRef = useRef(onBeatsChange);
  const recordingRef = useRef(false);
  const recordingStartTimeRef = useRef(0);
  const pendingTapsRef = useRef<number[]>([]);
  const selectedBeatTimeRef = useRef<number | null>(null);

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

  // Keep stable refs in sync with latest props/state
  useEffect(() => { beatsRef.current = beats; }, [beats]);
  useEffect(() => { onBeatsChangeRef.current = onBeatsChange; }, [onBeatsChange]);
  useEffect(() => { selectedBeatTimeRef.current = selectedBeatTime; }, [selectedBeatTime]);

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

    // Shift+click on waveform → add beat at that position
    ws.on("interaction", () => {
      if (!shiftHeldRef.current) return;
      const t = ws.getCurrentTime();
      const updated = [...beatsRef.current, t].sort((a, b) => a - b);
      onBeatsChangeRef.current(updated);
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

    ws.load(convertFileSrc(mp3Path));
    wsRef.current = ws;

    return () => {
      ws.destroy();
      wsRef.current = null;
      regionsRef.current = null;
      containerRef.current?.removeEventListener("wheel", onWheel);
    };
  }, [mp3Path]);

  // ── Keyboard: Space, R, B, Delete, Shift tracking ─────────────────────────
  const shiftHeldRef = useRef(false);

  useEffect(() => {
    function onKeyDown(e: KeyboardEvent) {
      const tag = (e.target as HTMLElement).tagName;
      const isInput = tag === "INPUT" || tag === "TEXTAREA";

      if (e.key === "Shift") { shiftHeldRef.current = true; return; }

      if (isInput) return;

      if (e.code === "Space") {
        e.preventDefault();
        wsRef.current?.playPause();
        return;
      }

      if (e.code === "KeyR") {
        e.preventDefault();
        if (recordingRef.current) {
          stopRecording(wsRef.current!);
        } else {
          startRecording();
        }
        return;
      }

      if (e.code === "KeyB" && recordingRef.current) {
        e.preventDefault();
        const t = wsRef.current?.getCurrentTime();
        if (t !== undefined) pendingTapsRef.current.push(t);
        return;
      }

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
    // Replace any pre-existing beats in the swept range with new taps
    const kept = beatsRef.current.filter(t => t < startTime || t > endTime);
    const merged = [...kept, ...taps].sort((a, b) => a - b);
    onBeatsChangeRef.current(merged);
  }

  // ── Beat region sync ───────────────────────────────────────────────────────
  useEffect(() => {
    const rp = regionsRef.current;
    if (!rp || !ready) return;

    rp.clearRegions();

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

  // ── Zoom helpers ───────────────────────────────────────────────────────────
  function applyZoom(next: number) {
    zoomPxPerSecRef.current = next;
    setZoomPxPerSec(next);
    if (zoomTimerRef.current) clearTimeout(zoomTimerRef.current);
    zoomTimerRef.current = setTimeout(() => wsRef.current?.zoom(zoomPxPerSecRef.current), ZOOM_DEBOUNCE_MS);
  }

  function handleZoomIn() {
    applyZoom(Math.min(zoomPxPerSec * ZOOM_STEP, fitPxPerSecRef.current * ZOOM_MAX_MULTIPLIER));
  }

  function handleZoomOut() {
    applyZoom(Math.max(zoomPxPerSec / ZOOM_STEP, fitPxPerSecRef.current));
  }

  function handleZoomFit() {
    applyZoom(fitPxPerSecRef.current);
  }

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

  // Show BPM labels when adjacent beats are at least 40px apart
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
        className={`waveform-canvas${showLabels ? " show-beat-labels" : ""}`}
        style={{ visibility: ready ? "visible" : "hidden" }}
      />

      {ready && (
        <div className={`transport-bar${recording ? " transport-bar-recording" : ""}`}>
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
                className={`rate-btn ${playbackRate === r ? "rate-btn-active" : ""}`}
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
            title={recording ? "Stop recording (R) — Esc also works" : "Record beats (R), tap B on each beat"}
          >
            {recording ? "■ Stop" : "● Rec"}
          </button>
          {recording && (
            <span className="rec-indicator" title="Tap B on each beat">
              REC · tap B
            </span>
          )}

          <div className="transport-spacer" />

          {/* Beat count */}
          {beats.length > 0 && (
            <span className="beat-count">{beats.length} beats</span>
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
    </div>
  );
}
