import { useEffect, useRef, useState } from "react";
import { convertFileSrc } from "@tauri-apps/api/core";
import WaveSurfer from "wavesurfer.js";
import Timeline from "wavesurfer.js/dist/plugins/timeline.esm.js";

interface Props {
  mp3Path: string;
}

function formatTime(seconds: number): string {
  const m = Math.floor(seconds / 60);
  const s = Math.floor(seconds % 60);
  const cs = Math.floor((seconds % 1) * 10);
  return `${m}:${String(s).padStart(2, "0")}.${cs}`;
}

export default function WaveformEditor({ mp3Path }: Props) {
  const containerRef = useRef<HTMLDivElement>(null);
  const wsRef = useRef<WaveSurfer | null>(null);
  const [loadProgress, setLoadProgress] = useState(0);
  const [ready, setReady] = useState(false);
  const [playing, setPlaying] = useState(false);
  const [currentTime, setCurrentTime] = useState(0);
  const [duration, setDuration] = useState(0);

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
    });
    ws.on("play", () => setPlaying(true));
    ws.on("pause", () => setPlaying(false));
    ws.on("timeupdate", (t) => setCurrentTime(t));
    ws.on("finish", () => setPlaying(false));

    ws.load(convertFileSrc(mp3Path));
    wsRef.current = ws;

    return () => {
      ws.destroy();
      wsRef.current = null;
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

  function handleSkipToStart() {
    wsRef.current?.seekTo(0);
  }

  function handlePlayPause() {
    wsRef.current?.playPause();
  }

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
          <button
            className="transport-btn"
            onClick={handleSkipToStart}
            title="Skip to start"
          >
            ⏮
          </button>
          <button
            className="transport-btn transport-play"
            onClick={handlePlayPause}
            title={playing ? "Pause (Space)" : "Play (Space)"}
          >
            {playing ? "⏸" : "▶"}
          </button>
          <span className="transport-time">
            {formatTime(currentTime)}
            <span className="transport-duration"> / {formatTime(duration)}</span>
          </span>
        </div>
      )}
    </div>
  );
}
