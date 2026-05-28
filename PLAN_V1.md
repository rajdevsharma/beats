# Beats — Plan V1

## Goal

A desktop audio editor for playing along with an orchestral backing track (specifically: Rachmaninoff Piano Concerto No. 2, piano part removed). Two core problems to solve:

1. **Cue visibility** — knowing when the orchestra comes back in during long piano-only stretches.
2. **Tempo editing** — stretching or squeezing specific sections so the recorded tempo matches personal preference.

---

## Feature Tiers

### Tier 1 — Waveform Player (v1 milestone)

- Load an MP3 from disk.
- Display the waveform (using WaveSurfer.js).
- Click anywhere on the waveform to seek; hit Play/Pause to control playback.
- Keyboard shortcuts for play/pause, seek forward/back.

This tier alone is useful. Validates the Tauri + WaveSurfer.js stack before touching audio processing.

### Tier 2 — Beat Annotation

- **Auto beat detection** (via `aubio` through Rust FFI): runs on load, produces a draft set of beat markers overlaid on the waveform.
  - Caveat: aubio works well on rhythmically regular music; Rachmaninoff with rubato and fermatas will produce noisy output. Treat auto-detection as a rough draft only.
- **Manual annotation**: click to place a beat marker; drag to reposition; delete key to remove.
- Beat markers are displayed as vertical lines on the waveform.
- Markers snap to a configurable grid or free-float.

### Tier 3 — Tempo Display

- From the set of beat markers, compute local BPM at each marker: `bpm = 60 / (t[n] - t[n-1])`.
- Display BPM value above each beat marker (or as a separate tempo lane below the waveform).
- Color-code or flag regions where BPM deviates significantly from a user-set target tempo.

### Tier 4 — Time Stretching

- Select a region (click + drag).
- Set a target BPM for that region (or drag the beat markers to define new beat positions).
- The audio between the previous and next beat marker stretches/compresses to fit.
- Use `rubberband` (via `rubberband-rs`) — the same algorithm used by Audacity and Ardour, with a real-time mode for low-latency preview.
- Hit Play immediately after editing to preview the stretched result in real time.

### Tier 5 — Export & Project Save

- **Export to MP3**: apply all stretch edits and encode via `ffmpeg` (bundled in the Tauri binary).
- **Save project**: write a JSON file referencing the original MP3 path plus all beat marker positions, stretch edits, and annotations.
- **Load project**: restore all markers and edits from a saved JSON file.

---

## Technical Stack

| Layer | Choice | Notes |
|---|---|---|
| Shell | Tauri (Rust + WebView) | Already scaffolded |
| Frontend | React + TypeScript | Already scaffolded |
| Waveform UI | WaveSurfer.js | Battle-tested, handles large files |
| Beat detection | `aubio` via Rust FFI | Best open-source option; noisy on complex orchestral audio |
| Time stretching | `rubberband-rs` | Gold standard algorithm; real-time mode available |
| MP3 export | `ffmpeg` bundled | Handles encoding after rubberband processes PCM |
| Project format | JSON | Beat positions (seconds), stretch segments, annotations |

---

## Key Design Decisions

**Drag-to-stretch interaction model**: the beat grid is authoritative. Dragging a beat marker redefines where that beat falls in time; the audio segment between the surrounding markers stretches to match. This maps cleanly onto rubberband's segment-by-segment API.

**Manual annotation is first-class, not a fallback**: auto detection is a convenience to generate a starting point. The manual annotation UX needs to be fast and low-friction (single click to place, drag to move).

**Project file, not destructive editing**: the original MP3 is never modified. All edits are stored as metadata and applied on export or playback.

---

## Honest Risk Flags

- **Beat detection quality**: the biggest unknown. Orchestral music with rubato is hard. Budget time for manual cleanup tooling regardless of how well auto-detection performs.
- **Real-time stretch preview**: computationally intensive. Rubberband's real-time mode should handle it, but latency under heavy stretch ratios needs testing.
- **FFmpeg bundling**: increases binary size and adds build complexity for cross-platform distribution.

---

## Suggested Build Order

1. Waveform player (Tier 1) — prove the Tauri + WaveSurfer.js stack
2. Manual beat annotation (Tier 2, manual half) — get the core UX right
3. Tempo display (Tier 3) — pure computation, no new dependencies
4. Auto beat detection (Tier 2, auto half) — add aubio once the manual flow is solid
5. Time stretching + preview (Tier 4) — the hardest piece, saved for last
6. Export + project save (Tier 5)
