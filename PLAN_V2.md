# Phase 2: Waveform Editor — Implementation Plan

## Core library: WaveSurfer.js

WaveSurfer.js handles everything we need in this phase out of the box: waveform rendering on Canvas, Web Audio API playback, click-to-seek, zoom, and scroll. Building this from scratch with raw Canvas + Web Audio API would take 10x longer with no benefit at this stage.

We'll use two of its plugins:
- **Timeline** — tick marks and time labels below the waveform
- **Zoom** — mousewheel zoom in/out

---

## Step 1 — Tauri asset protocol

Tauri's WebView can't load `file://` paths directly for security reasons. WaveSurfer needs to fetch the audio file as a URL. Tauri provides `convertFileSrc()` which converts `/Users/raj/music/rach2.mp3` → `asset://localhost/Users/raj/music/rach2.mp3`, a scheme the WebView is allowed to load.

This requires two config changes:
1. `tauri.conf.json` — enable the asset protocol with a scope
2. `capabilities/default.json` — verify the CSP allows `asset:` sources

---

## Step 2 — Install dependencies

```
pnpm add wavesurfer.js
```

No other deps needed — WaveSurfer bundles its own plugins.

---

## Step 3 — New component: `WaveformEditor.tsx`

Replaces the "Waveform editor coming soon" placeholder in `ProjectEditor`. Responsibilities:

- Takes `mp3Path: string` as a prop
- Converts it to an asset URL via `convertFileSrc`
- Mounts WaveSurfer into a `<div ref>` via `useEffect`
- Manages playback state (playing/paused, current time, duration)
- Exposes zoom level as state (integer 1–10, where 1 = fit-to-window)

**Internal structure:**
```
<div class="waveform-container">
  <div ref={waveformRef} />        ← WaveSurfer mounts here
  <div class="timeline" />         ← Timeline plugin mounts here
</div>
<div class="transport-bar">
  [|◀] [▶/⏸] time/duration  [zoom-]  [zoom+]  [fit]
</div>
```

---

## Step 4 — Playback controls

| Control | Behavior |
|---|---|
| Play/Pause button | Calls `wavesurfer.playPause()` |
| Space bar | Same as play/pause (keyboard shortcut) |
| Click on waveform | WaveSurfer handles seek natively |
| Current time | Updated via WaveSurfer's `timeupdate` event |
| Skip to start | `wavesurfer.seekTo(0)` |

---

## Step 5 — Zoom + Pan

WaveSurfer's zoom is set by calling `wavesurfer.zoom(pxPerSec)`. At zoom level 1, the waveform fits the full window width (computed as `containerWidth / duration`). Each zoom step multiplies that by a factor (e.g. 1.5x per step).

Pan when zoomed is handled automatically by WaveSurfer — it makes the waveform container scrollable and follows the playhead during playback. We add `overflow: hidden` on the outer container and let WaveSurfer manage its own scroll internally.

Mouse wheel over the waveform → zoom in/out (WaveSurfer Zoom plugin with `modifierKey: 'ctrl'` or unmodified, TBD).

---

## Step 6 — Loading state

WaveSurfer fires a `loading` event (0–100% progress) while it decodes the MP3. We show a simple progress bar or spinner in the waveform area during decode. Large orchestral MP3s can take 2–4 seconds to decode on first load.

---

## Key decisions to make before coding

1. **Zoom gesture**: mousewheel unmodified, or Ctrl+wheel? Unmodified is faster but conflicts with page scroll if there's ever overflow outside the waveform. Recommendation: Ctrl+wheel to avoid conflicts.

2. **Waveform colors**: single color (e.g. accent purple) or dual (unplayed vs played)? Dual (played = brighter, unplayed = dimmer) is the Audacity convention and makes position clearer.

3. **Scroll during playback**: should the waveform auto-scroll to follow the playhead? Yes — `autoScroll: true` in WaveSurfer options, with `autoCenter: true` so playhead stays in center.

---

## What this phase does NOT include

- Beat markers (Phase 3)
- Any Rust audio processing
- Waveform export or modification
