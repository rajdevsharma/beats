import { useEffect, useRef, useState } from "react";
import WaveSurfer from "wavesurfer.js";
import Timeline from "wavesurfer.js/dist/plugins/timeline.esm.js";
import RegionsPlugin from "wavesurfer.js/dist/plugins/regions.esm.js";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { save as saveDialog } from "@tauri-apps/plugin-dialog";
import { Stretch } from "./types";
import StretchModal from "./StretchModal";
import ExportVideoModal, { VideoExportOpts } from "./ExportVideoModal";
import SpectrogramControls from "./SpectrogramControls";
import {
  originalToStretched, stretchedToOriginal, stretchedDuration,
  buildSegments, TimeSegment,
} from "./timeMapping";

// ── Salamander Grand Piano sampler ─────────────────────────────────────────
// Free Steinway D recording by Alexander Holm, hosted by Tone.js.
// Samples every minor third across the full 88-key range (~2 MB total).
const SALAMANDER_BASE = 'https://tonejs.github.io/audio/salamander/';
const SALAMANDER_NOTES: [string, number][] = [
  ['A0',21],['C1',24],['Ds1',27],['Fs1',30],
  ['A1',33],['C2',36],['Ds2',39],['Fs2',42],
  ['A2',45],['C3',48],['Ds3',51],['Fs3',54],
  ['A3',57],['C4',60],['Ds4',63],['Fs4',66],
  ['A4',69],['C5',72],['Ds5',75],['Fs5',78],
  ['A5',81],['C6',84],['Ds6',87],['Fs6',90],
  ['A6',93],['C7',96],['Ds7',99],['Fs7',102],
  ['A7',105],['C8',108],
];

class PianoSampler {
  private buffers = new Map<number, AudioBuffer>();
  private fetchPromise: Promise<void> | null = null;

  // Phase 1: download MP3 bytes (no AudioContext needed, safe before user gesture)
  fetch(onProgress?: (loaded: number, total: number) => void): Promise<void> {
    if (this.fetchPromise) return this.fetchPromise;
    const rawBuffers = new Map<number, ArrayBuffer>();
    let loaded = 0;
    this.fetchPromise = Promise.all(
      SALAMANDER_NOTES.map(async ([name, midi]) => {
        const res = await fetch(`${SALAMANDER_BASE}${name}.mp3`);
        rawBuffers.set(midi, await res.arrayBuffer());
        onProgress?.(++loaded, SALAMANDER_NOTES.length);
      })
    ).then(async () => {
      // Phase 2 is kicked off by decode() — store raw so decode() can use them
      (this as any)._raw = rawBuffers;
    });
    return this.fetchPromise;
  }

  // Phase 2: decode ArrayBuffers using the live AudioContext (call after fetch resolves)
  async decode(ctx: AudioContext): Promise<void> {
    const raw: Map<number, ArrayBuffer> = (this as any)._raw;
    if (!raw) return;
    await Promise.all(
      Array.from(raw.entries()).map(async ([midi, ab]) => {
        const buf = await ctx.decodeAudioData(ab);
        this.buffers.set(midi, buf);
      })
    );
    (this as any)._raw = null;
  }

  get isReady() { return this.buffers.size === SALAMANDER_NOTES.length; }

  scheduleNote(
    ctx: AudioContext, out: AudioNode,
    pitch: number, vel: number, wallStart: number, dur: number,
  ): (now: number) => void {
    // Nearest sample — at most 1.5 semitones away, inaudible pitch error
    let nearest = 21, minDist = Infinity;
    for (const [, midi] of SALAMANDER_NOTES) {
      const d = Math.abs(midi - pitch);
      if (d < minDist) { minDist = d; nearest = midi; }
    }
    const buffer = this.buffers.get(nearest)!;
    const source = ctx.createBufferSource();
    source.buffer = buffer;
    source.playbackRate.value = Math.pow(2, (pitch - nearest) / 12);

    const gain = ctx.createGain();
    const amp = vel * 0.82;
    // Hold at full level while the key is down, then let the string ring out.
    // Release time is pitch-dependent: bass strings sustain longer than treble.
    const releaseTime = Math.max(0.6, Math.min(4.0, 2.5 - (pitch - 69) * 0.02));
    gain.gain.setValueAtTime(amp, wallStart);
    gain.gain.setValueAtTime(amp, wallStart + dur);
    gain.gain.exponentialRampToValueAtTime(
      Math.max(0.0001, amp * 0.08), wallStart + dur + releaseTime
    );

    const totalDur = dur + releaseTime + 0.1;
    source.connect(gain);
    gain.connect(out);
    source.start(wallStart);
    source.stop(wallStart + totalDur);

    return (now: number) => {
      try {
        gain.gain.cancelScheduledValues(now);
        gain.gain.setValueAtTime(gain.gain.value, now);
        gain.gain.linearRampToValueAtTime(0.0001, now + 0.03);
        source.stop(now + 0.04);
      } catch { /* already stopped */ }
    };
  }
}

// Module-level singleton so samples survive component re-mounts
let globalPianoSampler: PianoSampler | null = null;

interface Props {
  mp3Path: string;
  beats: number[];
  onBeatsChange: (beats: number[]) => void;
  stretches: Stretch[];
  onStretchesChange: (stretches: Stretch[]) => void;
  midiBeats: number[];
  onMidiBeatsChange: (midiBeats: number[]) => void;
}

function formatTime(seconds: number): string {
  const m = Math.floor(seconds / 60);
  const s = Math.floor(seconds % 60);
  const cs = Math.floor((seconds % 1) * 10);
  return `${m}:${String(s).padStart(2, "0")}.${cs}`;
}

interface PositionEvent { t: number; playing: boolean; }
interface LoadResult {
  peaks: number[][];
  bass_peaks: number[][];
  duration: number;
  sample_rate: number;
  channels: number;
  spec_cols: number;
  spec_cols_per_sec: number;
  spec_midi_lo: number;
  spec_midi_hi: number;
  spec_raw: { data: string; bins: number };       // fine magnitude, base64 flat u8
  spec_salience: { data: string; bins: number };  // harmonic-sum pitch salience
}

// Inferno colormap (sampled from matplotlib) — perceptually designed for spectrograms.
// Goes black → dark purple → red → orange → bright yellow, giving high contrast
// across the full dynamic range.
const INFERNO = (() => {
  const pts: [number, number, number][] = [
    [0,0,4],[14,6,36],[30,10,68],[50,11,93],[72,12,108],[94,13,117],
    [116,14,118],[135,18,113],[155,25,100],[174,35,84],[190,50,65],
    [203,67,48],[213,85,32],[221,104,17],[229,122,6],[237,143,8],
    [243,165,27],[247,188,55],[250,210,89],[252,232,130],[252,255,164],
  ];
  const lut = new Uint8Array(256 * 3);
  for (let i = 0; i < 256; i++) {
    const t  = i / 255 * (pts.length - 1);
    const lo = Math.floor(t);
    const hi = Math.min(lo + 1, pts.length - 1);
    const f  = t - lo;
    lut[i*3]   = Math.round(pts[lo][0] + (pts[hi][0] - pts[lo][0]) * f);
    lut[i*3+1] = Math.round(pts[lo][1] + (pts[hi][1] - pts[lo][1]) * f);
    lut[i*3+2] = Math.round(pts[lo][2] + (pts[hi][2] - pts[lo][2]) * f);
  }
  return lut;
})();

// ── Piano Roll ─────────────────────────────────────────────────────────────
type MidiNote = { time: number; dur: number; pitch: number; vel: number };
interface MidiTrack { name: string; notes: MidiNote[]; isPiano: boolean; }
interface TrackStats {
  density:       number; // 0–1, notes/sec relative to busiest track
  ioiRegularity: number; // 0–1, 1 = perfectly regular (low IOI variance)
}
type RollOptions = {
  durationAlpha:   boolean;
  attackFlash:     boolean;
  compressSustain: boolean;
  densityWeight:   boolean;
  velocityAlpha:   boolean;
  ioiRegularity:   boolean;
  gridLock:        boolean;
};
const DEFAULT_ROLL_OPTIONS: RollOptions = {
  durationAlpha: false, attackFlash: false, compressSustain: false,
  densityWeight: false, velocityAlpha: false, ioiRegularity: false, gridLock: false,
};
const ROLL_OPTION_LABELS: { key: keyof RollOptions; label: string; title: string }[] = [
  { key: 'durationAlpha',   label: 'Dur α',    title: 'Short notes bright, sustained notes faint' },
  { key: 'attackFlash',     label: 'Flash',    title: 'Bright 2px strike at every note onset' },
  { key: 'compressSustain', label: 'Squeeze',  title: 'Compress sustained notes to half height' },
  { key: 'densityWeight',   label: 'Density',  title: 'Busier tracks rendered brighter' },
  { key: 'velocityAlpha',   label: 'Vel α',    title: 'Loud notes opaque, soft notes transparent' },
  { key: 'ioiRegularity',   label: 'IOI',      title: 'Show beat-regularity bar in legend' },
  { key: 'gridLock',        label: 'Grid',     title: 'Highlight tracks that play on beats' },
];
const PIANO_ROLL_H = 220;   // px
const KEYBOARD_W   = 40;    // px, left keyboard strip
// Perceptually-distinct hues, ordered so the first 8 are maximally far apart
// (no two within ~40°) and no green cluster. Assigned by prominence rank so
// the most-featured instruments always get the most visually separated colors.
const PALETTE_HUES = [
   0,   // red
 210,   // sky-blue
  45,   // amber
 280,   // purple
 160,   // emerald
 330,   // rose
 195,   // teal
  85,   // lime
  25,   // orange
 255,   // indigo
 310,   // violet-pink
 130,   // green
  60,   // yellow
 230,   // cornflower
 350,   // crimson
 170,   // seafoam
 300,   // magenta
 100,   // chartreuse
 220,   // blue
  10,   // red-orange
];

function computeTrackColors(tracks: MidiTrack[]): string[] {
  const nonPiano = tracks
    .map((t, i) => ({ i, count: t.notes.length }))
    .filter((_, idx) => !tracks[idx].isPiano)
    .sort((a, b) => b.count - a.count);

  // rankOf[trackIndex] = 0 for most notes, increasing for fewer notes
  const rankOf = new Map(nonPiano.map((x, rank) => [x.i, rank]));
  const total = nonPiano.length || 1;

  return tracks.map((t, i) => {
    if (t.isPiano) return '#ffffff';
    const rank = rankOf.get(i) ?? total - 1;
    const prominence = 1 - rank / total; // 1 = most notes, 0 = fewest
    const hue = PALETTE_HUES[rank % PALETTE_HUES.length];
    const sat = Math.round(55 + prominence * 40);  // 55 – 95 %
    const lit = Math.round(42 + prominence * 28);  // 42 – 70 %
    return `hsl(${hue},${sat}%,${lit}%)`;
  });
}
const BLACK_PCS = new Set([1, 3, 6, 8, 10]);

// ── Spectrogram live controls ──────────────────────────────────────────────
export interface SpecCtrl {
  mode: 'raw' | 'melody'; // raw magnitude vs harmonic-salience (melody) layer
  gain: number;           // dB added to every cell (brightness)
  floor: number;          // dB black-point (noise gate)
  gamma: number;          // contrast curve
  lo: number;             // MIDI low edge of the visible pitch window
  hi: number;             // MIDI high edge
}
const DEFAULT_SPEC_CTRL: SpecCtrl = { mode: 'raw', gain: 0, floor: -68, gamma: 0.7, lo: 21, hi: 108 };
function loadSpecCtrl(): SpecCtrl {
  try {
    const raw = localStorage.getItem("beats_spec_ctrl");
    if (raw) return { ...DEFAULT_SPEC_CTRL, ...JSON.parse(raw) };
  } catch { /* ignore */ }
  return DEFAULT_SPEC_CTRL;
}
const NOTE_NAMES = ['C', 'C♯', 'D', 'D♯', 'E', 'F', 'F♯', 'G', 'G♯', 'A', 'A♯', 'B'];
export function midiNoteName(m: number): string {
  const n = Math.round(m);
  return `${NOTE_NAMES[((n % 12) + 12) % 12]}${Math.floor(n / 12) - 1}`;
}

// Parse the track colors we generate ('#rrggbb' or 'hsl(h,s%,l%)') to RGB
// for the Rust video renderer.
function cssColorToRgb(c: string): [number, number, number] {
  if (c.startsWith('#')) {
    const v = parseInt(c.slice(1), 16);
    return [(v >> 16) & 255, (v >> 8) & 255, v & 255];
  }
  const m = c.match(/hsl\(\s*([\d.]+)\s*,\s*([\d.]+)%\s*,\s*([\d.]+)%\s*\)/);
  if (!m) return [255, 255, 255];
  const h = parseFloat(m[1]) / 360, s = parseFloat(m[2]) / 100, l = parseFloat(m[3]) / 100;
  const q = l < 0.5 ? l * (1 + s) : l + s - l * s;
  const p = 2 * l - q;
  const channel = (t: number) => {
    t = ((t % 1) + 1) % 1;
    if (t < 1 / 6) return p + (q - p) * 6 * t;
    if (t < 1 / 2) return q;
    if (t < 2 / 3) return p + (q - p) * (2 / 3 - t) * 6;
    return p;
  };
  return [
    Math.round(channel(h + 1 / 3) * 255),
    Math.round(channel(h) * 255),
    Math.round(channel(h - 1 / 3) * 255),
  ];
}

const ZOOM_STEP = 1.5 ** 0.2; // ~1.084 — 5× less sensitive than original 1.5
const ZOOM_MAX_MULTIPLIER = 500;
const ZOOM_DEBOUNCE_MS = 80;
const PLAYBACK_RATES = [0.25, 0.5, 0.75, 1.0];

export default function WaveformEditor({
  mp3Path, beats, onBeatsChange, stretches, onStretchesChange, midiBeats, onMidiBeatsChange,
}: Props) {
  // ── DOM / WaveSurfer refs ──────────────────────────────────────────────────
  const containerRef = useRef<HTMLDivElement>(null);
  const wsRef = useRef<WaveSurfer | null>(null);
  const regionsRef = useRef<RegionsPlugin | null>(null);
  const bassContainerRef = useRef<HTMLDivElement>(null);
  const bassWsRef = useRef<WaveSurfer | null>(null);
  const beatsStripOuterRef = useRef<HTMLDivElement>(null);
  const beatsStripInnerRef = useRef<HTMLDivElement>(null);

  // ── Zoom refs ──────────────────────────────────────────────────────────────
  const fitPxPerSecRef = useRef<number>(0);
  const zoomPxPerSecRef = useRef<number>(0);
  const zoomTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const zoomRafRef = useRef<number | null>(null);
  const beatLabelElsRef = useRef<Map<number, HTMLElement>>(new Map());

  // ── Stable refs for event handlers ────────────────────────────────────────
  const beatsRef = useRef<number[]>(beats);
  const stretchesRef = useRef<Stretch[]>(stretches);
  const onBeatsChangeRef = useRef(onBeatsChange);
  const onStretchesChangeRef = useRef(onStretchesChange);
  const onMidiBeatsChangeRef = useRef(onMidiBeatsChange);
  const recordingRef = useRef(false);
  const recordingStartTimeRef = useRef(0);
  const pendingTapsRef = useRef<number[]>([]);
  const selectedBeatTimeRef = useRef<number | null>(null);
  const shiftHeldRef = useRef(false);
  const cmdHeldRef = useRef(false);
  const selectionRef = useRef<{ start: number; end: number } | null>(null);
  const regionJustClickedRef = useRef(false);
  const durationRef = useRef(0);          // STRETCHED (display/output) duration
  const origDurationRef = useRef(0);      // ORIGINAL recording duration
  const segmentsRef = useRef<TimeSegment[]>([]); // original↔stretched piecewise map
  const currentTimeRef = useRef(0);       // STRETCHED time
  const playingRef = useRef(false);
  const selectedTimelineRef = useRef<'mp3' | 'midi'>('mp3');

  // The editor's visible timeline is STRETCHED (output) time so that stretches
  // visibly rescale the waveform. Beats and stretches are STORED in original
  // time; these convert at the render / engine boundaries. With no stretches
  // both are the identity, so behavior is unchanged until a stretch exists.
  function o2s(t: number) { return originalToStretched(t, stretchesRef.current); }
  function s2o(t: number) { return stretchedToOriginal(t, segmentsRef.current); }
  // MIDI time ↔ display (stretched) time: warp via beat pairs, then apply stretch.
  // The piano roll is drawn on the same x-axis as the waveform, and MIDI
  // playback rides this axis so it follows stretches and stays in sync.
  function midiToDisp(mt: number) {
    const hasWarp = midiBeatsRef.current.length > 0 && beatsRef.current.length > 0;
    return o2s(hasWarp ? warpMidiTime(mt) : mt);
  }
  function dispToMidi(st: number) {
    const hasWarp = midiBeatsRef.current.length > 0 && beatsRef.current.length > 0;
    return hasWarp ? audioTimeToMidiTime(s2o(st)) : s2o(st);
  }

  // ── State ──────────────────────────────────────────────────────────────────
  const [loadProgress, setLoadProgress] = useState(0);
  const [ready, setReady] = useState(false);
  const [playing, setPlaying] = useState(false);
  const [selectedTimeline, setSelectedTimeline] = useState<'mp3' | 'midi'>('mp3');
  const [currentTime, setCurrentTime] = useState(0);
  const [duration, setDuration] = useState(0);
  const [zoomPxPerSec, setZoomPxPerSec] = useState(0);
  const [playbackRate, setPlaybackRate] = useState(1.0);
  const [recording, setRecording] = useState(false);
  const [selectedBeatTime, setSelectedBeatTime] = useState<number | null>(null);
  const [selection, setSelection] = useState<{ start: number; end: number } | null>(null);
  const [stretchModal, setStretchModal] = useState<{ start: number; end: number; existingFactor?: number } | null>(null);
  const [exportProgress, setExportProgress] = useState<number | null>(null);
  const [exportError, setExportError] = useState<string | null>(null);
  const [videoModalOpen, setVideoModalOpen] = useState(false);
  const [videoProgress, setVideoProgress] = useState<{ pct: number; stage: string } | null>(null);
  const [videoError, setVideoError] = useState<string | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [rateProcessing, setRateProcessing] = useState(false);
  // PCM decode happens in background after waveform is shown
  const [pcmReady, setPcmReady] = useState(false);
  const [loadPhase, setLoadPhase] = useState("");
  const [playBeats, setPlayBeats] = useState(() =>
    localStorage.getItem("beats_play_beats") !== "false"
  );
  const playBeatsRef = useRef(playBeats);
  const audioCtxRef = useRef<AudioContext | null>(null);
  const lastAudioPosRef = useRef<number>(-1);
  const lastTimeUiRef = useRef<number>(0); // throttle clock for the time-readout re-render
  const lastSeekRef = useRef<number>(0);   // throttle clock for cursor seek/auto-scroll

  // ── Spectrogram ────────────────────────────────────────────────────────────
  const specCanvasRef = useRef<HTMLCanvasElement | null>(null);
  const specRawRef = useRef<Uint8Array | null>(null);   // fine magnitude layer
  const specRawBinsRef = useRef(0);
  const specSalRef = useRef<Uint8Array | null>(null);   // harmonic-salience layer
  const specSalBinsRef = useRef(0);
  const specColsRef = useRef(0);
  const specColsPerSecRef = useRef(0);
  const specMidiLoRef = useRef(21);
  const specMidiHiRef = useRef(108);
  // Spectrogram render caches: a 256-entry color LUT (rebuilt only when the
  // dials change) and a reused ImageData buffer, so the per-frame draw avoids
  // a Math.pow per pixel and a ~1 MB allocation each frame.
  const specLutRef = useRef<Uint8ClampedArray>(new Uint8ClampedArray(256 * 4));
  const specLutKeyRef = useRef<string>("");
  const specImageDataRef = useRef<ImageData | null>(null);
  const scrollLeftRef = useRef(0);
  const [showSpec, setShowSpec] = useState(() =>
    localStorage.getItem("beats_show_spec") !== "false"
  );
  const showSpecRef = useRef(showSpec);
  useEffect(() => { showSpecRef.current = showSpec; }, [showSpec]);
  const [specCtrlOpen, setSpecCtrlOpen] = useState(() =>
    localStorage.getItem("beats_spec_ctrl_open") === "true"
  );

  // Live spectrogram dials (the "ultrasound" controls), persisted.
  const [specCtrl, setSpecCtrl] = useState<SpecCtrl>(loadSpecCtrl);
  const specCtrlRef = useRef<SpecCtrl>(specCtrl);
  useEffect(() => {
    specCtrlRef.current = specCtrl;
    localStorage.setItem("beats_spec_ctrl", JSON.stringify(specCtrl));
    drawSpectrogram();
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [specCtrl]);

  // ── Piano Roll state ───────────────────────────────────────────────────────
  const [midiLegend, setMidiLegend] = useState<{ name: string; color: string }[]>([]);
  const [soloTrackIndex, setSoloTrackIndex] = useState<number | null>(null);
  const soloTrackIndexRef = useRef<number | null>(null);
  const [rollOptions, setRollOptions] = useState<RollOptions>(DEFAULT_ROLL_OPTIONS);
  const pianoRollCanvasRef = useRef<HTMLCanvasElement | null>(null);
  const pianoKeyCanvasRef  = useRef<HTMLCanvasElement | null>(null);
  const midiTracksRef      = useRef<MidiTrack[]>([]);
  const midiTrackColorsRef = useRef<string[]>([]);
  const midiTrackStatsRef  = useRef<TrackStats[]>([]);
  const gridLockCacheRef   = useRef<{ key: string; scores: number[] } | null>(null);
  const rollOptionsRef     = useRef<RollOptions>(DEFAULT_ROLL_OPTIONS);
  const midiRangeRef       = useRef({ min: 21, max: 108 });
  const drawRafRef         = useRef<number | null>(null); // coalesces canvas redraws
  const viewPersistTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  // Precomputed display (stretched) positions, so the per-frame piano-roll draw
  // does no warp/stretch math. Per track: [t0,t1, t0,t1, …]; rebuilt only when
  // beats / midiBeats / stretches / the MIDI itself change.
  const midiDispNotesRef   = useRef<Float64Array[]>([]);
  const midiDispBeatsRef   = useRef<Float64Array>(new Float64Array(0));

  // ── MIDI playback engine ───────────────────────────────────────────────────
  const midiAudioCtxRef    = useRef<AudioContext & { masterOut?: AudioNode } | null>(null);
  const midiGainRef        = useRef<GainNode | null>(null); // master volume for MIDI
  const [midiVolume, setMidiVolume] = useState(() => {
    const v = parseFloat(localStorage.getItem("beats_midi_volume") ?? "1");
    return isFinite(v) ? v : 1;
  });
  const midiVolumeRef      = useRef(midiVolume);
  const midiPlayingRef     = useRef(false);
  const midiStartWallRef   = useRef(0);   // audioCtx.currentTime when play was pressed
  const midiStartPosRef    = useRef(0);   // MIDI cursor position (MIDI time) at play start
  const midiStartAudioRef  = useRef(0);   // audio timeline position at play start
  const midiScheduledToRef = useRef(0);   // audio time we've scheduled notes up to
  const midiActiveNodesRef = useRef<Array<{ stop: (now: number) => void }>>([]);
  const midiSchedulerRef   = useRef<ReturnType<typeof setInterval> | null>(null);
  const midiCursorRafRef   = useRef<number | null>(null);
  const midiCursorRef      = useRef(0);   // current MIDI cursor (MIDI seconds)
  const midiDurationRef    = useRef(0);   // total MIDI duration

  const [midiPlaying, setMidiPlaying]   = useState(false);
  const [midiCursorDisp, setMidiCursorDisp] = useState(0); // for header display only
  const [samplerStatus, setSamplerStatus] = useState<'idle' | 'loading' | 'ready' | 'error'>(() =>
    globalPianoSampler?.isReady ? 'ready' : 'idle'
  );
  const [samplerProgress, setSamplerProgress] = useState(0);
  const midiBeatsRef = useRef<number[]>(midiBeats);
  const [selectedMidiBeat, setSelectedMidiBeat] = useState<number | null>(null);
  const selectedMidiBeatRef = useRef<number | null>(null);
  useEffect(() => { selectedMidiBeatRef.current = selectedMidiBeat; }, [selectedMidiBeat]);

  useEffect(() => { playBeatsRef.current = playBeats; }, [playBeats]);
  useEffect(() => {
    midiVolumeRef.current = midiVolume;
    localStorage.setItem("beats_midi_volume", String(midiVolume));
    const g = midiGainRef.current;
    const ctx = midiAudioCtxRef.current;
    if (g && ctx) g.gain.setTargetAtTime(midiVolume, ctx.currentTime, 0.02);
  }, [midiVolume]);

  // Keep refs in sync
  useEffect(() => { beatsRef.current = beats; rebuildMidiDispCache(); drawPianoRoll(); }, [beats]);
  useEffect(() => {
    // Keep the ref sorted by start: originalToStretched (a hot-path helper) now
    // assumes sorted input to avoid per-call allocation.
    const sorted = [...stretches].sort((a, b) => a.start - b.start);
    stretchesRef.current = sorted;
    segmentsRef.current = buildSegments(origDurationRef.current, sorted);
    rebuildMidiDispCache();
  }, [stretches]);
  useEffect(() => { onBeatsChangeRef.current = onBeatsChange; }, [onBeatsChange]);
  useEffect(() => { onStretchesChangeRef.current = onStretchesChange; }, [onStretchesChange]);
  useEffect(() => { onMidiBeatsChangeRef.current = onMidiBeatsChange; }, [onMidiBeatsChange]);
  useEffect(() => { midiBeatsRef.current = midiBeats; rebuildMidiDispCache(); drawPianoRoll(); }, [midiBeats]);
  useEffect(() => { soloTrackIndexRef.current = soloTrackIndex; drawPianoRoll(); }, [soloTrackIndex]);
  useEffect(() => { rollOptionsRef.current = rollOptions; drawPianoRoll(); }, [rollOptions]);
  useEffect(() => { selectedBeatTimeRef.current = selectedBeatTime; }, [selectedBeatTime]);
  useEffect(() => { selectionRef.current = selection; }, [selection]);
  useEffect(() => { durationRef.current = duration; }, [duration]);
  useEffect(() => { currentTimeRef.current = currentTime; }, [currentTime]);
  useEffect(() => { playingRef.current = playing; }, [playing]);

  // Update cursor colors when the selected timeline changes
  useEffect(() => {
    const mp3Color = selectedTimeline === 'mp3' ? '#44ff88' : '#ffdd44';
    wsRef.current?.setOptions({ cursorColor: mp3Color });
    bassWsRef.current?.setOptions({ cursorColor: mp3Color });
    drawPianoRoll();
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selectedTimeline]);

  // ── WaveSurfer creation (display only — Rust drives audio) ────────────────
  useEffect(() => {
    if (!containerRef.current) return;

    const regions = RegionsPlugin.create();
    regionsRef.current = regions;

    const ws = WaveSurfer.create({
      container: containerRef.current,
      waveColor: "#5a4fcf",
      progressColor: "#9b8eff",
      cursorColor: "#ffdd44",
      cursorWidth: 2,
      height: containerRef.current.clientHeight || 128,
      normalize: true,
      autoScroll: true,
      autoCenter: true,
      interact: true,
      plugins: [
        Timeline.create({ style: { color: "#a0a0b0", fontSize: "11px" } }),
        regions,
      ],
    });

    ws.on("ready", (dur) => {
      setReady(true);
      const fit = containerRef.current!.clientWidth / dur;
      fitPxPerSecRef.current = fit;

      const savedRaw = localStorage.getItem(`beats_view_${mp3Path}`);
      const saved = savedRaw ? JSON.parse(savedRaw) as { multiplier: number; scrollTime: number } : null;
      const multiplier = saved?.multiplier ?? 1;
      const scrollTime = saved?.scrollTime ?? 0;
      const zoom = fit * Math.max(1, multiplier);
      zoomPxPerSecRef.current = zoom;
      setZoomPxPerSec(zoom);

      if (multiplier > 1) {
        ws.zoom(zoom);
        // Bass WaveSurfer may not have loaded yet here — zoom it after its own ready event
      }

      // Restore scroll after the DOM has applied the zoom
      requestAnimationFrame(() => {
        const scrollLeft = scrollTime * zoom;
        const scrollEl = ws.getWrapper().parentElement as HTMLElement | null;
        if (scrollEl) scrollEl.scrollLeft = scrollLeft;
        bassWsRef.current?.setScroll(scrollLeft);
        scrollLeftRef.current = scrollLeft;
        if (beatsStripInnerRef.current)
          beatsStripInnerRef.current.style.transform = `translateX(-${scrollLeft}px)`;
        drawSpectrogram();
        drawPianoRoll();
      });

      setLoadProgress(100);
      drawSpectrogram();
      drawPianoRoll();
    });

    ws.on("scroll", (_s: number, _e: number, scrollLeft: number) => {
      scrollLeftRef.current = scrollLeft;
      // Transform is cheap and must track the cursor tightly → keep synchronous.
      if (beatsStripInnerRef.current) {
        beatsStripInnerRef.current.style.transform = `translateX(-${scrollLeft}px)`;
      }
      scheduleDraw(); // batch canvas repaints to one per frame
      // Persist view state — debounced so we don't stringify+write every event.
      if (viewPersistTimerRef.current) clearTimeout(viewPersistTimerRef.current);
      viewPersistTimerRef.current = setTimeout(() => {
        const fit = fitPxPerSecRef.current;
        if (fit) {
          const multiplier = zoomPxPerSecRef.current / fit;
          const scrollT = scrollLeftRef.current / zoomPxPerSecRef.current;
          localStorage.setItem(`beats_view_${mp3Path}`, JSON.stringify({ multiplier, scrollTime: scrollT }));
        }
      }, 250);
    });

    ws.on("interaction", (t: number) => {
      if (regionJustClickedRef.current) return;

      // Any interaction with the top timeline selects it
      selectedTimelineRef.current = 'mp3';
      setSelectedTimeline('mp3');

      if (cmdHeldRef.current) {
        // Cmd+click: add a beat (t is display time; beats store original time)
        const updated = [...beatsRef.current, s2o(t)].sort((a, b) => a - b);
        onBeatsChangeRef.current(updated);
        return;
      }

      if (shiftHeldRef.current) {
        // Shift+click: set selection from current cursor to clicked point
        const anchor = currentTimeRef.current;
        const start = Math.min(anchor, t);
        const end = Math.max(anchor, t);
        if (end - start > 0.01) {
          selectionRef.current = { start, end };
          setSelection({ start, end });
        }
        return;
      }

      // Plain click: seek and clear any selection
      handleSeek(t);
      selectionRef.current = null;
      setSelection(null);

      // Sync MIDI cursor if t is within the jointly-annotated range (original time)
      {
        const to = s2o(t);
        const mb = midiBeatsRef.current;
        const ab = beatsRef.current;
        const n = Math.min(mb.length, ab.length);
        if (n >= 2 && to >= ab[0] && to <= ab[n - 1]) {
          midiSeekTo(audioTimeToMidiTime(to));
        }
      }
    });

    // Ctrl+wheel zoom
    wsRef.current = ws;

    return () => {
      ws.destroy();
      wsRef.current = null;
      regionsRef.current = null;
    };
  }, [mp3Path]);

  // ── Bass WaveSurfer (separate small instance) ─────────────────────────────
  useEffect(() => {
    if (!bassContainerRef.current) return;

    const bws = WaveSurfer.create({
      container: bassContainerRef.current,
      waveColor: "rgba(251, 146, 60, 0.75)",
      progressColor: "rgba(251, 146, 60, 0.4)",
      cursorColor: "#ffdd44",
      cursorWidth: 2,
      height: bassContainerRef.current.clientHeight || 52,
      normalize: true,
      autoScroll: false,
      autoCenter: false,
      interact: false,
      hideScrollbar: true,
    });

    // Sync scroll from main waveform
    const unsub = wsRef.current?.on("scroll", (_s, _e, scrollLeft: number) => {
      bws.setScroll(scrollLeft);
    });

    // Apply saved zoom once bass waveform has its own data loaded
    bws.on("ready", () => {
      const z = zoomPxPerSecRef.current;
      if (z > 0) bws.zoom(z);
    });

    bassWsRef.current = bws;
    return () => {
      unsub?.();
      bws.destroy();
      bassWsRef.current = null;
    };
  }, [mp3Path]);

  // ── Resize observer: keep WaveSurfer height in sync with container ────────
  useEffect(() => {
    const el = containerRef.current;
    if (!el) return;
    const ro = new ResizeObserver(entries => {
      const h = entries[0]?.contentRect.height;
      if (h && h > 0) wsRef.current?.setOptions({ height: h });
    });
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  // ── Spectrogram canvas (injected as overlay inside WaveSurfer container) ───
  useEffect(() => {
    const el = containerRef.current;
    if (!el) return;

    // Ensure container is positioned so absolute children are relative to it
    el.style.position = 'relative';

    const canvas = document.createElement('canvas');
    canvas.style.cssText =
      'position:absolute;top:0;left:0;width:100%;pointer-events:none;z-index:5;opacity:0.8;';
    el.appendChild(canvas);
    specCanvasRef.current = canvas;

    const elNN = el; // capture non-null for closure
    function resize() {
      canvas.width = elNN.clientWidth;
      canvas.height = elNN.clientHeight;
      drawSpectrogram();
    }

    const ro = new ResizeObserver(resize);
    ro.observe(el);
    resize();

    return () => {
      ro.disconnect();
      canvas.remove();
      specCanvasRef.current = null;
    };
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [mp3Path]);

  // ── Piano Roll canvas setup ────────────────────────────────────────────────
  useEffect(() => {
    const container = document.querySelector('.piano-roll-notes') as HTMLDivElement | null;
    const keyContainer = document.querySelector('.piano-key-wrap') as HTMLDivElement | null;
    if (!container || !keyContainer) return;

    const noteCanvas = document.createElement('canvas');
    noteCanvas.style.cssText = 'display:block;width:100%;height:100%;';
    container.appendChild(noteCanvas);
    pianoRollCanvasRef.current = noteCanvas;

    const keyCanvas = document.createElement('canvas');
    keyCanvas.style.cssText = 'display:block;width:100%;height:100%;';
    keyContainer.appendChild(keyCanvas);
    pianoKeyCanvasRef.current = keyCanvas;

    function resize() {
      noteCanvas.width  = container!.clientWidth;
      noteCanvas.height = container!.clientHeight;
      keyCanvas.width   = keyContainer!.clientWidth;
      keyCanvas.height  = keyContainer!.clientHeight;
      drawPianoRoll();
      drawPianoKeyboard();
    }

    function onPointerDown(e: PointerEvent) {
      if (e.button !== 0) return;

      // Any click in the piano roll selects the bottom timeline
      selectedTimelineRef.current = 'midi';
      setSelectedTimeline('midi');

      const rect = noteCanvas.getBoundingClientRect();
      const x    = e.clientX - rect.left;

      // Cmd/Ctrl+click → add a new MIDI beat
      if (e.metaKey || e.ctrlKey) {
        e.preventDefault();
        const midiT = canvasXToMidiTime(x);
        const next = [...midiBeatsRef.current, midiT].sort((a, b) => a - b);
        midiBeatsRef.current = next;
        onMidiBeatsChangeRef.current(next);
        selectedMidiBeatRef.current = midiT;
        setSelectedMidiBeat(midiT);
        drawPianoRoll();
        return;
      }

      // Hit-test existing beat markers
      const hit: number | null = hitTestMidiBeat(x);
      if (hit !== null) {
        const hitBeat: number = hit;
        e.preventDefault();
        noteCanvas.setPointerCapture(e.pointerId);

        const startClientX  = e.clientX;
        const pxPerSec      = zoomPxPerSecRef.current;
        const startAudioT   = midiToDisp(hit); // display (stretched) time
        const startX        = startAudioT * pxPerSec - scrollLeftRef.current - KEYBOARD_W;
        let dragging        = false;
        let liveMidiT       = hit;

        function onMove(ev: PointerEvent) {
          const dx = ev.clientX - startClientX;
          if (!dragging && Math.abs(dx) > 3) {
            dragging = true;
            noteCanvas.style.cursor = 'grabbing';
          }
          if (!dragging) return;
          liveMidiT = canvasXToMidiTime(startX + dx);
          // Update ref for live drawing without touching state
          midiBeatsRef.current = midiBeatsRef.current
            .map(bt => Math.abs(bt - hitBeat) < 0.001 ? liveMidiT : bt)
            .sort((a, b) => a - b);
          selectedMidiBeatRef.current = liveMidiT;
          rebuildMidiDispCache(); // warp changed → refresh cached note positions
          drawPianoRoll();
        }

        function onUp() {
          noteCanvas.releasePointerCapture(e.pointerId);
          noteCanvas.removeEventListener('pointermove', onMove);
          noteCanvas.removeEventListener('pointerup',   onUp);
          noteCanvas.style.cursor = '';
          if (dragging) {
            onMidiBeatsChangeRef.current([...midiBeatsRef.current]);
            setSelectedMidiBeat(liveMidiT);
            selectedBeatTimeRef.current = null;
            setSelectedBeatTime(null);
          } else {
            // plain click: select / deselect
            const alreadySel = selectedMidiBeatRef.current !== null &&
              Math.abs(selectedMidiBeatRef.current - hitBeat) < 0.001;
            selectedMidiBeatRef.current = alreadySel ? null : hitBeat;
            setSelectedMidiBeat(alreadySel ? null : hitBeat);
            if (!alreadySel) {
              selectedBeatTimeRef.current = null;
              setSelectedBeatTime(null);
            }
            drawPianoRoll();
          }
        }

        noteCanvas.addEventListener('pointermove', onMove);
        noteCanvas.addEventListener('pointerup',   onUp);
        return;
      }

      // Empty area: pan on drag, seek on plain click (decided on pointerup)
      {
        const startClientX = e.clientX;
        const getScrollEl = () => {
          const ws = wsRef.current;
          return ws ? (ws.getWrapper().parentElement as HTMLElement | null) : null;
        };
        const startScrollLeft = getScrollEl()?.scrollLeft ?? 0;
        let panning = false;

        function onPanMove(ev: PointerEvent) {
          const dx = ev.clientX - startClientX;
          if (!panning && Math.abs(dx) > 4) {
            panning = true;
            noteCanvas.style.cursor = 'grabbing';
          }
          if (panning) {
            const scrollEl = getScrollEl();
            if (scrollEl) scrollEl.scrollLeft = startScrollLeft - dx;
          }
        }

        function onPanUp(ev: PointerEvent) {
          noteCanvas.removeEventListener('pointermove', onPanMove);
          noteCanvas.removeEventListener('pointerup', onPanUp);
          noteCanvas.style.cursor = '';
          if (panning) return;
          // Plain click — seek and optionally sync MP3 cursor
          const rect = noteCanvas.getBoundingClientRect();
          const midiT = canvasXToMidiTime(ev.clientX - rect.left);
          midiSeekTo(midiT);
          const mb = midiBeatsRef.current;
          const ab = beatsRef.current;
          const n = Math.min(mb.length, ab.length);
          if (n >= 2 && midiT >= mb[0] && midiT <= mb[n - 1]) {
            handleSeek(midiToDisp(midiT));
          }
        }

        noteCanvas.addEventListener('pointermove', onPanMove);
        noteCanvas.addEventListener('pointerup', onPanUp);
      }
    }

    function onPointerMove(e: PointerEvent) {
      if (e.buttons !== 0) return; // ignore while dragging
      const rect = noteCanvas.getBoundingClientRect();
      const x = e.clientX - rect.left;
      noteCanvas.style.cursor = hitTestMidiBeat(x) !== null ? 'grab' : '';
    }

    noteCanvas.addEventListener('pointerdown', onPointerDown);
    noteCanvas.addEventListener('pointermove', onPointerMove);
    const ro = new ResizeObserver(resize);
    ro.observe(container);
    resize();
    return () => {
      ro.disconnect();
      noteCanvas.removeEventListener('pointerdown', onPointerDown);
      noteCanvas.removeEventListener('pointermove', onPointerMove);
      noteCanvas.remove();
      keyCanvas.remove();
      pianoRollCanvasRef.current = null;
      pianoKeyCanvasRef.current  = null;
    };
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // ── Load MIDI on movement change ───────────────────────────────────────────
  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const { Midi } = await import('@tonejs/midi');
        const midi = await Midi.fromUrl('/midi/rach2_all.mid');
        if (cancelled) return;
        let minP = 127, maxP = 0;
        const tracks: MidiTrack[] = midi.tracks
          .filter(t => t.notes.length > 0)
          .map(t => {
            const notes = t.notes.map(n => {
              if (n.midi < minP) minP = n.midi;
              if (n.midi > maxP) maxP = n.midi;
              return { time: n.time, dur: n.duration, pitch: n.midi, vel: n.velocity };
            });
            return {
              name: t.name || t.instrument.name || 'Track',
              notes,
              isPiano: /piano(forte)?/i.test(t.name || ''),
            };
          });
        midiTracksRef.current  = tracks;
        midiRangeRef.current   = { min: Math.max(0, minP - 2), max: Math.min(127, maxP + 2) };
        midiDurationRef.current = midi.duration;

        const colors = computeTrackColors(tracks);
        midiTrackColorsRef.current = colors;

        // Per-track stats for rendering features
        const dur = midi.duration || 1;
        const rawStats = tracks.map(t => {
          const density = t.notes.length / dur;
          let ioiRegularity = 0;
          if (t.notes.length >= 3) {
            const iois = t.notes.slice(1).map((n, i) => n.time - t.notes[i].time);
            const mean = iois.reduce((a, b) => a + b, 0) / iois.length;
            const cv = mean > 0
              ? Math.sqrt(iois.reduce((s, x) => s + (x - mean) ** 2, 0) / iois.length) / mean
              : 1;
            ioiRegularity = Math.max(0, 1 - Math.min(1, cv));
          }
          return { density, ioiRegularity };
        });
        const maxDensity = Math.max(...rawStats.map(s => s.density), 1);
        midiTrackStatsRef.current = rawStats.map(s => ({
          ...s, density: s.density / maxDensity,
        }));
        gridLockCacheRef.current = null; // invalidate on reload
        midiPause();
        midiCursorRef.current = 0;
        setMidiCursorDisp(0);
        setSoloTrackIndex(null);
        setMidiLegend(tracks.map((t, i) => ({ name: t.name, color: colors[i] })));
        rebuildMidiDispCache();
        drawPianoRoll();
        drawPianoKeyboard();

        // Eagerly prefetch Steinway samples as soon as MIDI loads — no AudioContext
        // needed for the download phase, so this is safe before any user gesture.
        if (tracks.some(t => t.isPiano) && !globalPianoSampler?.isReady) {
          if (!globalPianoSampler) globalPianoSampler = new PianoSampler();
          setSamplerStatus('loading');
          setSamplerProgress(0);
          globalPianoSampler.fetch((n, total) =>
            setSamplerProgress(Math.round(n / total * 100))
          ).catch(() => setSamplerStatus('error'));
          // Decode happens in midiPlay() once the AudioContext exists.
        }
      } catch (e) {
        if (!cancelled) console.error('MIDI load failed:', e);
      }
    })();
    return () => { cancelled = true; };
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // ── Spectrogram draw (reads only refs — stable across renders) ─────────────
  function drawSpectrogram() {
    const canvas = specCanvasRef.current;
    if (!canvas || !showSpecRef.current) {
      if (canvas) {
        const ctx = canvas.getContext('2d');
        ctx?.clearRect(0, 0, canvas.width, canvas.height);
      }
      return;
    }
    const ctrl = specCtrlRef.current;
    const specData = ctrl.mode === 'melody' ? specSalRef.current : specRawRef.current;
    const bins = ctrl.mode === 'melody' ? specSalBinsRef.current : specRawBinsRef.current;
    if (!specData || bins === 0) return;

    const width = canvas.width;
    const height = canvas.height;
    if (width === 0 || height === 0) return;

    const ctx = canvas.getContext('2d');
    if (!ctx) return;

    const scrollLeft = scrollLeftRef.current;
    const pxPerSec = zoomPxPerSecRef.current;
    const cols = specColsRef.current;
    const colsPerSec = specColsPerSecRef.current;
    if (cols === 0 || pxPerSec === 0) return;

    const midiLo = specMidiLoRef.current;
    const midiHi = specMidiHiRef.current;
    const midiSpan = midiHi - midiLo;          // bin b ↔ midi = midiLo + b/(bins-1)*midiSpan
    const viewLo = Math.max(midiLo, ctrl.lo);
    const viewHi = Math.min(midiHi, ctrl.hi);
    if (viewHi <= viewLo) return;

    // Build the dial→color lookup table only when the dials change. It maps a
    // stored u8 (−90..0 dB) through gain/floor/contrast to RGBA, so the pixel
    // loop is a table read instead of pow()/log() per pixel.
    const lutKey = `${ctrl.gain}|${ctrl.floor}|${ctrl.gamma}`;
    if (specLutKeyRef.current !== lutKey) {
      const DB_FLOOR = -90, DB_CEIL = 0, DB_RANGE = DB_CEIL - DB_FLOOR;
      const floor = ctrl.floor, span = Math.max(1, DB_CEIL - floor);
      const gain = ctrl.gain, invGamma = ctrl.gamma;
      const lut = specLutRef.current;
      for (let u = 0; u < 256; u++) {
        const db = DB_FLOOR + (u / 255) * DB_RANGE + gain;
        let tnorm = (db - floor) / span;
        const o = u * 4;
        if (tnorm <= 0) { lut[o] = lut[o + 1] = lut[o + 2] = lut[o + 3] = 0; continue; }
        if (tnorm > 1) tnorm = 1;
        const v = Math.pow(tnorm, invGamma);
        const pi3 = ((v * 255) | 0) * 3;
        lut[o]     = INFERNO[pi3];
        lut[o + 1] = INFERNO[pi3 + 1];
        lut[o + 2] = INFERNO[pi3 + 2];
        lut[o + 3] = 30 + v * 225;
      }
      specLutKeyRef.current = lutKey;
    }
    const lut = specLutRef.current;

    // Per output row: fractional bin + interpolation tap (constant across x).
    const b0 = new Int32Array(height);
    const b1 = new Int32Array(height);
    const bf = new Float32Array(height);
    for (let y = 0; y < height; y++) {
      const midi = viewHi - (y / height) * (viewHi - viewLo);
      const fb = midiSpan > 0 ? ((midi - midiLo) / midiSpan) * (bins - 1) : 0;
      const lo = Math.floor(fb);
      const hi = Math.min(bins - 1, lo + 1);
      b0[y] = Math.max(0, Math.min(bins - 1, lo));
      b1[y] = Math.max(0, hi);
      bf[y] = fb - lo;
    }

    // Reuse the ImageData buffer across frames (re-create only on resize).
    let imageData = specImageDataRef.current;
    if (!imageData || imageData.width !== width || imageData.height !== height) {
      imageData = ctx.createImageData(width, height);
      specImageDataRef.current = imageData;
    }
    const data = imageData.data;

    for (let x = 0; x < width; x++) {
      // Display x is stretched time; spectrogram columns are in original time.
      const t = s2o((scrollLeft + x) / pxPerSec);
      const col = Math.floor(t * colsPerSec);
      const valid = col >= 0 && col < cols;
      const colBase = col * bins;

      for (let y = 0; y < height; y++) {
        const idx = (y * width + x) * 4;
        if (!valid) { data[idx + 3] = 0; continue; } // clear stale pixel
        // Vertically interpolate between adjacent pitch bins, then LUT.
        const u8 = (specData[colBase + b0[y]] * (1 - bf[y]) + specData[colBase + b1[y]] * bf[y] + 0.5) | 0;
        const o = u8 * 4;
        data[idx]     = lut[o];
        data[idx + 1] = lut[o + 1];
        data[idx + 2] = lut[o + 2];
        data[idx + 3] = lut[o + 3];
      }
    }

    ctx.putImageData(imageData, 0, 0);
  }

  // ── Piano Roll helpers ─────────────────────────────────────────────────────
  // Convert a canvas x-pixel to MIDI time (inverse-warp if beats are aligned)
  function canvasXToMidiTime(x: number): number {
    // x is on the display (stretched) axis; map to original audio time first.
    const audioT = s2o((scrollLeftRef.current + KEYBOARD_W + x) / zoomPxPerSecRef.current);
    const mb = midiBeatsRef.current;
    const ab = beatsRef.current;
    const n = Math.min(mb.length, ab.length);
    if (n === 0) return audioT;
    if (audioT <= ab[0]) {
      const ratio = n >= 2 ? (mb[1] - mb[0]) / (ab[1] - ab[0]) : 1;
      return Math.max(0, mb[0] + (audioT - ab[0]) * ratio);
    }
    if (audioT >= ab[n - 1]) {
      const ratio = n >= 2 ? (mb[n - 1] - mb[n - 2]) / (ab[n - 1] - ab[n - 2]) : 1;
      return mb[n - 1] + (audioT - ab[n - 1]) * ratio;
    }
    for (let i = 0; i < n - 1; i++) {
      if (audioT >= ab[i] && audioT < ab[i + 1]) {
        const t = (audioT - ab[i]) / (ab[i + 1] - ab[i]);
        return mb[i] + t * (mb[i + 1] - mb[i]);
      }
    }
    return audioT;
  }

  // Find MIDI beat within HIT_RADIUS pixels of canvas x (in display space)
  function hitTestMidiBeat(x: number): number | null {
    const HIT = 6;
    const pxPerSec  = zoomPxPerSecRef.current;
    const scrollLeft = scrollLeftRef.current + KEYBOARD_W;
    const mb = midiBeatsRef.current;
    const ab = beatsRef.current;
    const hasWarp = mb.length > 0 && ab.length > 0;
    for (const bt of mb) {
      const t  = hasWarp ? o2s(warpMidiTime(bt)) : bt;
      const bx = t * pxPerSec - scrollLeft;
      if (Math.abs(x - bx) <= HIT) return bt;
    }
    return null;
  }

  // Recompute cached display positions for every MIDI note + beat. Only the
  // warp (beats) and stretch mapping change these, never zoom/scroll/playback,
  // so we do it here instead of per-note per-frame in drawPianoRoll.
  function rebuildMidiDispCache() {
    const tracks = midiTracksRef.current;
    midiDispNotesRef.current = tracks.map(t => {
      const arr = new Float64Array(t.notes.length * 2);
      for (let i = 0; i < t.notes.length; i++) {
        const n = t.notes[i];
        arr[2 * i]     = midiToDisp(n.time);
        arr[2 * i + 1] = midiToDisp(n.time + n.dur);
      }
      return arr;
    });
    const mb = midiBeatsRef.current;
    const beats = new Float64Array(mb.length);
    for (let i = 0; i < mb.length; i++) beats[i] = midiToDisp(mb[i]);
    midiDispBeatsRef.current = beats;
  }

  // Coalesce spectrogram + piano-roll redraws to at most one per animation
  // frame. WaveSurfer fires many scroll events per frame during playback;
  // without this each one triggered two full canvas repaints.
  function scheduleDraw() {
    if (drawRafRef.current != null) return;
    drawRafRef.current = requestAnimationFrame(() => {
      drawRafRef.current = null;
      drawSpectrogram();
      drawPianoRoll();
    });
  }

  // ── Piano Roll draw ────────────────────────────────────────────────────────
  function drawPianoRoll() {
    const canvas = pianoRollCanvasRef.current;
    if (!canvas) return;
    const tracks = midiTracksRef.current;
    const { width, height } = canvas;
    if (!width || !height) return;

    const ctx = canvas.getContext('2d');
    if (!ctx) return;
    ctx.clearRect(0, 0, width, height);
    ctx.fillStyle = '#080810';
    ctx.fillRect(0, 0, width, height);

    if (!tracks.length) return;

    const scrollLeft = scrollLeftRef.current + KEYBOARD_W;
    const pxPerSec   = zoomPxPerSecRef.current;
    if (!pxPerSec) return;

    const { min: minP, max: maxP } = midiRangeRef.current;
    const noteRange = maxP - minP + 1;
    const noteH = height / noteRange;

    // Black key bands and octave lines
    for (let p = minP; p <= maxP; p++) {
      const y = (maxP - p) * noteH;
      if (BLACK_PCS.has(p % 12)) {
        ctx.fillStyle = '#0c0c18';
        ctx.fillRect(0, y, width, noteH);
      }
      if (p % 12 === 0) {
        ctx.strokeStyle = '#1e1e30';
        ctx.lineWidth = 1;
        ctx.beginPath(); ctx.moveTo(0, y + 0.5); ctx.lineTo(width, y + 0.5); ctx.stroke();
      }
    }

    // ── Note rendering ─────────────────────────────────────────────────────────
    // Positions use display (stretched) time via midiToDisp, matching the
    // waveform's axis. tStart/tEnd cull off-screen notes in the same space.
    const opts = rollOptionsRef.current;
    const tStart = scrollLeft / pxPerSec;
    const tEnd   = (scrollLeft + width) / pxPerSec;

    // Avg beat duration in audio seconds (used by duration-alpha and compress)
    const mb = midiBeatsRef.current;
    const avgBeatDur = mb.length >= 2
      ? (mb[mb.length - 1] - mb[0]) / (mb.length - 1) : 0.5;

    // Grid-lock scores: fraction of each track's notes that land on a beat (cached)
    let gridLockScores: number[] = [];
    if (opts.gridLock && mb.length >= 2) {
      const cacheKey = mb.join(',');
      if (!gridLockCacheRef.current || gridLockCacheRef.current.key !== cacheKey) {
        const tol = avgBeatDur * 0.15;
        gridLockCacheRef.current = {
          key: cacheKey,
          scores: tracks.map(t => {
            if (!t.notes.length) return 0;
            let hit = 0;
            for (const n of t.notes)
              if (mb.some(b => Math.abs(n.time - b) < tol)) hit++;
            return hit / t.notes.length;
          }),
        };
      }
      gridLockScores = gridLockCacheRef.current.scores;
    }

    const trackColors = midiTrackColorsRef.current;
    const stats       = midiTrackStatsRef.current;
    const solo        = soloTrackIndexRef.current;

    const dispNotes = midiDispNotesRef.current;
    // Visible note index window via binary search on t0 (notes are sorted by
    // time and the display mapping is monotonic). LOOKBACK admits long sustained
    // notes whose onset is left of the viewport but which still overlap it.
    const NOTE_LOOKBACK = 16;
    function noteWindow(disp: Float64Array): [number, number] {
      const n = disp.length >> 1;
      let lo = 0, hi = n;            // first index with t0 > tEnd
      while (lo < hi) { const m = (lo + hi) >> 1; if (disp[2 * m] > tEnd) hi = m; else lo = m + 1; }
      const end = lo;
      const target = tStart - NOTE_LOOKBACK;
      lo = 0; hi = end;             // first index with t0 >= target
      while (lo < hi) { const m = (lo + hi) >> 1; if (disp[2 * m] < target) lo = m + 1; else hi = m; }
      return [lo, end];
    }

    const ctx2 = ctx;
    function drawTrackNotes(ti: number, color: string) {
      const st  = stats[ti];
      const densityFactor = opts.densityWeight && st ? 0.15 + st.density * 0.85 : 1.0;
      const glScore  = gridLockScores[ti] ?? 0;
      const glBoost  = opts.gridLock ? 0.3 + glScore * 1.4 : 1.0;

      const notes = tracks[ti].notes;
      const disp = dispNotes[ti];
      if (!disp) return;
      const [wStart, wEnd] = noteWindow(disp);
      for (let i = wStart; i < wEnd; i++) {
        const t0 = disp[2 * i];
        const t1 = disp[2 * i + 1];
        if (t1 < tStart || t0 > tEnd) continue;
        const note = notes[i];
        const durAudio = t1 - t0;
        const x = t0 * pxPerSec - scrollLeft;
        const w = Math.max(2, (t1 - t0) * pxPerSec - 1);
        const y = (maxP - note.pitch) * noteH;

        // Height: compress long sustained notes
        let h = Math.max(1, noteH - 1);
        if (opts.compressSustain && durAudio > avgBeatDur * 1.5)
          h = Math.max(1, Math.round(h * 0.4));

        // Alpha
        let alpha = opts.velocityAlpha
          ? 0.05 + Math.pow(note.vel, 1.8) * 0.9
          : 0.45 + note.vel * 0.55;
        if (opts.durationAlpha && durAudio > 0.25)
          alpha *= Math.min(1, 0.25 / durAudio);
        alpha *= densityFactor * glBoost;
        alpha  = Math.max(0.03, Math.min(1, alpha));

        ctx2.globalAlpha = alpha;
        ctx2.fillStyle   = color;
        ctx2.fillRect(x, y, w, h);
      }
    }

    // Pass 1 — non-piano tracks (lighter blending)
    ctx.globalCompositeOperation = 'lighter';
    for (let ti = 0; ti < tracks.length; ti++) {
      if (solo !== null && ti !== solo) continue;
      if (!tracks[ti].isPiano)
        drawTrackNotes(ti, trackColors[ti] ?? `hsl(${PALETTE_HUES[ti % PALETTE_HUES.length]},70%,55%)`);
    }
    // Pass 2 — piano on top
    for (let ti = 0; ti < tracks.length; ti++) {
      if (solo !== null && ti !== solo) continue;
      if (tracks[ti].isPiano) drawTrackNotes(ti, '#ffffff');
    }

    // Pass 3 — attack flash (source-over so onsets are always crisp)
    if (opts.attackFlash) {
      ctx.globalCompositeOperation = 'source-over';
      for (let ti = 0; ti < tracks.length; ti++) {
        if (solo !== null && ti !== solo) continue;
        const fc = tracks[ti].isPiano ? '#ffffff' : (trackColors[ti] ?? '#fff');
        const notes = tracks[ti].notes;
        const disp = dispNotes[ti];
        if (!disp) continue;
        const [wStart, wEnd] = noteWindow(disp);
        for (let i = wStart; i < wEnd; i++) {
          const t0 = disp[2 * i];
          if (t0 < tStart || t0 > tEnd) continue;
          const note = notes[i];
          const x = t0 * pxPerSec - scrollLeft;
          ctx.globalAlpha = 0.5 + note.vel * 0.5;
          ctx.fillStyle   = fc;
          ctx.fillRect(x, (maxP - note.pitch) * noteH, 2, Math.max(1, noteH - 1));
        }
      }
    }

    ctx.globalCompositeOperation = 'source-over';
    ctx.globalAlpha = 1;

    // MIDI beat markers
    const selBeat = selectedMidiBeatRef.current;
    const dispBeats = midiDispBeatsRef.current;
    for (let bi = 0; bi < mb.length; bi++) {
      const bt = mb[bi];
      const x = (dispBeats[bi] ?? midiToDisp(bt)) * pxPerSec - scrollLeft;
      if (x < -4 || x > width + 4) continue;
      const isSel = selBeat !== null && Math.abs(bt - selBeat) < 0.001;

      if (isSel) {
        // Glow pass behind the selected marker
        ctx.strokeStyle = '#ff8855';
        ctx.lineWidth = 7;
        ctx.globalAlpha = 0.18;
        ctx.beginPath(); ctx.moveTo(x + 0.5, 0); ctx.lineTo(x + 0.5, height); ctx.stroke();
        ctx.lineWidth = 3;
        ctx.globalAlpha = 0.25;
        ctx.beginPath(); ctx.moveTo(x + 0.5, 0); ctx.lineTo(x + 0.5, height); ctx.stroke();
      }

      ctx.strokeStyle = isSel ? '#ff7755' : '#ffffff';
      ctx.lineWidth   = isSel ? 2 : 1;
      ctx.globalAlpha = isSel ? 1.0 : 0.4;
      ctx.beginPath(); ctx.moveTo(x + 0.5, 0); ctx.lineTo(x + 0.5, height); ctx.stroke();
      // handle triangle at top
      ctx.fillStyle   = ctx.strokeStyle;
      ctx.globalAlpha = isSel ? 1 : 0.6;
      ctx.beginPath();
      ctx.moveTo(x - (isSel ? 6 : 4), 0);
      ctx.lineTo(x + (isSel ? 6 : 4), 0);
      ctx.lineTo(x, isSel ? 9 : 7);
      ctx.fill();
    }
    ctx.globalAlpha = 1;

    // MIDI cursor
    const cursorT = midiToDisp(midiCursorRef.current);
    const cx = cursorT * pxPerSec - scrollLeft;
    if (cx >= 0 && cx <= width) {
      ctx.strokeStyle = selectedTimelineRef.current === 'midi' ? '#44ff88' : '#ffdd44';
      ctx.lineWidth = 2;
      ctx.globalAlpha = 0.9;
      ctx.beginPath(); ctx.moveTo(cx, 0); ctx.lineTo(cx, height); ctx.stroke();
      ctx.globalAlpha = 1;
    }
  }

  function drawPianoKeyboard() {
    const canvas = pianoKeyCanvasRef.current;
    if (!canvas) return;
    const { width, height } = canvas;
    const ctx = canvas.getContext('2d');
    if (!ctx) return;
    ctx.clearRect(0, 0, width, height);
    ctx.fillStyle = '#080810';
    ctx.fillRect(0, 0, width, height);

    const { min: minP, max: maxP } = midiRangeRef.current;
    const noteRange = maxP - minP + 1;
    const noteH = height / noteRange;

    for (let p = minP; p <= maxP; p++) {
      const pc = p % 12;
      const isBlack = BLACK_PCS.has(pc);
      const y = (maxP - p) * noteH;
      ctx.fillStyle = isBlack ? '#1a1a2e' : '#d0d0e0';
      ctx.fillRect(0, y, width - 1, noteH);
      if (pc === 0) {
        const oct = Math.floor(p / 12) - 1;
        ctx.fillStyle = '#606080';
        ctx.font = `${Math.min(9, Math.floor(noteH * 3))}px sans-serif`;
        ctx.textAlign = 'right';
        ctx.fillText(`C${oct}`, width - 3, y + noteH - 1);
      }
    }
    // Right border
    ctx.strokeStyle = '#2a2a3e';
    ctx.lineWidth = 1;
    ctx.beginPath(); ctx.moveTo(width - 0.5, 0); ctx.lineTo(width - 0.5, height); ctx.stroke();
  }

  // ── MIDI playback engine ───────────────────────────────────────────────────
  const MIDI_LOOKAHEAD = 0.4;  // seconds to schedule ahead
  const MIDI_TICK_MS   = 100;  // scheduler interval

  function getMidiCtx(): AudioContext & { masterOut?: AudioNode } {
    if (!midiAudioCtxRef.current) {
      const ctx = new AudioContext() as AudioContext & { masterOut?: AudioNode };
      const comp = ctx.createDynamicsCompressor();
      comp.threshold.value = -18;
      comp.ratio.value = 4;
      // Master volume: notes → compressor → gain → destination.
      const gain = ctx.createGain();
      gain.gain.value = midiVolumeRef.current;
      comp.connect(gain);
      gain.connect(ctx.destination);
      midiGainRef.current = gain;
      ctx.masterOut = comp;
      midiAudioCtxRef.current = ctx;
    }
    if (midiAudioCtxRef.current.state === 'suspended') midiAudioCtxRef.current.resume();
    return midiAudioCtxRef.current;
  }

  function scheduleMidiNote(ctx: AudioContext, out: AudioNode, pitch: number, vel: number, wallStart: number, dur: number) {
    const freq = 440 * Math.pow(2, (pitch - 69) / 12);
    const osc  = ctx.createOscillator();
    const gain = ctx.createGain();
    osc.type = 'triangle';
    osc.frequency.value = freq;
    const amp = vel * 0.10;
    const atk = Math.min(0.012, dur * 0.1);
    const rel = Math.min(0.06, dur * 0.15);
    gain.gain.setValueAtTime(0.0001, wallStart);
    gain.gain.linearRampToValueAtTime(amp, wallStart + atk);
    gain.gain.exponentialRampToValueAtTime(Math.max(0.0001, amp * 0.55), wallStart + Math.min(0.18, dur * 0.45));
    gain.gain.setValueAtTime(Math.max(0.0001, amp * 0.55), wallStart + dur - rel);
    gain.gain.linearRampToValueAtTime(0.0001, wallStart + dur);
    osc.connect(gain);
    gain.connect(out);
    osc.start(wallStart);
    osc.stop(wallStart + dur + 0.02);
    const entry = {
      stop: (now: number) => {
        try {
          gain.gain.cancelScheduledValues(now);
          gain.gain.setValueAtTime(gain.gain.value, now);
          gain.gain.linearRampToValueAtTime(0.0001, now + 0.02);
          osc.stop(now + 0.025);
        } catch { /* already stopped */ }
      },
    };
    midiActiveNodesRef.current.push(entry);
    osc.onended = () => {
      const arr = midiActiveNodesRef.current;
      const i = arr.indexOf(entry);
      if (i !== -1) arr.splice(i, 1);
    };
  }

  function schedulePianoNote(ctx: AudioContext, out: AudioNode, pitch: number, vel: number, wallStart: number, dur: number) {
    const freq = 440 * Math.pow(2, (pitch - 69) / 12);

    // Master gain carries the piano envelope
    const masterGain = ctx.createGain();
    masterGain.connect(out);

    // Brightness filter: wide open at attack, sweeps closed as note decays
    const filter = ctx.createBiquadFilter();
    filter.type = 'lowpass';
    const nyquist = ctx.sampleRate / 2 - 1;
    const brightFreq = Math.min(nyquist, freq * 18);
    const darkFreq   = Math.min(nyquist, Math.max(freq * 2.5, 1200));
    filter.frequency.setValueAtTime(brightFreq, wallStart);
    filter.frequency.exponentialRampToValueAtTime(darkFreq, wallStart + Math.min(0.5, dur * 0.5));
    filter.connect(masterGain);

    // Piano envelope: 3 ms hammer-strike attack → fast initial decay → slow sustain decay
    const amp = vel * 0.055;
    const decayEnd = Math.min(0.15, dur * 0.25);
    const sustainAmp = Math.max(0.0001, amp * 0.38);
    masterGain.gain.setValueAtTime(0.0001, wallStart);
    masterGain.gain.linearRampToValueAtTime(amp, wallStart + 0.003);
    masterGain.gain.exponentialRampToValueAtTime(sustainAmp, wallStart + decayEnd);
    if (dur > decayEnd + 0.01) {
      masterGain.gain.exponentialRampToValueAtTime(Math.max(0.0001, sustainAmp * 0.08), wallStart + dur);
    }

    // Harmonic series: sine partials with amplitude + decay rate falling off with number
    // Higher harmonics also decay faster (piano inharmonicity approximation)
    const partials: [number, number][] = [
      [1, 1.00], [2, 0.50], [3, 0.22], [4, 0.12], [5, 0.06], [6, 0.03],
    ];
    const oscs: OscillatorNode[] = [];
    for (const [n, relAmp] of partials) {
      const osc  = ctx.createOscillator();
      const hGain = ctx.createGain();
      osc.type = 'sine';
      osc.frequency.value = freq * n;
      // Each partial decays proportionally faster as n increases
      const partialDecay = Math.min(0.6, dur * 0.7) / n;
      hGain.gain.setValueAtTime(relAmp, wallStart);
      hGain.gain.exponentialRampToValueAtTime(Math.max(0.0001, relAmp * 0.05), wallStart + partialDecay);
      osc.connect(hGain);
      hGain.connect(filter);
      osc.start(wallStart);
      osc.stop(wallStart + dur + 0.08);
      oscs.push(osc);
    }

    // Brief noise burst for the hammer-strike transient
    const burstLen = Math.ceil(ctx.sampleRate * 0.012);
    const noiseBuf = ctx.createBuffer(1, burstLen, ctx.sampleRate);
    const nd = noiseBuf.getChannelData(0);
    for (let i = 0; i < burstLen; i++) nd[i] = Math.random() * 2 - 1;
    const noise = ctx.createBufferSource();
    noise.buffer = noiseBuf;
    const noiseBP = ctx.createBiquadFilter();
    noiseBP.type = 'bandpass';
    noiseBP.frequency.value = Math.min(nyquist, freq * 5);
    noiseBP.Q.value = 0.8;
    const noiseGain = ctx.createGain();
    noiseGain.gain.setValueAtTime(vel * 0.035, wallStart);
    noiseGain.gain.exponentialRampToValueAtTime(0.0001, wallStart + 0.018);
    noise.connect(noiseBP);
    noiseBP.connect(noiseGain);
    noiseGain.connect(masterGain);
    noise.start(wallStart);
    noise.stop(wallStart + 0.02);

    const entry = {
      stop: (now: number) => {
        try {
          masterGain.gain.cancelScheduledValues(now);
          masterGain.gain.setValueAtTime(masterGain.gain.value, now);
          masterGain.gain.linearRampToValueAtTime(0.0001, now + 0.025);
          oscs.forEach(o => { try { o.stop(now + 0.03); } catch { /* already stopped */ } });
        } catch { /* already stopped */ }
      },
    };
    midiActiveNodesRef.current.push(entry);
    oscs[0].onended = () => {
      const arr = midiActiveNodesRef.current;
      const i = arr.indexOf(entry);
      if (i !== -1) arr.splice(i, 1);
    };
  }

  function runMidiScheduler() {
    const ctx = midiAudioCtxRef.current;
    if (!ctx || !midiPlayingRef.current) return;
    const out = ctx.masterOut!;
    const elapsed      = ctx.currentTime - midiStartWallRef.current;
    const audioNow     = midiStartAudioRef.current + elapsed;
    const scheduleUpto = audioNow + MIDI_LOOKAHEAD;
    const already      = midiScheduledToRef.current;
    if (scheduleUpto <= already) return;
    const solo = soloTrackIndexRef.current;
    const tracks = midiTracksRef.current;
    const dispNotes = midiDispNotesRef.current; // precomputed display t0/t1, sorted by t0
    for (let ti = 0; ti < tracks.length; ti++) {
      if (solo !== null && ti !== solo) continue;
      const disp = dispNotes[ti];
      const notes = tracks[ti].notes;
      if (!disp) continue;
      // Binary-search the first note whose onset is at/after the already-
      // scheduled point, so each tick only touches the small [already,
      // scheduleUpto) window — not every note from the start of the piece.
      const n = disp.length >> 1;
      let lo = 0, hi = n;
      while (lo < hi) { const m = (lo + hi) >> 1; if (disp[2 * m] < already) lo = m + 1; else hi = m; }
      for (let i = lo; i < n; i++) {
        const noteAudioStart = disp[2 * i];
        if (noteAudioStart >= scheduleUpto) break; // sorted by t0 → safe
        const noteAudioEnd = disp[2 * i + 1];
        const wallStart = midiStartWallRef.current + (noteAudioStart - midiStartAudioRef.current);
        if (wallStart < ctx.currentTime - 0.01) continue;
        const note = notes[i];
        const noteDur = Math.max(0.05, noteAudioEnd - noteAudioStart);
        let stop: (now: number) => void;
        if (tracks[ti].isPiano && globalPianoSampler?.isReady) {
          stop = globalPianoSampler.scheduleNote(ctx, out, note.pitch, note.vel, wallStart, noteDur);
        } else if (tracks[ti].isPiano) {
          schedulePianoNote(ctx, out, note.pitch, note.vel, wallStart, noteDur);
          continue; // schedulePianoNote pushes its own entry
        } else {
          scheduleMidiNote(ctx, out, note.pitch, note.vel, wallStart, noteDur);
          continue; // scheduleMidiNote pushes its own entry
        }
        const entry = { stop };
        midiActiveNodesRef.current.push(entry);
      }
    }
    midiScheduledToRef.current = scheduleUpto;
  }

  const midiDispFrameRef = useRef(0);
  function midiCursorLoop() {
    const ctx = midiAudioCtxRef.current;
    if (!ctx || !midiPlayingRef.current) return;
    const elapsed  = ctx.currentTime - midiStartWallRef.current;
    const audioNow = midiStartAudioRef.current + elapsed; // display (stretched) time
    // Keep midiCursorRef in MIDI time so beat-tapping still works
    midiCursorRef.current = dispToMidi(audioNow);
    drawPianoRoll();
    midiDispFrameRef.current++;
    if (midiDispFrameRef.current % 6 === 0) setMidiCursorDisp(midiCursorRef.current);
    if (audioNow < midiToDisp(midiDurationRef.current) + 1) {
      midiCursorRafRef.current = requestAnimationFrame(midiCursorLoop);
    } else {
      midiPause();
    }
  }

  async function midiPlay(from?: number) {
    const ctx = getMidiCtx();

    // Decode pre-fetched samples if piano is audible and samples aren't decoded yet.
    // Fetch already started on MIDI load; this only runs the fast decode step.
    const solo = soloTrackIndexRef.current;
    const pianoVisible = midiTracksRef.current.some((t, i) =>
      t.isPiano && (solo === null || solo === i)
    );
    if (pianoVisible && globalPianoSampler && !globalPianoSampler.isReady) {
      try {
        await globalPianoSampler.fetch(); // no-op if already fetched; waits if still in progress
        await globalPianoSampler.decode(ctx);
        setSamplerStatus('ready');
      } catch {
        setSamplerStatus('error');
      }
    }

    const pos      = from ?? midiCursorRef.current;
    const audioPos = midiToDisp(pos); // display (stretched) time
    midiStartPosRef.current    = pos;
    midiStartAudioRef.current  = audioPos;
    midiStartWallRef.current   = ctx.currentTime;
    midiScheduledToRef.current = audioPos;
    midiPlayingRef.current     = true;
    setMidiPlaying(true);
    if (midiSchedulerRef.current) clearInterval(midiSchedulerRef.current);
    midiSchedulerRef.current = setInterval(runMidiScheduler, MIDI_TICK_MS);
    runMidiScheduler();
    if (midiCursorRafRef.current) cancelAnimationFrame(midiCursorRafRef.current);
    midiCursorRafRef.current = requestAnimationFrame(midiCursorLoop);
  }

  function midiPause() {
    const ctx = midiAudioCtxRef.current;
    if (ctx && midiPlayingRef.current) {
      const elapsed = ctx.currentTime - midiStartWallRef.current;
      midiCursorRef.current = dispToMidi(midiStartAudioRef.current + elapsed);
    }
    // Kill all scheduled notes immediately with a short fade to avoid clicks
    if (ctx) {
      const now = ctx.currentTime;
      for (const entry of midiActiveNodesRef.current) entry.stop(now);
    }
    midiActiveNodesRef.current = [];
    midiPlayingRef.current = false;
    setMidiPlaying(false);
    setMidiCursorDisp(midiCursorRef.current);
    if (midiSchedulerRef.current) { clearInterval(midiSchedulerRef.current); midiSchedulerRef.current = null; }
    if (midiCursorRafRef.current) { cancelAnimationFrame(midiCursorRafRef.current); midiCursorRafRef.current = null; }
    drawPianoRoll();
  }

  function midiTogglePlay() {
    if (midiPlayingRef.current) midiPause(); else midiPlay();
  }

  function midiSeekTo(midiT: number) {
    const wasPlaying = midiPlayingRef.current;
    if (wasPlaying) midiPause();
    midiCursorRef.current = Math.max(0, midiT);
    drawPianoRoll();
    if (wasPlaying) midiPlay(midiCursorRef.current);
  }

  function midiTapBeat() {
    const t = midiCursorRef.current;
    if (t < 0) return;
    const next = [...midiBeatsRef.current, t].sort((a, b) => a - b);
    midiBeatsRef.current = next;
    onMidiBeatsChangeRef.current(next);
    drawPianoRoll();
  }

  // Piecewise-linear warp: MIDI time → audio time
  function warpMidiTime(mt: number): number {
    const mb = midiBeatsRef.current;
    const ab = beatsRef.current;
    const n  = Math.min(mb.length, ab.length);
    if (n === 0) return mt;
    if (n === 1) return ab[0] + (mt - mb[0]);
    if (mt <= mb[0]) {
      const ratio = (ab[1] - ab[0]) / (mb[1] - mb[0]);
      return ab[0] + (mt - mb[0]) * ratio;
    }
    if (mt >= mb[n - 1]) {
      const ratio = (ab[n - 1] - ab[n - 2]) / (mb[n - 1] - mb[n - 2]);
      return ab[n - 1] + (mt - mb[n - 1]) * ratio;
    }
    for (let i = 0; i < n - 1; i++) {
      if (mt >= mb[i] && mt < mb[i + 1]) {
        const t = (mt - mb[i]) / (mb[i + 1] - mb[i]);
        return ab[i] + t * (ab[i + 1] - ab[i]);
      }
    }
    return mt;
  }

  // Piecewise-linear inverse: audio time → MIDI time
  function audioTimeToMidiTime(at: number): number {
    const mb = midiBeatsRef.current;
    const ab = beatsRef.current;
    const n  = Math.min(mb.length, ab.length);
    if (n === 0) return at;
    if (n === 1) return mb[0] + (at - ab[0]);
    if (at <= ab[0]) {
      const ratio = (mb[1] - mb[0]) / (ab[1] - ab[0]);
      return mb[0] + (at - ab[0]) * ratio;
    }
    if (at >= ab[n - 1]) {
      const ratio = (mb[n - 1] - mb[n - 2]) / (ab[n - 1] - ab[n - 2]);
      return mb[n - 1] + (at - ab[n - 1]) * ratio;
    }
    for (let i = 0; i < n - 1; i++) {
      if (at >= ab[i] && at < ab[i + 1]) {
        const t = (at - ab[i]) / (ab[i + 1] - ab[i]);
        return mb[i] + t * (mb[i + 1] - mb[i]);
      }
    }
    return at;
  }

  // ── Drag-to-pan ───────────────────────────────────────────────────────────
  useEffect(() => {
    const el = containerRef.current!;
    if (!el) return;

    let startX = 0;
    let startScrollLeft = 0;
    let panning = false;
    let started = false; // only activate for pointers that began on this container

    function getScrollEl() {
      const ws = wsRef.current;
      return ws ? (ws.getWrapper().parentElement as HTMLElement) : null;
    }

    function onPointerDown(e: PointerEvent) {
      if (e.button !== 0) return;
      started = true;
      startX = e.clientX;
      startScrollLeft = getScrollEl()?.scrollLeft ?? 0;
      panning = false;
    }

    function onPointerMove(e: PointerEvent) {
      if (!started || !(e.buttons & 1)) return;
      const dx = e.clientX - startX;
      if (!panning && Math.abs(dx) > 4) {
        panning = true;
        regionJustClickedRef.current = true;
        wsRef.current?.setOptions({ interact: false });
        el.style.cursor = 'grabbing';
      }
      if (panning) {
        const scrollEl = getScrollEl();
        if (scrollEl) scrollEl.scrollLeft = startScrollLeft - dx;
      }
    }

    function onPointerUp() {
      if (panning) {
        el.style.cursor = '';
        setTimeout(() => {
          wsRef.current?.setOptions({ interact: true });
          regionJustClickedRef.current = false;
        }, 50);
      }
      panning = false;
      started = false;
    }

    el.addEventListener('pointerdown', onPointerDown);
    window.addEventListener('pointermove', onPointerMove);
    window.addEventListener('pointerup', onPointerUp);
    return () => {
      el.removeEventListener('pointerdown', onPointerDown);
      window.removeEventListener('pointermove', onPointerMove);
      window.removeEventListener('pointerup', onPointerUp);
    };
  }, []);

  // ── Wheel-to-zoom (separate effect so HMR re-attaches it without restart) ──
  useEffect(() => {
    const el = containerRef.current;
    if (!el) return;
    function onWheel(e: WheelEvent) {
      if (Math.abs(e.deltaX) * 3 > Math.abs(e.deltaY)) return; // not strongly vertical → pan
      e.preventDefault();
      const fit = fitPxPerSecRef.current;
      if (fit === 0) return;
      const factor = e.deltaY < 0 ? 1 / ZOOM_STEP : ZOOM_STEP;
      const next = Math.max(fit, Math.min(zoomPxPerSecRef.current * factor, fit * ZOOM_MAX_MULTIPLIER));
      zoomPxPerSecRef.current = next;
      setZoomPxPerSec(next);
      // rAF-throttle: re-render at most once per frame instead of waiting for idle debounce
      if (zoomRafRef.current === null) {
        zoomRafRef.current = requestAnimationFrame(() => {
          zoomRafRef.current = null;
          const z = zoomPxPerSecRef.current;
          wsRef.current?.zoom(z);
          bassWsRef.current?.zoom(z);
          drawSpectrogram();
          drawPianoRoll();
        });
      }
    }
    el.addEventListener("wheel", onWheel, { passive: false });
    return () => el.removeEventListener("wheel", onWheel);
  }, []);

  // ── Load audio via Rust engine ─────────────────────────────────────────────
  useEffect(() => {
    if (!mp3Path) return;
    let cancelled = false;
    setReady(false);
    setLoadProgress(0);
    setLoadError(null);
    setPlaying(false);
    playingRef.current = false;
    setPcmReady(false);
    specRawRef.current = null;
    specSalRef.current = null;
    scrollLeftRef.current = 0;

    (async () => {
      try {
        setLoadProgress(5);
        const result = await invoke<LoadResult>("load_audio", { path: mp3Path });
        if (cancelled) return;
        setLoadProgress(80);
        // result.duration is the ORIGINAL recording duration; the display axis
        // is stretched time. With no stretches these are equal.
        origDurationRef.current = result.duration;
        segmentsRef.current = buildSegments(result.duration, stretchesRef.current);
        const dispDur = stretchedDuration(result.duration, stretchesRef.current);
        setDuration(dispDur);
        durationRef.current = dispDur;

        const ws = wsRef.current;
        if (!ws) return;

        // If stretches exist, the [stretches] effect reloads the waveform with
        // the warped peaks once PCM is ready; here we just show the original
        // peaks (correct shape when there are no stretches).

        // Decode both spectrogram layers (base64 → Uint8Array). Bytes are
        // linear-dB u8 (−90..0 dB); the live dials reconstruct dB and remap.
        const decodeB64 = (s: string) => {
          const raw = atob(s);
          const out = new Uint8Array(raw.length);
          for (let i = 0; i < raw.length; i++) out[i] = raw.charCodeAt(i);
          return out;
        };
        specRawRef.current = decodeB64(result.spec_raw.data);
        specRawBinsRef.current = result.spec_raw.bins;
        specSalRef.current = decodeB64(result.spec_salience.data);
        specSalBinsRef.current = result.spec_salience.bins;
        specColsRef.current = result.spec_cols;
        specColsPerSecRef.current = result.spec_cols_per_sec;
        specMidiLoRef.current = result.spec_midi_lo;
        specMidiHiRef.current = result.spec_midi_hi;

        const channelData = result.peaks.map(ch => new Float32Array(ch));
        await ws.load("", channelData, dispDur);

        if (bassWsRef.current && result.bass_peaks?.length) {
          const bassData = result.bass_peaks.map(ch => new Float32Array(ch));
          bassWsRef.current.load("", bassData, dispDur).catch(() => {});
        }
      } catch (e) {
        if (!cancelled) setLoadError(String(e));
      }
    })();

    return () => { cancelled = true; };
  }, [mp3Path]);

  // ── Listen to Rust decode-progress and pcm-ready events ───────────────────
  useEffect(() => {
    const u1 = listen<number>("load-progress", (ev) => setLoadProgress(ev.payload));
    const u2 = listen("pcm-ready", () => { setPcmReady(true); setLoadPhase(""); });
    const u3 = listen<number>("export-progress", (ev) => {
      const pct = ev.payload;
      setExportProgress(pct);
      if (pct >= 100) setTimeout(() => setExportProgress(null), 600);
    });
    const u4 = listen<string>("load-phase", (ev) => setLoadPhase(ev.payload));
    const u5 = listen<{ pct: number; stage: string }>("video-export-progress", (ev) => {
      setVideoProgress(ev.payload);
      if (ev.payload.pct >= 100) setTimeout(() => setVideoProgress(null), 600);
    });
    return () => {
      u1.then(fn => fn()); u2.then(fn => fn()); u3.then(fn => fn());
      u4.then(fn => fn()); u5.then(fn => fn());
    };
  }, []);

  // ── Beat tick sound ───────────────────────────────────────────────────────
  function playBeatTick() {
    if (!audioCtxRef.current) audioCtxRef.current = new AudioContext();
    const ctx = audioCtxRef.current;
    if (ctx.state === "suspended") ctx.resume();
    const now = ctx.currentTime;
    const osc = ctx.createOscillator();
    const gain = ctx.createGain();
    osc.connect(gain);
    gain.connect(ctx.destination);
    osc.type = "sine";
    osc.frequency.setValueAtTime(1200, now);
    osc.frequency.exponentialRampToValueAtTime(400, now + 0.03);
    gain.gain.setValueAtTime(0.35, now);
    gain.gain.exponentialRampToValueAtTime(0.001, now + 0.06);
    osc.start(now);
    osc.stop(now + 0.07);
  }

  // ── Listen to Rust position events ────────────────────────────────────────
  useEffect(() => {
    const unlisten = listen<PositionEvent>("audio-position", (ev) => {
      // Engine reports position in ORIGINAL time. Beat-tick detection compares
      // against beats (also original); the cursor displays in stretched time.
      const { t, playing: p } = ev.payload;

      // Beat tick: detect when playback crosses a beat timestamp.
      // Skip if position jumped > 0.1 s (seek) to avoid spurious ticks.
      const last = lastAudioPosRef.current;
      if (p && playBeatsRef.current && last >= 0 && t - last < 0.1 && t > last) {
        const crossed = beatsRef.current.filter(bt => bt > last && bt <= t);
        if (crossed.length > 0) playBeatTick();
      }
      lastAudioPosRef.current = t;

      const dispT = o2s(t);
      currentTimeRef.current = dispT;

      // The cursor itself is moved directly on WaveSurfer below (smooth, no
      // React). The time-readout React state is throttled to ~12 Hz so we don't
      // re-render this large component on every position event.
      const now = performance.now();
      const playChanged = p !== playingRef.current;
      if (playChanged || now - lastTimeUiRef.current > 80) {
        lastTimeUiRef.current = now;
        setCurrentTime(dispT);
        setPlaying(p);
      }
      playingRef.current = p;

      // Move the cursor (WaveSurfer.seekTo → its auto-center scrollIntoView,
      // which forces a layout reflow + waveform re-render). The engine emits at
      // ~60 Hz; a 30 Hz cursor looks smooth and halves that reflow cost. Always
      // apply the final frame on pause so the cursor lands exactly.
      const ws = wsRef.current;
      const dur = durationRef.current;
      if (ws && dur > 0 && (playChanged || now - lastSeekRef.current > 32)) {
        lastSeekRef.current = now;
        const pos = Math.max(0, Math.min(dispT / dur, 1));
        ws.seekTo(pos);
        bassWsRef.current?.seekTo(pos);
      }
    });
    return () => { unlisten.then(fn => fn()); };
  }, []);

  // ── Stretches → Rust engine + rescaled waveform ──────────────────────────
  // The engine applies the stretches and returns the warped peaks + duration;
  // we reload WaveSurfer with them so the timeline visibly rescales. The cursor
  // is kept at the same musical moment (cursor_orig → stretched).
  useEffect(() => {
    if (!ready) return;
    let cancelled = false;
    // Fresh load with no stretches: the engine's warped buffer is already the
    // original and the waveform already shows it — nothing to do (avoids a
    // needless PCM wait + recompute on every open).
    if (stretches.length === 0 &&
        Math.abs(durationRef.current - origDurationRef.current) < 1e-6) {
      return;
    }
    (async () => {
      try {
        const res = await invoke<{ peaks: number[][]; bass_peaks: number[][]; warped_duration: number; cursor_orig: number }>(
          "set_stretches_audio", { stretches }
        );
        if (cancelled) return;
        const ws = wsRef.current;
        setDuration(res.warped_duration);
        durationRef.current = res.warped_duration;
        if (ws && res.peaks?.length) {
          const channelData = res.peaks.map(ch => new Float32Array(ch));
          await ws.load("", channelData, res.warped_duration);
        }
        if (bassWsRef.current && res.bass_peaks?.length) {
          const bassData = res.bass_peaks.map(ch => new Float32Array(ch));
          await bassWsRef.current.load("", bassData, res.warped_duration).catch(() => {});
        }
        // Reposition cursor to the same musical point on the new timeline.
        const dispT = o2s(res.cursor_orig);
        currentTimeRef.current = dispT;
        setCurrentTime(dispT);
        if (ws && res.warped_duration > 0) {
          const pos = Math.max(0, Math.min(dispT / res.warped_duration, 1));
          ws.seekTo(pos);
          bassWsRef.current?.seekTo(pos);
        }
        drawSpectrogram();
        drawPianoRoll();
      } catch (e) {
        console.error("set_stretches_audio:", e);
      }
    })();
    return () => { cancelled = true; };
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [stretches, ready]);

  // ── Keyboard handlers ──────────────────────────────────────────────────────
  useEffect(() => {
    function onKeyDown(e: KeyboardEvent) {
      const tag = (e.target as HTMLElement).tagName;
      const isInput = tag === "INPUT" || tag === "TEXTAREA";

      if (e.key === "Shift") { shiftHeldRef.current = true; return; }
      if (e.key === "Meta" || e.key === "Control") { cmdHeldRef.current = true; return; }
      if (isInput) return;

      if (e.code === "Space") {
        e.preventDefault();
        if (selectedTimelineRef.current === 'midi') {
          midiTogglePlay();
        } else {
          togglePlayPause();
        }
        return;
      }

      if (e.code === "KeyT") {
        e.preventDefault();
        midiTapBeat();
        return;
      }

      if (e.code === "Escape") {
        selectionRef.current = null;
        setSelection(null);
        return;
      }

      if (e.code === "KeyS") {
        e.preventDefault();
        const sel = selectionRef.current;
        if (sel) {
          // Selection is in display (stretched) time; stretches store original time.
          const so = s2o(sel.start), eo = s2o(sel.end);
          const overlaps = stretchesRef.current.some(s => eo > s.start && so < s.end);
          if (!overlaps) setStretchModal({ start: so, end: eo });
        } else {
          const t = s2o(currentTimeRef.current);
          const active = stretchesRef.current.find(s => t >= s.start && t <= s.end);
          if (active) setStretchModal({ start: active.start, end: active.end, existingFactor: active.factor });
        }
        return;
      }

      if (e.code === "KeyR") {
        e.preventDefault();
        if (recordingRef.current) stopRecording();
        else startRecording();
        return;
      }

      if (e.code === "KeyB" && recordingRef.current) {
        e.preventDefault();
        pendingTapsRef.current.push(s2o(currentTimeRef.current)); // store beats in original time
        return;
      }

      if (e.code === "ArrowLeft" || e.code === "ArrowRight") {
        const sel = selectionRef.current;
        const selBeat = selectedBeatTimeRef.current;
        if (!sel && selBeat === null) return;
        e.preventDefault();
        const nudge = (e.shiftKey ? 0.1 : 0.01) * (e.code === "ArrowRight" ? 1 : -1);

        if (sel) {
          const updated = beatsRef.current
            .map(b => (b >= sel.start && b <= sel.end) ? Math.max(0, b + nudge) : b)
            .sort((a, b) => a - b);
          onBeatsChangeRef.current(updated);
          const newSel = { start: sel.start + nudge, end: sel.end + nudge };
          selectionRef.current = newSel;
          setSelection(newSel);
        } else if (selBeat !== null) {
          const newTime = Math.max(0, selBeat + nudge);
          const updated = beatsRef.current
            .map(b => Math.abs(b - selBeat) < 0.001 ? newTime : b)
            .sort((a, b) => a - b);
          selectedBeatTimeRef.current = newTime;
          setSelectedBeatTime(newTime);
          onBeatsChangeRef.current(updated);
        }
        return;
      }

      if (e.code === "BracketLeft" || e.code === "BracketRight") {
        const selBeat = selectedBeatTimeRef.current;
        if (selBeat === null) return;
        e.preventDefault();

        const ab = beatsRef.current.slice();
        const idx = ab.findIndex(b => Math.abs(b - selBeat) < 0.001);
        if (idx < 0) return;

        const MIN_GAP = 60 / 400; // 0.15 s — maximum 400 BPM spacing used during cascade

        if (e.code === "BracketLeft") {
          if (idx < 1 || idx >= ab.length - 1) return; // need a beat on each side
          const interval = ab[idx + 1] - ab[idx];
          ab[idx - 1] = Math.max(0, ab[idx] - interval);
          // Cascade: push any beat that is now ≤ its left neighbour
          for (let j = idx - 1; j >= 1; j--) {
            if (ab[j] <= ab[j - 1]) ab[j - 1] = Math.max(0, ab[j] - MIN_GAP);
            else break;
          }
          const newSel = ab[idx - 1];
          selectedBeatTimeRef.current = newSel;
          setSelectedBeatTime(newSel);
          selectedMidiBeatRef.current = null;
          setSelectedMidiBeat(null);
          onBeatsChangeRef.current(ab);
          scrollToBeat(newSel);

        } else {
          if (idx >= ab.length - 2) return; // need beats at i+1 and i+2
          const interval = ab[idx + 1] - ab[idx];
          ab[idx + 2] = ab[idx + 1] + interval;
          // Cascade: push any beat that is now ≥ its right neighbour
          for (let j = idx + 2; j < ab.length - 1; j++) {
            if (ab[j] >= ab[j + 1]) ab[j + 1] = Math.min(durationRef.current, ab[j] + MIN_GAP);
            else break;
          }
          const newSel = ab[idx + 1];
          selectedBeatTimeRef.current = newSel;
          setSelectedBeatTime(newSel);
          selectedMidiBeatRef.current = null;
          setSelectedMidiBeat(null);
          onBeatsChangeRef.current(ab);
          scrollToBeat(newSel);
        }
        return;
      }

      if (e.code === "Delete" || e.code === "Backspace") {
        if (selectedMidiBeatRef.current !== null) {
          e.preventDefault();
          const sel = selectedMidiBeatRef.current;
          const next = midiBeatsRef.current.filter(t => Math.abs(t - sel) > 0.001);
          midiBeatsRef.current = next;
          selectedMidiBeatRef.current = null;
          onMidiBeatsChangeRef.current(next);
          setSelectedMidiBeat(null);
          drawPianoRoll();
          return;
        }
        if (selectedBeatTimeRef.current !== null) {
          e.preventDefault();
          const sel = selectedBeatTimeRef.current;
          const updated = beatsRef.current.filter(t => Math.abs(t - sel) > 0.001);
          selectedBeatTimeRef.current = null;
          setSelectedBeatTime(null);
          onBeatsChangeRef.current(updated);
          return;
        }
      }
    }

    function onKeyUp(e: KeyboardEvent) {
      if (e.key === "Shift") shiftHeldRef.current = false;
      if (e.key === "Meta" || e.key === "Control") cmdHeldRef.current = false;
    }

    function onBlur() {
      shiftHeldRef.current = false;
      cmdHeldRef.current = false;
    }

    window.addEventListener("keydown", onKeyDown);
    window.addEventListener("keyup", onKeyUp);
    window.addEventListener("blur", onBlur);
    return () => {
      window.removeEventListener("keydown", onKeyDown);
      window.removeEventListener("keyup", onKeyUp);
      window.removeEventListener("blur", onBlur);
    };
  }, []);

  // ── Scroll to keep a beat time within the middle 50% of the viewport ──────
  function scrollToBeat(beatTime: number) {
    const ws = wsRef.current;
    if (!ws) return;
    const scrollEl = ws.getWrapper().parentElement as HTMLElement | null;
    if (!scrollEl) return;
    const containerWidth = containerRef.current?.clientWidth ?? scrollEl.clientWidth;
    const beatPx = o2s(beatTime) * zoomPxPerSecRef.current; // beat stored in original time
    const viewportX = beatPx - scrollLeftRef.current;
    let newScrollLeft: number | null = null;
    if (viewportX < 0.25 * containerWidth)
      newScrollLeft = beatPx - 0.25 * containerWidth;
    else if (viewportX > 0.75 * containerWidth)
      newScrollLeft = beatPx - 0.75 * containerWidth;
    if (newScrollLeft !== null)
      scrollEl.scrollLeft = Math.max(0, newScrollLeft);
  }

  // ── Seek helper ───────────────────────────────────────────────────────────
  // `t` is in display (stretched) time. WaveSurfer cursor uses it directly; the
  // engine expects original time, so convert at the boundary.
  function handleSeek(t: number) {
    const ws = wsRef.current;
    const dur = durationRef.current;
    if (ws && dur > 0) {
      ws.seekTo(Math.max(0, Math.min(t / dur, 1)));
    }
    invoke("seek_audio", { t: s2o(t) }).catch(console.error);
  }

  // ── Playback helpers ───────────────────────────────────────────────────────
  function togglePlayPause() {
    if (playingRef.current) {
      invoke("pause_audio").catch(console.error);
    } else {
      // Seek before play to avoid a race where a recent click-to-seek hasn't
      // reached Rust yet and play_audio would start from the wrong position.
      invoke("seek_audio", { t: s2o(currentTimeRef.current) })
        .then(() => invoke("play_audio"))
        .catch(console.error);
    }
  }

  function togglePlayBoth() {
    if (playingRef.current || midiPlayingRef.current) {
      invoke("pause_audio").catch(console.error);
      midiPause();
    } else {
      invoke("seek_audio", { t: s2o(currentTimeRef.current) })
        .then(() => invoke("play_audio"))
        .catch(console.error);
      midiPlay();
    }
  }

  // ── Recording helpers ──────────────────────────────────────────────────────
  function startRecording() {
    recordingStartTimeRef.current = s2o(currentTimeRef.current); // original time
    pendingTapsRef.current = [];
    recordingRef.current = true;
    setRecording(true);
    if (!playingRef.current) invoke("play_audio").catch(console.error);
  }

  function stopRecording() {
    recordingRef.current = false;
    setRecording(false);
    const startTime = recordingStartTimeRef.current;     // original time
    const endTime = s2o(currentTimeRef.current);          // original time
    const taps = pendingTapsRef.current;
    pendingTapsRef.current = [];
    const kept = beatsRef.current.filter(t => t < startTime || t > endTime);
    const merged = [...kept, ...taps].sort((a, b) => a - b);
    onBeatsChangeRef.current(merged);
    if (playingRef.current) invoke("pause_audio").catch(console.error);
  }

  // ── Bake (Cmd+R) ──────────────────────────────────────────────────────────
  // Bake is purely a high-quality file export. The engine already plays the
  // ── Export to MP3 ─────────────────────────────────────────────────────────
  async function handleExport() {
    if (exportProgress !== null) return;
    setExportError(null);
    const outPath = await saveDialog({
      title: "Export as MP3",
      filters: [{ name: "MP3 Audio", extensions: ["mp3"] }],
      defaultPath: mp3Path.replace(/\.mp3$/i, "") + "_stretched.mp3",
    });
    if (!outPath) return;

    setExportProgress(0);
    try {
      await invoke("export_mp3", { mp3Path, stretches, outputPath: outPath });
    } catch (e) {
      setExportError(String(e));
      setExportProgress(null);
    }
  }

  // ── Export to Video ───────────────────────────────────────────────────────
  // Notes are mapped MIDI time → original audio time (beat-pair warp) →
  // stretched/output time, so the video lines up with the exported audio.
  async function handleVideoExport(opts: VideoExportOpts) {
    setVideoModalOpen(false);
    if (videoProgress !== null) return;
    setVideoError(null);

    const outPath = await saveDialog({
      title: "Export Video",
      filters: [{ name: "MP4 Video", extensions: ["mp4"] }],
      defaultPath: mp3Path.replace(/\.mp3$/i, "") + "_pianoroll.mp4",
    });
    if (!outPath) return;

    const tracks = midiTracksRef.current;
    const colors = midiTrackColorsRef.current;
    const sts = stretchesRef.current;
    const hasWarp = midiBeatsRef.current.length > 0 && beatsRef.current.length > 0;

    const vTracks = tracks.map((t, i) => ({
      color: cssColorToRgb(t.isPiano ? '#ffffff' : (colors[i] ?? '#ffffff')),
      is_piano: t.isPiano,
    }));
    const vNotes: { start: number; dur: number; pitch: number; vel: number; track: number }[] = [];
    tracks.forEach((t, ti) => {
      for (const n of t.notes) {
        const o0 = hasWarp ? warpMidiTime(n.time) : n.time;
        const o1 = hasWarp ? warpMidiTime(n.time + n.dur) : n.time + n.dur;
        const s0 = originalToStretched(o0, sts);
        const s1 = originalToStretched(o1, sts);
        vNotes.push({ start: s0, dur: Math.max(0.05, s1 - s0), pitch: n.pitch, vel: n.vel, track: ti });
      }
    });
    const vBeats = beatsRef.current.map(b => originalToStretched(b, sts));

    setVideoProgress({ pct: 0, stage: "Starting" });
    try {
      await invoke("export_video", {
        mp3Path,
        stretches: sts,
        outputPath: outPath,
        tracks: vTracks,
        notes: vNotes,
        beats: vBeats,
        options: {
          orientation: opts.orientation,
          start: opts.start,
          end: opts.end,
          fps: 30,
          width: 1920,
          height: 1080,
          beat_pulse: opts.beatPulse,
          orchestra_bars: opts.orchestraBars,
          tempo_pendulum: opts.tempoPendulum,
        },
      });
    } catch (e) {
      setVideoError(String(e));
      setVideoProgress(null);
    }
  }

  // ── Stretch modal confirm / remove ────────────────────────────────────────
  function handleStretchConfirm(factor: number) {
    if (!stretchModal) return;
    const { start, end } = stretchModal;
    const kept = stretchesRef.current.filter(s => s.end <= start || s.start >= end);
    const updated: Stretch[] = [...kept, { start, end, factor }]
      .sort((a, b) => a.start - b.start);
    onStretchesChangeRef.current(updated);
    setStretchModal(null);
    setSelection(null);
    selectionRef.current = null;
  }

  function handleStretchRemove() {
    if (!stretchModal) return;
    const { start, end } = stretchModal;
    const updated = stretchesRef.current.filter(
      s => Math.abs(s.start - start) > 0.001 || Math.abs(s.end - end) > 0.001
    );
    onStretchesChangeRef.current(updated);
    setStretchModal(null);
  }

  // ── Beat markers in beats strip ───────────────────────────────────────────
  useEffect(() => {
    const inner = beatsStripInnerRef.current;
    if (!inner || !ready) return;

    inner.innerHTML = '';
    beatLabelElsRef.current.clear();
    inner.style.width = `${Math.max(durationRef.current * zoomPxPerSecRef.current, 1)}px`;

    beats.forEach((t, i) => {
      const bpm = i < beats.length - 1 ? 60 / (beats[i + 1] - t) : null;
      const isSelected = selectedBeatTime !== null && Math.abs(t - selectedBeatTime) < 0.001;

      const markerEl = document.createElement('div');
      markerEl.dataset.beatTime = String(t); // stored in original time
      Object.assign(markerEl.style, {
        position: 'absolute', top: '0', left: `${o2s(t) * zoomPxPerSecRef.current}px`,
        width: '8px', height: '100%', cursor: 'grab',
        transform: 'translateX(-4px)',
      });

      const normalColor = isSelected ? 'rgba(255,100,50,1)' : 'rgba(239,68,68,0.9)';
      const tickEl = document.createElement('div');
      Object.assign(tickEl.style, {
        position: 'absolute', top: '0', left: '3px',
        width: isSelected ? '3px' : '2px',
        height: isSelected ? '100%' : '8px',
        borderRadius: '1px',
        background: normalColor, pointerEvents: 'none',
        ...(isSelected && {
          boxShadow: '0 0 6px 2px rgba(255,100,50,0.7), 0 0 2px 1px rgba(255,180,100,0.5)',
        }),
      });
      markerEl.appendChild(tickEl);

      const labelEl = document.createElement('div');
      Object.assign(labelEl.style, {
        position: 'absolute', top: '10px', left: '4px',
        fontSize: '9px', fontFamily: '"SF Mono","Fira Code",monospace',
        color: 'rgba(251,191,36,0.9)', whiteSpace: 'nowrap',
        pointerEvents: 'none', lineHeight: '1',
      });
      if (bpm !== null) {
        labelEl.textContent = String(Math.round(bpm));
        beatLabelElsRef.current.set(t, labelEl);
      }
      markerEl.appendChild(labelEl);

      markerEl.addEventListener('pointerover', () => { tickEl.style.background = 'rgba(255,140,80,1)'; });
      markerEl.addEventListener('pointerout',  () => { tickEl.style.background = normalColor; });

      markerEl.addEventListener('pointerdown', (e: PointerEvent) => {
        if (e.button !== 0) return;
        e.preventDefault();
        e.stopPropagation();
        regionJustClickedRef.current = true;

        const startClientX = e.clientX;
        const startPixel = o2s(t) * zoomPxPerSecRef.current; // on-screen (stretched) pixel
        let dragging = false;
        let newTime = t; // beat time STORED in original

        function onMove(ev: PointerEvent) {
          const dx = ev.clientX - startClientX;
          if (!dragging && Math.abs(dx) > 3) {
            dragging = true;
            markerEl.style.cursor = 'grabbing';
          }
          if (dragging) {
            const sT = Math.max(0, Math.min((startPixel + dx) / zoomPxPerSecRef.current, durationRef.current));
            newTime = s2o(sT); // convert display position back to original time
            markerEl.style.left = `${sT * zoomPxPerSecRef.current}px`;

            // Update BPM labels in real-time
            const ab = beatsRef.current;
            const idx = ab.findIndex(b => Math.abs(b - t) < 0.001);

            // This beat's label — BPM to the next beat, moved lower to clear the cursor
            if (bpm !== null && idx >= 0 && idx < ab.length - 1) {
              const interval = ab[idx + 1] - newTime;
              if (interval > 0.001) labelEl.textContent = String(Math.round(60 / interval));
              labelEl.style.top = '16px';
            }

            // Previous beat's label — BPM from prev beat to here
            if (idx > 0) {
              const prevLabel = beatLabelElsRef.current.get(ab[idx - 1]);
              if (prevLabel) {
                const interval = newTime - ab[idx - 1];
                if (interval > 0.001) prevLabel.textContent = String(Math.round(60 / interval));
              }
            }
          }
        }

        function onUp() {
          window.removeEventListener('pointermove', onMove);
          window.removeEventListener('pointerup', onUp);
          markerEl.style.cursor = 'grab';
          if (bpm !== null) labelEl.style.top = '10px'; // restore label position
          setTimeout(() => { regionJustClickedRef.current = false; }, 50);
          if (dragging) {
            const updated = beatsRef.current
              .map(bt => Math.abs(bt - t) < 0.001 ? newTime : bt)
              .sort((a, b) => a - b);
            selectedBeatTimeRef.current = newTime;
            selectedMidiBeatRef.current = null;
            setSelectedMidiBeat(null);
            onBeatsChangeRef.current(updated);
          } else {
            selectedBeatTimeRef.current = t;
            setSelectedBeatTime(t);
            selectedMidiBeatRef.current = null;
            setSelectedMidiBeat(null);
          }
        }

        window.addEventListener('pointermove', onMove);
        window.addEventListener('pointerup', onUp);
      });

      inner.appendChild(markerEl);
    });
  }, [beats, ready, selectedBeatTime]);

  // ── Beat marker positions — update on zoom without recreating elements ─────
  useEffect(() => {
    const inner = beatsStripInnerRef.current;
    if (!inner || !ready) return;
    inner.style.width = `${Math.max(durationRef.current * zoomPxPerSec, 1)}px`;
    inner.querySelectorAll<HTMLElement>('[data-beat-time]').forEach(el => {
      const t = parseFloat(el.dataset.beatTime!);
      el.style.left = `${o2s(t) * zoomPxPerSec}px`;
    });
  // Reposition on stretch change too: o2s shifts even when beats are unchanged.
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [zoomPxPerSec, ready, stretches]);

  // ── Beat label overlap culling ─────────────────────────────────────────────
  // Greedy left-to-right pass: hide a label if it would overlap the previous
  // visible one, using estimated label width + 20% margin.
  useEffect(() => {
    const CHAR_WIDTH_PX = 5.5; // 9px monospace
    const MARGIN = 1.2;
    let lastVisibleRight = -Infinity;
    beats.forEach((t, i) => {
      const el = beatLabelElsRef.current.get(t);
      if (!el) return;
      const bpm = i < beats.length - 1 ? 60 / (beats[i + 1] - t) : null;
      const labelPx = o2s(t) * zoomPxPerSec;
      const estimatedWidth = bpm !== null ? String(Math.round(bpm)).length * CHAR_WIDTH_PX : 0;
      if (labelPx >= lastVisibleRight) {
        el.style.display = 'block';
        lastVisibleRight = labelPx + estimatedWidth * MARGIN;
      } else {
        el.style.display = 'none';
      }
    });
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [beats, zoomPxPerSec, ready, stretches]);

  // ── Selection region sync ──────────────────────────────────────────────────
  useEffect(() => {
    const rp = regionsRef.current;
    if (!rp || !ready) return;

    rp.getRegions().filter(r => r.id === "selection").forEach(r => r.remove());

    if (selection) {
      rp.addRegion({
        id: "selection",
        start: selection.start,
        end: selection.end,
        color: "rgba(59, 130, 246, 0.18)",
        drag: false,
        resize: false,
      });
    }
  }, [selection, ready]);

  // ── Stretch overlay regions sync ───────────────────────────────────────────
  useEffect(() => {
    const rp = regionsRef.current;
    if (!rp || !ready) return;

    rp.getRegions().filter(r => r.id.startsWith("stretch-")).forEach(r => r.remove());

    stretches.forEach((s, i) => {
      const pct = Math.round((s.factor - 1) * 100);
      const label = document.createElement("div");
      label.className = "stretch-label";
      label.textContent = `${pct >= 0 ? "+" : ""}${pct}%`;

      rp.addRegion({
        id: `stretch-${i}`,
        start: o2s(s.start), // overlay spans the stretched region on the display axis
        end: o2s(s.end),
        color: s.factor >= 1 ? "rgba(34, 197, 94, 0.12)" : "rgba(239, 68, 68, 0.12)",
        drag: false,
        resize: false,
        content: label,
      });
    });
  }, [stretches, ready]);

  // ── Zoom helpers ───────────────────────────────────────────────────────────
  function applyZoom(next: number) {
    zoomPxPerSecRef.current = next;
    setZoomPxPerSec(next);
    drawPianoRoll();
    if (zoomTimerRef.current) clearTimeout(zoomTimerRef.current);
    zoomTimerRef.current = setTimeout(() => {
      const z = zoomPxPerSecRef.current;
      wsRef.current?.zoom(z);
      bassWsRef.current?.zoom(z);
      const fit = fitPxPerSecRef.current;
      if (fit) {
        const multiplier = z / fit;
        const scrollT = scrollLeftRef.current / z;
        localStorage.setItem(`beats_view_${mp3Path}`, JSON.stringify({ multiplier, scrollTime: scrollT }));
      }
    }, ZOOM_DEBOUNCE_MS);
  }

  const handleZoomIn = () => applyZoom(Math.min(zoomPxPerSec * ZOOM_STEP, fitPxPerSecRef.current * ZOOM_MAX_MULTIPLIER));
  const handleZoomOut = () => applyZoom(Math.max(zoomPxPerSec / ZOOM_STEP, fitPxPerSecRef.current));
  const handleZoomFit = () => applyZoom(fitPxPerSecRef.current);

  function handleRateChange(rate: number) {
    setPlaybackRate(rate);
    setRateProcessing(true);
    invoke("set_playback_rate", { rate })
      .catch(console.error)
      .finally(() => setRateProcessing(false));
  }

  // ── Derived display values ─────────────────────────────────────────────────
  const atFit = fitPxPerSecRef.current > 0 && Math.abs(zoomPxPerSec - fitPxPerSecRef.current) < 0.01;
  const zoomMultiplier = fitPxPerSecRef.current > 0 ? zoomPxPerSec / fitPxPerSecRef.current : 1;
  const zoomLabel = zoomMultiplier < 1.05 ? "Fit"
    : zoomMultiplier >= 10 ? `${Math.round(zoomMultiplier)}×`
    : `${zoomMultiplier.toFixed(1)}×`;


  // ── Render ─────────────────────────────────────────────────────────────────
  return (
    <div className="waveform-editor">
      {loadError && (
        <div className="waveform-error">
          Failed to load audio: {loadError}
        </div>
      )}
      {!ready && !loadError && (
        <div className="waveform-loading">
          <div className="waveform-loading-bar">
            <div className="waveform-loading-fill" style={{ width: `${loadProgress}%` }} />
          </div>
          <span className="waveform-loading-label">
            {loadProgress < 80
              ? `Decoding audio… ${loadProgress}%`
              : loadProgress < 100 ? "Computing peaks…" : "Rendering…"}
            {loadPhase && <span className="load-phase-badge"> · {loadPhase}</span>}
          </span>
        </div>
      )}

      <div
        ref={beatsStripOuterRef}
        className="beats-strip-outer"
        style={{ visibility: ready ? "visible" : "hidden" }}
      >
        <div ref={beatsStripInnerRef} className="beats-strip-inner" />
      </div>

      <div
        ref={bassContainerRef}
        className="bass-canvas"
        style={{ visibility: ready ? "visible" : "hidden" }}
      />

      <div
        ref={containerRef}
        className={[
          "waveform-canvas",
        ].filter(Boolean).join(" ")}
        style={{ visibility: ready ? "visible" : "hidden" }}
      />

      {ready && (
        <div className={[
          "transport-bar",
          recording ? "transport-bar-recording" : "",
        ].filter(Boolean).join(" ")}>

          {/* Playback */}
          <button
            className="transport-btn"
            onClick={() => handleSeek(0)}
            title="Skip to start"
          >⏮</button>
          <button
            className="transport-btn transport-play"
            onClick={togglePlayPause}
            title={playing ? "Pause (Space)" : "Play (Space)"}
          >
            {playing ? "⏸" : "▶"}
          </button>
          <button
            className={`transport-btn transport-play-both${(playing && midiPlaying) ? " transport-play-both-active" : ""}`}
            onClick={togglePlayBoth}
            title={(playing || midiPlaying) ? "Pause MP3 + MIDI" : "Play MP3 + MIDI together"}
          >
            {(playing || midiPlaying) ? "⏸" : "▶"}
            <span className="play-both-badge">+♪</span>
          </button>
          <span className="transport-time">
            {formatTime(currentTime)}
            <span className="transport-duration"> / {formatTime(duration)}</span>
          </span>

          {loadPhase && (
            <span className="load-phase-transport">{loadPhase}</span>
          )}

          {/* Play beats */}
          <label className="play-beats-label">
            <input
              type="checkbox"
              checked={playBeats}
              onChange={e => {
                const v = e.target.checked;
                setPlayBeats(v);
                localStorage.setItem("beats_play_beats", String(v));
              }}
            />
            Play beats
          </label>

          {/* Spectrogram toggle */}
          <label className="play-beats-label">
            <input
              type="checkbox"
              checked={showSpec}
              onChange={e => {
                const v = e.target.checked;
                setShowSpec(v);
                showSpecRef.current = v;
                localStorage.setItem("beats_show_spec", String(v));
                drawSpectrogram();
              }}
            />
            Spectrogram
          </label>
          {showSpec && (
            <button
              className={`spec-expand-btn${specCtrlOpen ? " spec-expand-btn-active" : ""}`}
              onClick={() => {
                const v = !specCtrlOpen;
                setSpecCtrlOpen(v);
                localStorage.setItem("beats_spec_ctrl_open", String(v));
              }}
              title={specCtrlOpen ? "Collapse spectrogram controls" : "Spectrogram controls"}
            >
              ⚙ {specCtrlOpen ? "▾" : "▸"}
            </button>
          )}
          {showSpec && specCtrlOpen && (
            <SpectrogramControls value={specCtrl} onChange={setSpecCtrl} />
          )}

          {/* Rate */}
          <div className="rate-selector">
            {PLAYBACK_RATES.map((r) => (
              <button
                key={r}
                className={`rate-btn${playbackRate === r ? " rate-btn-active" : ""}${rateProcessing && playbackRate === r ? " rate-btn-processing" : ""}`}
                onClick={() => handleRateChange(r)}
                disabled={rateProcessing || (!pcmReady && r !== 1)}
                title={
                  !pcmReady && r !== 1
                    ? "Loading audio in background…"
                    : r < 1 ? "Pitch-correct slow practice via Rubber Band" : undefined
                }
              >
                {rateProcessing && playbackRate === r ? "…" : r === 1 ? "1×" : `${r * 100}%`}
              </button>
            ))}
          </div>

          {/* Record */}
          <button
            className={`transport-btn record-btn${recording ? " record-btn-active" : ""}`}
            onClick={() => recording ? stopRecording() : startRecording()}
            title={recording ? "Stop recording (R)" : "Record beats (R), tap B on each beat"}
          >
            {recording ? "■ Stop" : "● Rec"}
          </button>
          {recording && <span className="rec-indicator">REC · tap B</span>}

          {/* Stretch */}
          {(() => {
            // currentTime / selection are display (stretched) time; stretches are original.
            const curOrig = s2o(currentTime);
            const selOrig = selection ? { start: s2o(selection.start), end: s2o(selection.end) } : null;
            const activeStretch = stretches.find(s => curOrig >= s.start && curOrig <= s.end) ?? null;
            const selectionOverlapsStretch = selOrig !== null &&
              stretches.some(s => selOrig.end > s.start && selOrig.start < s.end);
            const canStretch = !selectionOverlapsStretch && (!!selection || !!activeStretch);
            const isActive = !!selection || !!activeStretch;
            const title = selectionOverlapsStretch
              ? "Selection overlaps an existing stretch — clear selection or pick a different range"
              : selection
                ? "Stretch selected region (S)"
                : activeStretch
                  ? "Edit or remove this stretch region (S)"
                  : "Shift+click to select a region, then stretch (S)";
            return (
              <>
                <button
                  className={`transport-btn stretch-btn${isActive && !selectionOverlapsStretch ? " stretch-btn-active" : ""}`}
                  onClick={() => {
                    if (!canStretch) return;
                    if (selOrig) {
                      setStretchModal({ start: selOrig.start, end: selOrig.end });
                    } else if (activeStretch) {
                      setStretchModal({ start: activeStretch.start, end: activeStretch.end, existingFactor: activeStretch.factor });
                    }
                  }}
                  disabled={!canStretch}
                  title={title}
                >
                  ⇔ Stretch
                </button>
                {selection && (
                  <span className={`stretch-indicator${selectionOverlapsStretch ? " stretch-indicator-warning" : ""}`}>
                    {formatTime(selection.start)} → {formatTime(selection.end)}
                    {selectionOverlapsStretch && " ⚠"}
                  </span>
                )}
              </>
            );
          })()}

          <div className="transport-spacer" />

          {beats.length > 0 && <span className="beat-count">{beats.length} beats</span>}
          {stretches.length > 0 && (
            <span className="beat-count">{stretches.length} stretch{stretches.length !== 1 ? "es" : ""}</span>
          )}

          {/* Export */}
          <button
            className="transport-btn export-btn"
            onClick={handleExport}
            disabled={exportProgress !== null}
            title="Export to MP3 with all stretches applied (requires ffmpeg)"
          >
            ↓ Export MP3
          </button>
          {exportError && (
            <span className="export-error" title={exportError}>⚠ export failed</span>
          )}
          <button
            className="transport-btn export-btn"
            onClick={() => setVideoModalOpen(true)}
            disabled={videoProgress !== null || midiLegend.length === 0}
            title="Export a Synthesia-style piano-roll video with the stretched audio (requires ffmpeg)"
          >
            ▶ Export Video
          </button>
          {videoError && (
            <span className="export-error" title={videoError}>⚠ video export failed</span>
          )}

          {/* Zoom */}
          <span className="zoom-hint">scroll to zoom</span>
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

      {/* ── Piano Roll ───────────────────────────────────────────────────── */}
      <div className="piano-roll-section">
        <div className="piano-roll-header">
          <span className="piano-roll-title">Piano Roll · Rach 2</span>

          {/* Movement selector */}
          {/* MIDI transport */}
          <button className="piano-roll-play-btn" onClick={() => {
            selectedTimelineRef.current = 'midi';
            setSelectedTimeline('midi');
            midiTogglePlay();
          }} title={midiPlaying ? 'Pause MIDI' : 'Play MIDI'}>
            {midiPlaying ? '⏸' : '▶'}
          </button>
          <span className="piano-roll-cursor-time">{formatTime(midiCursorDisp)}</span>

          {/* MIDI volume */}
          <label className="midi-vol" title={`MIDI volume: ${Math.round(midiVolume * 100)}%`}>
            <span className="midi-vol-icon">{midiVolume === 0 ? '🔇' : '🔊'}</span>
            <input
              type="range" min={0} max={1.5} step={0.01} value={midiVolume}
              onChange={e => setMidiVolume(parseFloat(e.target.value))}
            />
            <span className="midi-vol-val">{Math.round(midiVolume * 100)}%</span>
          </label>

          {samplerStatus === 'loading' && (
            <span className="sampler-status sampler-loading">
              Loading Steinway… {samplerProgress}%
            </span>
          )}
          {samplerStatus === 'ready' && (
            <span className="sampler-status sampler-ready" title="Salamander Grand Piano (Steinway D)">
              🎹 Steinway
            </span>
          )}
          {samplerStatus === 'error' && (
            <span className="sampler-status sampler-error" title="Could not load samples — using synthesis">
              ⚠ samples unavailable
            </span>
          )}

          {/* Beat annotation */}
          <button
            className="piano-roll-tap-btn"
            onClick={midiTapBeat}
            title="Mark current MIDI cursor position as a beat (T)"
          >
            ✦ Tap Beat <span className="piano-roll-tap-hint">T</span>
          </button>
          <span className={`piano-roll-beat-count${midiBeats.length > beats.length ? ' warn' : ''}`}>
            {midiBeats.length}/{beats.length} beats
          </span>
          {midiBeats.length > 0 && (
            <button
              className="piano-roll-clear-btn"
              onClick={() => { onMidiBeatsChangeRef.current([]); midiBeatsRef.current = []; drawPianoRoll(); }}
              title="Clear MIDI beats"
            >✕</button>
          )}

          {/* Render options */}
          <div style={{ display: 'flex', gap: 3, flexWrap: 'wrap' }}>
            {ROLL_OPTION_LABELS.map(({ key, label, title }) => (
              <button
                key={key}
                title={title}
                onClick={() => setRollOptions(o => ({ ...o, [key]: !o[key] }))}
                style={{
                  padding: '1px 6px', fontSize: 10, borderRadius: 3, cursor: 'pointer',
                  border: `1px solid ${rollOptions[key] ? 'var(--accent)' : 'var(--border)'}`,
                  background: rollOptions[key] ? 'var(--accent)' : 'transparent',
                  color: rollOptions[key] ? '#fff' : 'var(--text-muted)',
                  fontFamily: 'inherit',
                }}
              >
                {label}
              </button>
            ))}
          </div>

          {/* Legend */}
          <div className="piano-roll-legend">
            {midiLegend.map((t, i) => {
              const isSolo  = soloTrackIndex === i;
              const dimmed  = soloTrackIndex !== null && !isSolo;
              const st      = midiTrackStatsRef.current[i];
              return (
                <span
                  key={i}
                  className="piano-roll-legend-item"
                  onClick={() => setSoloTrackIndex(isSolo ? null : i)}
                  style={{ cursor: 'pointer', opacity: dimmed ? 0.3 : 1, outline: isSolo ? `1px solid ${t.color}` : 'none', borderRadius: 3, padding: '1px 3px', flexDirection: 'column', alignItems: 'flex-start', gap: 2 }}
                >
                  <span style={{ display: 'flex', alignItems: 'center', gap: 4 }}>
                    <span className="piano-roll-legend-dot" style={{ background: t.color }} />
                    {t.name}
                  </span>
                  {st && (rollOptions.ioiRegularity || rollOptions.gridLock) && (
                    <span style={{ display: 'flex', gap: 3, paddingLeft: 12 }}>
                      {rollOptions.ioiRegularity && (
                        <span title={`IOI regularity: ${Math.round(st.ioiRegularity * 100)}%`}
                          style={{ display: 'flex', alignItems: 'center', gap: 2, fontSize: 9, color: 'var(--text-muted)' }}>
                          <span style={{ width: 28, height: 3, background: 'var(--border)', borderRadius: 2, overflow: 'hidden', display: 'inline-block' }}>
                            <span style={{ display: 'block', width: `${st.ioiRegularity * 100}%`, height: '100%', background: '#ffb347', borderRadius: 2 }} />
                          </span>
                        </span>
                      )}
                      {rollOptions.gridLock && (
                        <span title={`Grid alignment: ${Math.round((gridLockCacheRef.current?.scores[i] ?? 0) * 100)}%`}
                          style={{ display: 'flex', alignItems: 'center', gap: 2, fontSize: 9, color: 'var(--text-muted)' }}>
                          <span style={{ width: 28, height: 3, background: 'var(--border)', borderRadius: 2, overflow: 'hidden', display: 'inline-block' }}>
                            <span style={{ display: 'block', width: `${(gridLockCacheRef.current?.scores[i] ?? 0) * 100}%`, height: '100%', background: '#4ea6dc', borderRadius: 2 }} />
                          </span>
                        </span>
                      )}
                    </span>
                  )}
                </span>
              );
            })}
          </div>
        </div>
        <div className="piano-roll-body" style={{ height: PIANO_ROLL_H }}>
          <div className="piano-key-wrap" style={{ width: KEYBOARD_W, flexShrink: 0 }} />
          <div className="piano-roll-notes" style={{ flex: 1 }} />
        </div>
      </div>

      {stretchModal && (
        <StretchModal
          start={stretchModal.start}
          end={stretchModal.end}
          beats={beats}
          onConfirm={handleStretchConfirm}
          onCancel={() => setStretchModal(null)}
          initialFactor={stretchModal.existingFactor}
          onRemove={stretchModal.existingFactor != null ? handleStretchRemove : undefined}
        />
      )}

      {exportProgress !== null && (
        <div className="modal-backdrop">
          <div className="modal export-progress-modal">
            <div className="modal-header">
              <span className="modal-title">Exporting MP3</span>
            </div>
            <div className="modal-body">
              <div className="export-progress-bar-wrap">
                <div
                  className="export-progress-bar-fill"
                  style={{ width: `${exportProgress}%` }}
                />
              </div>
              <span className="export-progress-pct">{exportProgress}%</span>
            </div>
          </div>
        </div>
      )}

      {videoModalOpen && (
        <ExportVideoModal
          totalDuration={duration}
          onConfirm={handleVideoExport}
          onCancel={() => setVideoModalOpen(false)}
        />
      )}

      {videoProgress !== null && (
        <div className="modal-backdrop">
          <div className="modal export-progress-modal">
            <div className="modal-header">
              <span className="modal-title">Exporting Video</span>
            </div>
            <div className="modal-body">
              <div className="export-progress-bar-wrap">
                <div
                  className="export-progress-bar-fill"
                  style={{ width: `${videoProgress.pct}%` }}
                />
              </div>
              <span className="export-progress-pct">
                {videoProgress.stage} · {videoProgress.pct}%
              </span>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
