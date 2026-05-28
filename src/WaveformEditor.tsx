import { useEffect, useRef, useState } from "react";
import { convertFileSrc } from "@tauri-apps/api/core";
import WaveSurfer from "wavesurfer.js";
import Timeline from "wavesurfer.js/dist/plugins/timeline.esm.js";

interface Props {
  mp3Path: string;
}

export default function WaveformEditor({ mp3Path }: Props) {
  const containerRef = useRef<HTMLDivElement>(null);
  const wsRef = useRef<WaveSurfer | null>(null);
  const [loadProgress, setLoadProgress] = useState(0);
  const [ready, setReady] = useState(false);

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
    ws.on("ready", () => setReady(true));

    ws.load(convertFileSrc(mp3Path));
    wsRef.current = ws;

    return () => {
      ws.destroy();
      wsRef.current = null;
    };
  }, [mp3Path]);

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
        className="waveform-container"
        style={{ visibility: ready ? "visible" : "hidden" }}
      />
    </div>
  );
}
