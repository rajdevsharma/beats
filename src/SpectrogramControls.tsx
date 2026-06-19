import { SpecCtrl, midiNoteName } from "./WaveformEditor";

interface Props {
  value: SpecCtrl;
  onChange: (next: SpecCtrl) => void;
}

// Register presets — MIDI low/high covering each instrument's playing range
// (fundamentals plus a little headroom), so melodic lines fill the view.
const PRESETS: { label: string; lo: number; hi: number }[] = [
  { label: "Full", lo: 21, hi: 108 },
  { label: "Violin", lo: 55, hi: 103 },   // G3 – G7
  { label: "Cello", lo: 36, hi: 84 },     // C2 – C6
  { label: "Flute", lo: 60, hi: 96 },     // C4 – C7
  { label: "Voice", lo: 48, hi: 84 },     // C3 – C6
];

function Slider({
  label, min, max, step, value, suffix, onChange,
}: {
  label: string; min: number; max: number; step: number;
  value: number; suffix?: string; onChange: (v: number) => void;
}) {
  return (
    <label className="spec-slider" title={label}>
      <span className="spec-slider-label">{label}</span>
      <input
        type="range" min={min} max={max} step={step} value={value}
        onChange={e => onChange(parseFloat(e.target.value))}
      />
      <span className="spec-slider-val">{value}{suffix}</span>
    </label>
  );
}

export default function SpectrogramControls({ value, onChange }: Props) {
  const set = (patch: Partial<SpecCtrl>) => onChange({ ...value, ...patch });
  const activePreset = PRESETS.find(p => p.lo === value.lo && p.hi === value.hi);

  return (
    <div className="spec-controls">
      <div className="spec-seg">
        <button
          className={`spec-seg-btn${value.mode === 'raw' ? ' spec-seg-btn-active' : ''}`}
          onClick={() => set({ mode: 'raw' })}
          title="Raw magnitude — full harmonic detail"
        >Raw</button>
        <button
          className={`spec-seg-btn${value.mode === 'melody' ? ' spec-seg-btn-active' : ''}`}
          onClick={() => set({ mode: 'melody' })}
          title="Melody — harmonic salience collapses each instrument's overtones onto its fundamental line"
        >Melody</button>
      </div>

      <Slider label="Gain"     min={-24} max={24} step={1}   value={value.gain}  suffix="dB" onChange={v => set({ gain: v })} />
      <Slider label="Floor"    min={-90} max={-20} step={1}  value={value.floor} suffix="dB" onChange={v => set({ floor: v })} />
      <Slider label="Contrast" min={0.3} max={2}  step={0.05} value={value.gamma}            onChange={v => set({ gamma: v })} />

      <div className="spec-range">
        <span className="spec-slider-label">Focus</span>
        <input
          type="range" min={21} max={108} step={1} value={value.lo}
          onChange={e => set({ lo: Math.min(parseInt(e.target.value), value.hi - 2) })}
          title="Low edge"
        />
        <input
          type="range" min={21} max={108} step={1} value={value.hi}
          onChange={e => set({ hi: Math.max(parseInt(e.target.value), value.lo + 2) })}
          title="High edge"
        />
        <span className="spec-slider-val">{midiNoteName(value.lo)}–{midiNoteName(value.hi)}</span>
      </div>

      <div className="spec-presets">
        {PRESETS.map(p => (
          <button
            key={p.label}
            className={`spec-preset-btn${activePreset?.label === p.label ? ' spec-preset-btn-active' : ''}`}
            onClick={() => set({ lo: p.lo, hi: p.hi })}
          >{p.label}</button>
        ))}
      </div>
    </div>
  );
}
