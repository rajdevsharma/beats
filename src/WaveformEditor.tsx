import { useEffect, useRef, useState } from "react";
import { convertFileSrc } from "@tauri-apps/api/core";
import WaveSurfer from "wavesurfer.js";
import Timeline from "wavesurfer.js/dist/plugins/timeline.esm.js";

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

export default function WaveformEditor({ mp3Path, beats: _beats, onBeatsChange: _onBeatsChange }: Props) {
  const containerRef = useRef<HTMLDivElement>(null);
  const wsRef = useRef<WaveSurfer | null>(null);
  const fitPxPerSecRef = useRef<number>(0);
  const zoomPxPerSecRef = useRef<number>(0);
  const zoomTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  const [loadProgress, setLoadProgress] = useState(0);
  const [ready, setReady] = useState(false);
  const [playing, setPlaying] = useState(false);
  const [currentTime, setCurrentTime] = useState(0);
  const [duration, setDuration] = useState(0);
  const [zoomPxPerSec, setZoomPxPerSec] = useState(0);

  useEffect(() => {
    if (!containerRef.current) return;

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
        Timeline.create({
          style: { color: "#a0a0b0", fontSize: "11px" },
        }),
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
    ws.on("pause", () => setPlaying(false));
    ws.on("timeupdate", (t) => setCurrentTime(t));
    ws.on("finish", () => setPlaying(false));

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
      containerRef.current?.removeEventListener("wheel", onWheel);
    };
  }, [mp3Path]);

  // Global space bar → play/pause, skipping text inputs
  useEffect(() => {
    function onKeyDown(e: KeyboardEvent) {
      if (e.code !== "Space") return;
      const tag = (e.target as HTMLElement).tagName;
      if (tag === "INPUT" || tag === "TEXTAREA") return;
      e.preventDefault();
      wsRef.current?.playPause();
    }
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, []);

  function applyZoom(next: number) {
    zoomPxPerSecRef.current = next;
    setZoomPxPerSec(next);
    if (zoomTimerRef.current) clearTimeout(zoomTimerRef.current);
    zoomTimerRef.current = setTimeout(() => wsRef.current?.zoom(zoomPxPerSecRef.current), ZOOM_DEBOUNCE_MS);
  }

  function handleZoomIn() {
    const fit = fitPxPerSecRef.current;
    applyZoom(Math.min(zoomPxPerSec * ZOOM_STEP, fit * ZOOM_MAX_MULTIPLIER));
  }

  function handleZoomOut() {
    applyZoom(Math.max(zoomPxPerSec / ZOOM_STEP, fitPxPerSecRef.current));
  }

  function handleZoomFit() {
    applyZoom(fitPxPerSecRef.current);
  }

  const atFit = fitPxPerSecRef.current > 0 &&
    Math.abs(zoomPxPerSec - fitPxPerSecRef.current) < 0.01;
  const zoomMultiplier = fitPxPerSecRef.current > 0
    ? zoomPxPerSec / fitPxPerSecRef.current
    : 1;
  const zoomLabel = zoomMultiplier < 1.05
    ? "Fit"
    : zoomMultiplier >= 10
    ? `${Math.round(zoomMultiplier)}×`
    : `${zoomMultiplier.toFixed(1)}×`;

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
        className="waveform-canvas"
        style={{ visibility: ready ? "visible" : "hidden" }}
      />

      {ready && (
        <div className="transport-bar">
          {/* Playback controls */}
          <button className="transport-btn" onClick={() => wsRef.current?.seekTo(0)} title="Skip to start">
            ⏮
          </button>
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

          <div className="transport-spacer" />

          {/* Zoom controls */}
          <span className="zoom-hint">Ctrl+scroll to zoom</span>
          <button className="transport-btn" onClick={handleZoomOut} title="Zoom out">−</button>
          <span className="zoom-label">{zoomLabel}</span>
          <button className="transport-btn" onClick={handleZoomIn} title="Zoom in">+</button>
          <button
            className={`transport-btn ${atFit ? "transport-btn-active" : ""}`}
            onClick={handleZoomFit}
            title="Fit to window"
          >
            ⊡
          </button>
        </div>
      )}
    </div>
  );
}
