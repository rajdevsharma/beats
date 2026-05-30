# Plan V5: Path to Completion

## Current State

### What works
- MP3 waveform display, click-to-seek, play/pause (WaveSurfer)
- Zoom and pan with Ctrl+scroll
- Beat annotation: tap recording, drag to reposition, select/delete, BPM labels
- Stretch region definition (T key → click anchor → Shift+click end → modal)
- Stretch overlays: green/red for live, slate for baked
- Bake: saves WAV, reloads waveform, repositions beats, project persists state
- Export to WAV
- Project save/load (.beats file, all metadata round-trips correctly)

### What's broken or missing
- **Per-region playback rate** (Step 4): hitchy and broken — WaveSurfer internally stops and restarts audio when `setPlaybackRate()` is called, causing an audible gap and cursor flicker at every stretch boundary. This feature needs to be removed or replaced.
- **Preview quality**: no pitch-correct stretch preview at all; the step 4 attempt was pitch-shifting and is broken anyway
- **Bake quality**: OfflineAudioContext with `playbackRate` = pitch-shifting, not pitch-correct
- **Export format**: WAV only; no MP3
- **Waveform after bake**: shows the baked WAV (Option B), which works, but requires a slow bake cycle just to see the stretched waveform visually

---

## Alternative 1 — Conservative: Fix and Complete

**Philosophy**: Keep the current architecture. Add what's missing. Ship something that works end-to-end with acceptable quality compromises.

### Step 1 — Remove broken Step 4 (1 day)
Strip the `timeupdate`-based `setPlaybackRate` logic entirely. Playback always plays at the user-selected rate, ignoring stretch regions. Stretch regions are visible as overlays only. Preview of stretches requires a bake. The UI becomes honest about what it does.

### Step 2 — Rubber Band in Rust for bake (1–2 weeks)

Replace the OfflineAudioContext (pitch-shifting) bake with a high-quality Rust command:

```
Rust command: bake_audio(mp3_path, stretches, output_wav_path)
  → symphonia decodes MP3 to PCM
  → Rubber Band applies each stretch segment in offline mode
  → WAV written to disk
```

**Rust deps:**
- `symphonia` — pure-Rust MP3/audio decoder, no C deps
- `rubberband-sys` — C++ bindings to Rubber Band library (the same engine Audacity uses)

This is the most impactful single change. It fixes pitch in the baked audio and is the foundation for everything else. Build complexity is real (Rubber Band requires a C++ toolchain) but is a one-time cost.

### Step 3 — Pitch-correct segment preview (1–2 weeks)

Rather than changing WaveSurfer's playback rate (which causes the hitch), pre-process each stretch segment when it is defined and play it via a separate custom audio pipeline at the right moment:

1. When the user confirms a stretch in the modal, immediately run SoundTouch.js on that segment in the background (segment only — fast, not the whole file)
2. Store the processed `AudioBuffer` in memory keyed by stretch ID
3. During WaveSurfer playback, monitor `timeupdate`; a few hundred milliseconds before a stretch region starts, schedule the pre-processed buffer to play via an `AudioBufferSourceNode` at exactly the right `AudioContext` time
4. Simultaneously silence WaveSurfer's audio for the duration of the stretch region (via `ws.setMuted(true)` or `ws.setVolume(0)`)
5. When the stretch region ends, re-enable WaveSurfer

This avoids the stop/restart issue entirely because we're not changing WaveSurfer's playback rate — we're scheduling a parallel audio event. SoundTouch.js gives pitch-correct output. Quality is below Rubber Band but far better than pitch-shifting.

Cursor sync during the stretch region: WaveSurfer's cursor keeps moving through the original-time waveform. Since the baked waveform IS the stretched audio, the cursor will move faster than the audio in a slow-down region. This is a known visual mismatch in the conservative approach.

### Step 4 — MP3 export (2–3 days)

After the Rust bake produces a WAV, convert to MP3:

**Option A (recommended):** Shell out to `ffmpeg` via Tauri's shell plugin:
```
ffmpeg -i baked.wav -q:a 2 output.mp3
```
Fast, high quality, requires ffmpeg to be installed. Fail gracefully with a clear message if not found.

**Option B (fallback):** `lamejs` in the frontend — pure JS LAME port, no system dependency, 5–10× slower than ffmpeg for large files, same quality at equivalent bitrate settings.

Implement A with B as a fallback, or just A with a clear error.

### Step 5 — UX clean-up (ongoing)
- Remove "Bake" requirement for hearing stretches — replaced by Step 3 preview
- Bake becomes the explicit "commit stretches to a high-quality WAV" operation
- Clear labelling of what preview vs. baked vs. exported audio quality means

### Conservative Path Summary

| What | How | Quality | Effort |
|---|---|---|---|
| Preview stretch | SoundTouch.js (pre-processed segment) | Medium (no pitch shift) | 1–2 weeks |
| Bake | Rubber Band in Rust | High | 1–2 weeks |
| MP3 export | ffmpeg shell-out | High | 2–3 days |

**Pros:** Incremental, lower risk, builds on existing code, ships faster  
**Cons:** Two audio systems (WaveSurfer + custom pipeline) are awkward to maintain; cursor sync during preview is visually approximate; SoundTouch preview quality is below what the Rust/Rubber Band bake produces; architectural messiness compounds with future features

**Estimated total effort:** 3–5 weeks

---

## Alternative 2 — Bold: Rust-First Audio Engine

**Philosophy:** Stop working against WaveSurfer's design. Move all audio responsibility to Rust. WaveSurfer becomes a dumb viewport — waveform drawing, regions, zoom, cursor. Rust handles decode, stretch, playback, and export.

This is the architecture the ChatGPT feedback pointed toward, and it is the right long-term design.

### Core model change

**Today:** WaveSurfer loads the MP3, plays it, manages the cursor. We hack around the edges.

**Bold path:** Rust loads the MP3, plays it via the system audio device (`cpal`), and emits position events. WaveSurfer is loaded with precomputed peak data and its cursor is driven by Rust events. There is no audio in the browser at all.

```
┌──────────────────────────────────────────────────────┐
│  Frontend (React + WaveSurfer)                       │
│  - Waveform display from peaks                       │
│  - Regions (beats, stretch overlays)                 │
│  - User interaction (click, zoom, keyboard)          │
│  - Cursor position driven by Tauri events            │
└────────────────────┬─────────────────────────────────┘
                     │ Tauri commands/events
┌────────────────────▼─────────────────────────────────┐
│  Rust Audio Engine                                   │
│  - symphonia: MP3 decode                             │
│  - Rubber Band: pitch-correct time stretching        │
│  - cpal: audio output to system speakers             │
│  - Peak pyramid: multi-resolution waveform data      │
│  - Warp map: stretch segment definitions             │
│  - Position events: emitted every ~16ms              │
└──────────────────────────────────────────────────────┘
```

### Phase 1 — Rust audio engine (2–3 weeks)

Build a Rust audio engine that handles:

- **Decode**: `symphonia` reads the MP3, stores PCM in memory
- **Peak extraction**: compute a peak array at multiple resolutions (e.g. 100 px/sec, 500 px/sec) and send to frontend for WaveSurfer to display
- **Playback**: `cpal` opens the system audio device; a render callback pulls stretched samples
- **Stretch**: Rubber Band in real-time mode processes each warp segment as the playhead passes through it
- **Transport controls** exposed as Tauri commands: `play()`, `pause()`, `seek(t)`, `set_stretches(segments)`
- **Position events**: Rust emits a Tauri event every ~16ms with current playback position; frontend drives WaveSurfer cursor

WaveSurfer is initialised with peaks data (`ws.load(peaks)`) rather than an audio URL. Its `interact` mode is kept for click events (which are forwarded to Rust as `seek` commands), but its own audio playback is never started.

### Phase 2 — Instant waveform updates without baking (1 week)

When the user defines or edits a stretch region, compute warped peaks instantly:

```
For each output peak bucket:
  map output_time → original_time via inverse warp map
  sample original peak array at original_time
```

This is O(n) where n is the number of peak buckets — milliseconds of computation. Feed the warped peaks to WaveSurfer. The waveform immediately shows the stretched shape without any audio processing.

This eliminates the "bake just to see the waveform" workflow entirely.

### Phase 3 — Bake and export (1 week)

**Bake**: Rust command, offline high-quality mode
```
bake_audio(mp3_path, stretches) → wav_path
  symphonia decode → Rubber Band offline mode → WAV write
```

**Export to MP3**: after bake, pipe WAV to ffmpeg:
```
ffmpeg -i baked.wav -q:a 2 output.mp3
```
Or use `mp3lame-encoder` crate for LAME bindings in Rust — no system ffmpeg dependency, but adds a C dep.

### Phase 4 — Clean up WaveSurfer integration (1 week)

- Remove all frontend audio processing code (OfflineAudioContext, encodeWav, audioProcessing.ts)
- Remove SoundTouch.js (if it was added in conservative path)
- WaveSurfer now purely displays; Rust cursor events drive the playhead
- Seek on waveform click: intercept WaveSurfer's `interaction` event, call `invoke("seek", {t})` to Rust

### Bold Path Summary

| What | How | Quality | Effort |
|---|---|---|---|
| Preview stretch | Rubber Band real-time in Rust | High (pitch-correct) | Phase 1 |
| Waveform after stretch | Warped peaks (instant) | Good (approximate) | Phase 2 |
| Bake | Rubber Band offline in Rust | Highest | Phase 3 |
| MP3 export | ffmpeg / LAME in Rust | High | Phase 3 |

**Pros:** Clean architecture, single audio system (Rust), pitch-correct at all stages, instant waveform updates, no more bake-to-display cycle, best possible quality, easier to maintain and extend  
**Cons:** Significant rewrite (2–4 weeks of no visible UI progress), Rubber Band + cpal add native dependencies and build complexity, cursor sync requires careful timing work

**Estimated total effort:** 5–8 weeks

**New Rust dependencies:**
- `symphonia` — pure Rust, no C, excellent MP3 support
- `rubberband-sys` — C++ bindings to Rubber Band (requires C++ toolchain, but this is standard on macOS)
- `cpal` — cross-platform audio I/O, the standard Rust choice (used by Bevy, Rodio, etc.)

---

## Recommendation

For a project you intend to actually use for Rachmaninoff practice, **Alternative 2 is the right choice** for these reasons:

1. The pitch-shifting artifacts in the conservative preview will be noticeable on an orchestral recording — violins and woodwinds are much more sensitive to pitch changes than drums
2. The "two audio systems" awkwardness in Alternative 1 will cause more bugs and edge cases over time
3. The instant waveform update in Alternative 2 (Phase 2) is qualitatively better UX than the bake-to-see workflow

However, if the goal is to ship something functional soon and revisit architecture later, the conservative path gets you there faster with acceptable trade-offs.

**A pragmatic middle path:** Do Alternative 1 Steps 1 and 4 first (remove broken code, add MP3 export via ffmpeg). Then start Alternative 2 as a clean rewrite of the audio layer. This way you have a working product at all times.
