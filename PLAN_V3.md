# Phase 3: Beat Annotation — Implementation Plan

## Data Model

`beats: number[]` in `Project` (TypeScript) and `beats: Vec<f64>` in the Rust struct.
JavaScript's `number` is a 64-bit double, giving ~15 significant digits — more than enough for
sub-millisecond precision at any audio position. No rounding; store exactly what
`wavesurfer.getCurrentTime()` returns.

The `.beats` project file gains a `beats` field:

```json
{
  "version": 1,
  "mp3_path": "/path/to/file.mp3",
  "beats": [0.523456789, 1.045678901, 1.567890123]
}
```

Both `save_project` and `load_project` Rust commands are updated to include the beats array.
On load, beats are restored and markers are re-rendered. If the field is absent (old project
file), it defaults to an empty array.

---

## Playback Rate Selector

Segmented control in the transport bar: `25% | 50% | 75% | 100%`.

- Calls `ws.setPlaybackRate(rate)` on change
- Default is 100%
- Pitch shifts at non-1× rates — accepted limitation of the Web Audio API
- At 50% speed, beats are twice as far apart in real time, giving more slack for tapping
- Lives in the transport bar to the right of the time display

---

## Beat Recording Mode

**Entering record mode:**
- "Record" button in transport bar, or `R` key shortcut
- On enter: auto-starts playback, shows a red `● REC` indicator, notes
  `recordingStartTime = ws.getCurrentTime()`

**Tapping beats:**
- `B` key during record mode appends `ws.getCurrentTime()` to a pending taps list
- As playback advances, beats in the range `[recordingStartTime, currentCursorTime]`
  are continuously replaced by the tapped beats — the swept region is "owned" by the
  current recording session
- Beats beyond the cursor are untouched until the cursor reaches them

**Exiting record mode:**
- Press `R` again, `Escape`, or pause
- Commits the result: merges tapped beats into the full beats array, sorts by time
- Saves to project state (marks project as unsaved)
- Beats beyond the cursor at exit time are untouched

---

## Beat Marker Rendering

WaveSurfer Regions plugin. Each beat is a Region:

```
{ start: t, end: t + 0.002, drag: true, resize: false, color: amber }
```

- CSS forces a minimum visual width (2px line) regardless of zoom level
- Color: amber/yellow to stand out against the purple waveform
- Markers are re-synced to the Regions plugin whenever the beats array changes
- If this approach proves awkward for future features (e.g. time stretching),
  switch to a custom SVG overlay synchronized with WaveSurfer's scroll events

---

## BPM Display

Local BPM at beat `i` = `60 / (beats[i+1] - beats[i])`.

- Small label rendered above each marker
- Only shown when zoom is high enough that adjacent labels don't overlap
  (threshold: adjacent markers ≥ 40px apart)
- Shown on hover at all zoom levels regardless of threshold

---

## Beat Editing

- **Drag marker** → reposition in time (Regions plugin handles natively)
- **Click marker** → select it (highlighted state)
- **Delete / Backspace** → remove selected marker, update beats array
- **Click on empty waveform** → add a beat at that position (manual alternative to tapping)
- All edits mark the project as unsaved; existing Save / Save As flow persists everything

---

## Implementation Order

1. Data model — add `beats` to `Project` type, update Rust `BeatsProject` struct,
   update `save_project` / `load_project` commands
2. Playback rate selector — transport bar addition, quick win
3. Beat recording mode + tap input (`R` to arm, `B` to tap, sweep-replace logic)
4. Beat marker rendering via Regions plugin
5. BPM labels (zoom-dependent)
6. Drag, select, delete editing

---

## What This Phase Does NOT Include

- Auto beat detection (Phase 4)
- Time stretching (Phase 4)
- Tap offset correction (future refinement)
- Pitch-correct slow playback (requires rubberband, out of scope)
