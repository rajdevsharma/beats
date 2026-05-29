# Phase 4: Time Stretching — Implementation Plan

## Overview

Non-destructive time stretching of selected audio regions. Stretches are stored as metadata
and applied during playback preview and final export. The original MP3 is never modified.

---

## Data Model

Add `stretches` to the project file:

```json
{
  "version": 1,
  "mp3_path": "/path/to/file.mp3",
  "beats": [0.523, 1.045, ...],
  "stretches": [
    { "start": 45.2, "end": 78.9, "factor": 1.20 },
    { "start": 112.0, "end": 134.5, "factor": 0.85 }
  ]
}
```

`factor > 1` = slower (more time), `factor < 1` = faster (less time). A factor of 1.20 means
the region takes 20% longer to play — e.g. 58 BPM becomes ~48 BPM.

Both `save_project` and `load_project` Rust commands updated to include stretches.
Old project files without a `stretches` field default to an empty array.

---

## UI: Stretch Mode

A dedicated stretch mode avoids conflicts with existing click-to-seek and beat-tap behavior.

- **`T` key** (or toolbar button) toggles stretch mode on/off
- Stretch mode indicator in the transport bar (similar to REC indicator)
- **In stretch mode:**
  - First click → sets the selection anchor (vertical line at that time position)
  - Shift+click → sets the selection end → immediately opens the stretch modal
  - Escape → cancels current selection
- **Outside stretch mode:** all existing interactions (click-to-seek, beat tap, etc.) work normally

---

## Stretch Modal

Opens after a region is selected in stretch mode. Shows:

- **Selected range**: `1:23.4 → 1:58.7` (35.3 seconds)
- **Current BPM**: weighted average BPM across beats in the selected region
  - If beats exist in the region: `60 / avg_beat_gap`, weighted toward the center of the selection
  - If no beats in the region: `N/A`
- **Stretch factor input**: numeric field, default `100%`
  - `> 100%` = slower, `< 100%` = faster
- **New BPM preview**: recomputes live as user types (current BPM / factor)
- **New duration preview**: `original_duration × factor`
- Confirm → applies the stretch; Cancel → dismisses

---

## Audio Preview

Uses **SoundTouch.js** (pure-JS pitch-correct time stretching) running in the browser.

- Only the selected segment is processed — not the full file
- On confirm, the selected segment's PCM data is extracted from the decoded AudioBuffer,
  run through SoundTouch at the given factor, and stored as a stretched AudioBuffer in memory
- Playback uses a custom Web Audio pipeline (bypassing WaveSurfer's native playback):
  - Before stretch region: normal AudioBufferSourceNode at 1× rate
  - Within stretch region: pre-processed stretched AudioBuffer
  - After stretch region: normal AudioBufferSourceNode resumes
- WaveSurfer cursor position is kept in sync with stretched time so it moves correctly
  through stretched regions (slower in a 120% stretch, faster in an 80% one)

Pitch-correct preview is preferred over `playbackRate` (pitch-shift) since the user is
evaluating rhythm; pitch artifacts would be distracting even if technically tolerable.

---

## Waveform Display

### Default: Overlay (immediate, no wait)

After confirming a stretch, a colored semi-transparent band is drawn over the stretched
region using the WaveSurfer Regions plugin:

- Color: green-tinted for slow-downs (> 100%), red-tinted for speed-ups (< 100%)
- Label: `+20%` or `−15%` shown inside the band
- The underlying waveform image is unchanged (still drawn from the original audio)
- Beat markers within the region shift to their stretched positions

The cursor moves at the correct stretched rate through the region during playback,
so there is a visual mismatch between cursor speed and waveform detail — this is expected
and acceptable for tempo evaluation purposes.

### On-demand: Full Re-render (`Cmd+R` or toolbar button)

When waveform accuracy matters more than wait time, the user can trigger a full re-render:

1. All stretches are applied to the full audio buffer (SoundTouch.js sequentially per segment)
2. The resulting buffer is fed to WaveSurfer as a new audio source
3. WaveSurfer re-draws the waveform from the stretched audio
4. Beat markers remain at their correct (stretched) time positions
5. A loading indicator is shown during processing

Re-render is idempotent — triggering it again after further edits re-processes from the
original audio + current stretch list (not from the previously-rendered result).

---

## Export

High-quality pitch-correct export using **Rubberband** (via `rubberband-rs` Rust crate):

1. Tauri Rust command reads the original MP3, decodes to PCM
2. Applies each stretch segment in sequence using Rubberband's offline mode
3. Encodes the result to MP3 via `ffmpeg` (bundled)
4. Writes to user-selected output path

Rubberband is the same algorithm used by Audacity and Ardour — no pitch artifacts at
reasonable stretch ratios (up to ~2×).

---

## Implementation Order

1. Data model — add `stretches` to Project type and Rust struct
2. Stretch mode toggle + region selection UI (`T` key, anchor + shift-click)
3. Stretch modal (BPM display, factor input, live preview math)
4. SoundTouch.js integration + custom Web Audio playback pipeline
5. Overlay display via Regions plugin (colored bands, labels)
6. Beat marker repositioning after stretch
7. On-demand full re-render (`Cmd+R`)
8. Rubberband export (Rust command + ffmpeg)

---

## What This Phase Does NOT Include

- Overlapping stretch regions (undefined behavior — enforce non-overlap in the UI)
- Pitch shifting as a separate operation (only tempo)
- Undo/redo (future phase)
