//! Real-time audio engine: cpal playback, pitch-correct time stretch + rate control.
//!
//! Model:
//!   original → [user stretch segments via RubberBand offline] → warped
//!   warped   → cpal callback reads at rate-adjusted speed (linear interp)
//!
//! Position is a fixed-point warped-frame index stored as AtomicU64 × 65536.
//! Rate changes are instant: they only change how fast the read-head advances.
//! Position events emit original-timeline seconds so the cursor stays correct.

use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use serde::Serialize;
use tauri::{AppHandle, Emitter};

use super::{apply_stretches, StretchSeg};
use super::decode::{scan_peaks_cached, decode_audio_file_with_progress, decode_audio_file_full, estimated_pcm_bytes, pcm_cache_is_fresh, to_base64, SpectrogramData};
use super::rubberband::{Stretcher, OPT_REALTIME};

// Fixed-point scale: position AtomicU64 = warped_frame × FP_SCALE
const FP_SCALE: u64 = 1 << 16; // 16 fractional bits

// ── Realtime stretch state (held in StreamShared for non-1× rates) ─────────

struct RtState {
    stretcher: Stretcher,
    feed_frame: usize,  // next warped frame to feed into the stretcher
    done: bool,         // true once we've fed the final block
}

// ── Shared state between engine and cpal callback ─────────────────────────

struct StreamShared {
    buf: Arc<Vec<f32>>,          // warped buffer
    channels: usize,
    position_fp: Arc<AtomicU64>, // warped frame × FP_SCALE (cursor + end-detection)
    is_playing: Arc<AtomicBool>,
    rate: f64,
    // Some when rate != 1.0: pitch-preserving realtime stretch via RubberBand R2.
    // try_lock in the callback — returns silence on the rare contended call.
    rt: Option<Mutex<RtState>>,
}

unsafe impl Send for StreamShared {}
unsafe impl Sync for StreamShared {}

// ── Engine inner state ─────────────────────────────────────────────────────

struct Inner {
    sample_rate: u32,
    channels: usize,
    source_path: String,
    original: Arc<Vec<f32>>,
    warped: Arc<Vec<f32>>,
    stretches: Vec<StretchSeg>,
    playback_rate: f64,
    original_duration_secs: f64,
    warped_duration_secs: f64,
    warped_peaks: Vec<Vec<f32>>,
    pcm_ready: bool,
}

pub struct AudioEngine {
    inner: Mutex<Inner>,
    /// Fixed-point warped-frame position (warped_frame × FP_SCALE).
    position_fp: Arc<AtomicU64>,
    is_playing: Arc<AtomicBool>,
    stream: Mutex<Option<cpal::Stream>>,
    pcm_tx: tokio::sync::watch::Sender<bool>,
    pub pcm_rx: tokio::sync::watch::Receiver<bool>,
}

unsafe impl Send for AudioEngine {}
unsafe impl Sync for AudioEngine {}

impl AudioEngine {
    pub fn new() -> Self {
        let empty: Arc<Vec<f32>> = Arc::new(Vec::new());
        let (pcm_tx, pcm_rx) = tokio::sync::watch::channel(false);
        AudioEngine {
            inner: Mutex::new(Inner {
                sample_rate: 44100,
                channels: 2,
                source_path: String::new(),
                original: Arc::clone(&empty),
                warped: Arc::clone(&empty),
                stretches: Vec::new(),
                playback_rate: 1.0,
                original_duration_secs: 0.0,
                warped_duration_secs: 0.0,
                warped_peaks: Vec::new(),
                pcm_ready: false,
            }),
            position_fp: Arc::new(AtomicU64::new(0)),
            is_playing: Arc::new(AtomicBool::new(false)),
            stream: Mutex::new(None),
            pcm_tx,
            pcm_rx,
        }
    }
}

// ── Peak extraction ────────────────────────────────────────────────────────

fn compute_peaks(samples: &[f32], channels: usize, sample_rate: u32) -> Vec<Vec<f32>> {
    let chunk = (sample_rate / 100).max(1) as usize;
    let total_frames = samples.len() / channels;
    let num_peaks = (total_frames + chunk - 1) / chunk;
    let mut out = vec![vec![0f32; num_peaks]; channels];
    for (peak_idx, frame_chunk) in (0..total_frames).step_by(chunk).enumerate() {
        let end = (frame_chunk + chunk).min(total_frames);
        for c in 0..channels {
            let mx = (frame_chunk..end)
                .map(|f| samples[f * channels + c].abs())
                .fold(0f32, f32::max);
            out[c][peak_idx] = mx;
        }
    }
    out
}

// ── Warp-time mapping ─────────────────────────────────────────────────────

fn original_secs_to_warped_frame(t: f64, stretches: &[StretchSeg], sr: u32) -> usize {
    let sr = sr as f64;
    let mut warp_frame = 0usize;
    let mut orig_cursor = 0.0f64;
    for seg in stretches {
        if t <= seg.start {
            return warp_frame + ((t - orig_cursor) * sr) as usize;
        }
        warp_frame += ((seg.start - orig_cursor) * sr) as usize;
        orig_cursor = seg.start;
        let seg_orig = ((seg.end - seg.start) * sr) as usize;
        let seg_warp = (seg_orig as f64 * seg.factor) as usize;
        if t <= seg.end {
            let frac = (t - seg.start) / (seg.end - seg.start);
            return warp_frame + (frac * seg_warp as f64) as usize;
        }
        warp_frame += seg_warp;
        orig_cursor = seg.end;
    }
    warp_frame + ((t - orig_cursor) * sr) as usize
}

fn warped_frame_to_original_secs(frame: usize, stretches: &[StretchSeg], sr: u32) -> f64 {
    let sr_f = sr as f64;
    let mut warp_cursor = 0usize;
    let mut orig_cursor = 0.0f64;
    for seg in stretches {
        let seg_orig_frames = ((seg.end - seg.start) * sr_f) as usize;
        let normal_frames = ((seg.start - orig_cursor) * sr_f) as usize;
        if frame < warp_cursor + normal_frames {
            return orig_cursor + (frame - warp_cursor) as f64 / sr_f;
        }
        warp_cursor += normal_frames;
        orig_cursor = seg.start;
        let seg_warp_frames = (seg_orig_frames as f64 * seg.factor) as usize;
        if frame < warp_cursor + seg_warp_frames {
            let frac = (frame - warp_cursor) as f64 / seg_warp_frames as f64;
            return seg.start + frac * (seg.end - seg.start);
        }
        warp_cursor += seg_warp_frames;
        orig_cursor = seg.end;
    }
    orig_cursor + (frame - warp_cursor) as f64 / sr_f
}

// ── RubberBand realtime state factory ────────────────────────────────────

fn make_rt_state(sample_rate: u32, channels: usize, rate: f64, start_frame: usize) -> RtState {
    // time_ratio = 1/rate: rate 0.5 → play at half speed → time_ratio 2.0
    let time_ratio = 1.0 / rate;
    let mut s = Stretcher::new(sample_rate, channels, OPT_REALTIME, time_ratio);
    // Prime the stretcher with silence so output is available from the first callback.
    let prime = s.get_samples_required().max(1);
    let silence: Vec<Vec<f32>> = (0..channels).map(|_| vec![0.0f32; prime]).collect();
    let refs: Vec<&[f32]> = silence.iter().map(|v| v.as_slice()).collect();
    s.process_rt(&refs, false);
    RtState { stretcher: s, feed_frame: start_frame, done: false }
}

// ── cpal stream factory ───────────────────────────────────────────────────

fn build_stream(shared: Arc<StreamShared>, sample_rate: u32, channels: usize) -> Result<cpal::Stream, String> {
    let host = cpal::default_host();
    let device = host.default_output_device().ok_or("no audio output device")?;
    let config = cpal::StreamConfig {
        channels: channels as u16,
        sample_rate: cpal::SampleRate(sample_rate),
        buffer_size: cpal::BufferSize::Default,
    };

    let stream = device
        .build_output_stream(
            &config,
            move |data: &mut [f32], _| {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    if !shared.is_playing.load(Ordering::Relaxed) {
                        data.fill(0.0);
                        return;
                    }

                    let buf = &shared.buf;
                    let ch = shared.channels.max(1);
                    let total_frames = buf.len() / ch;
                    if total_frames == 0 { data.fill(0.0); return; }

                    if let Some(rt_mutex) = &shared.rt {
                        // ── Pitch-correct path: RubberBand R2 realtime ────────
                        let mut rt = match rt_mutex.try_lock() {
                            Ok(g) => g,
                            Err(_) => { data.fill(0.0); return; } // rare contention
                        };
                        let n_out = data.len() / ch;
                        let mut filled = 0;

                        while filled < n_out {
                            // Drain available output first.
                            let avail = rt.stretcher.available();
                            if avail > 0 {
                                let want = (avail as usize).min(n_out - filled);
                                let got = rt.stretcher.retrieve_interleaved(data, filled * ch, want);
                                filled += got;
                                // Cursor tracks input consumed (≈ output / ratio, good enough for display).
                                shared.position_fp.store((rt.feed_frame as u64) << 16, Ordering::Relaxed);
                                continue;
                            }

                            // Need more input.
                            if rt.done || rt.feed_frame >= total_frames {
                                data[filled * ch..].fill(0.0);
                                if avail < 0 { shared.is_playing.store(false, Ordering::Relaxed); }
                                break;
                            }

                            let required = rt.stretcher.get_samples_required().max(1);
                            let end = (rt.feed_frame + required).min(total_frames);
                            let is_final = end >= total_frames;

                            // Deinterleave into per-channel vecs (~1 KB for typical 256-frame blocks).
                            let deint: Vec<Vec<f32>> = (0..ch)
                                .map(|c| (rt.feed_frame..end).map(|f| buf[f * ch + c]).collect())
                                .collect();
                            let refs: Vec<&[f32]> = deint.iter().map(|v| v.as_slice()).collect();
                            rt.stretcher.process_rt(&refs, is_final);
                            rt.feed_frame = end;
                            if is_final { rt.done = true; }
                        }
                    } else {
                        // ── Rate-1.0 fast path: direct copy ──────────────────
                        let frame = (shared.position_fp.load(Ordering::Relaxed) >> 16) as usize;
                        let start = frame * ch;
                        let available = buf.len().saturating_sub(start);
                        let to_copy = data.len().min(available);
                        if to_copy > 0 {
                            data[..to_copy].copy_from_slice(&buf[start..start + to_copy]);
                        }
                        data[to_copy..].fill(0.0);
                        let new_frame = frame + to_copy / ch;
                        shared.position_fp.store((new_frame as u64) << 16, Ordering::Relaxed);
                        if new_frame >= total_frames {
                            shared.is_playing.store(false, Ordering::Relaxed);
                        }
                    }
                }));
            },
            |err| log::error!("cpal stream error: {err}"),
            None,
        )
        .map_err(|e| format!("build_output_stream: {e}"))?;

    stream.play().map_err(|e| format!("stream.play: {e}"))?;
    Ok(stream)
}

// ── Position event loop ───────────────────────────────────────────────────

pub fn spawn_position_emitter(
    app: AppHandle,
    position_fp: Arc<AtomicU64>,
    is_playing: Arc<AtomicBool>,
    stretches: Vec<StretchSeg>,
    sample_rate: u32,
) {
    tokio::spawn(async move {
        let mut was_playing = true;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(16)).await;
            let playing = is_playing.load(Ordering::Relaxed);
            let warped_frame = (position_fp.load(Ordering::Relaxed) >> 16) as usize;
            let t = warped_frame_to_original_secs(warped_frame, &stretches, sample_rate);
            let _ = app.emit("audio-position", PositionEvent { t, playing });
            if !playing {
                if was_playing { was_playing = false; } else { break; }
            } else {
                was_playing = true;
            }
        }
    });
}

#[derive(Serialize, Clone)]
struct PositionEvent { t: f64, playing: bool }

// ── Tauri command return types ─────────────────────────────────────────────

#[derive(Serialize)]
pub struct SpecLayerOut {
    pub data: String, // base64 of flat u8 [col * bins + bin]
    pub bins: usize,
}

#[derive(Serialize)]
pub struct LoadResult {
    pub peaks: Vec<Vec<f32>>,
    pub bass_peaks: Vec<Vec<f32>>,
    pub duration: f64,
    pub sample_rate: u32,
    pub channels: usize,
    pub spec_cols: usize,
    pub spec_cols_per_sec: f32,
    pub spec_midi_lo: u8,
    pub spec_midi_hi: u8,
    pub spec_raw: SpecLayerOut,
    pub spec_salience: SpecLayerOut,
}

impl LoadResult {
    fn spec_fields(spec: &SpectrogramData) -> (usize, f32, u8, u8, SpecLayerOut, SpecLayerOut) {
        (
            spec.cols,
            spec.cols_per_sec,
            spec.midi_lo,
            spec.midi_hi,
            SpecLayerOut { data: to_base64(&spec.raw.bytes), bins: spec.raw.bins },
            SpecLayerOut { data: to_base64(&spec.salience.bytes), bins: spec.salience.bins },
        )
    }
}

#[derive(Serialize)]
pub struct SetStretchesResult {
    pub peaks: Vec<Vec<f32>>,
    pub bass_peaks: Vec<Vec<f32>>,
    pub warped_duration: f64,
    pub cursor_orig: f64, // playback position in original-audio seconds after remap
}

// ── Tauri commands ─────────────────────────────────────────────────────────

#[tauri::command]
pub async fn load_audio(
    path: String,
    app: AppHandle,
    engine: tauri::State<'_, Arc<AudioEngine>>,
) -> Result<LoadResult, String> {
    let eng = Arc::clone(&engine);

    eng.is_playing.store(false, Ordering::Relaxed);
    drop(eng.stream.lock().unwrap().take());
    eng.pcm_tx.send(false).ok();

    const SINGLE_PASS_THRESHOLD: usize = 500 * 1024 * 1024;
    let use_single_pass = estimated_pcm_bytes(&path)
        .map(|b| b < SINGLE_PASS_THRESHOLD)
        .unwrap_or(false);

    let result = if use_single_pass {
        let path2 = path.clone();
        let app2 = app.clone();
        let (decoded, peaks, bass_peaks, spec) = tokio::task::spawn_blocking(move || {
            decode_audio_file_full(&path2, |frac| {
                let _ = app2.emit("load-progress", (5.0 + frac * 90.0) as u8);
            })
        })
        .await
        .map_err(|e| e.to_string())??;

        let original = Arc::new(decoded.samples);
        {
            let mut inner = eng.inner.lock().unwrap();
            inner.sample_rate = decoded.sample_rate;
            inner.channels = decoded.channels;
            inner.source_path = path.clone();
            inner.original = Arc::clone(&original);
            inner.warped = Arc::clone(&original);
            inner.stretches = Vec::new();
            inner.playback_rate = 1.0;
            inner.original_duration_secs = decoded.duration_secs;
            inner.warped_duration_secs = decoded.duration_secs;
            inner.warped_peaks = peaks.clone();
            inner.pcm_ready = true;
        }
        eng.position_fp.store(0, Ordering::Relaxed);
        eng.pcm_tx.send(true).ok();
        let _ = app.emit("pcm-ready", ());
        let _ = app.emit("load-progress", 100u8);

        let (spec_cols, spec_cols_per_sec, spec_midi_lo, spec_midi_hi, spec_raw, spec_salience) =
            LoadResult::spec_fields(&spec);
        LoadResult {
            peaks, bass_peaks,
            duration: decoded.duration_secs,
            sample_rate: decoded.sample_rate,
            channels: decoded.channels,
            spec_cols, spec_cols_per_sec, spec_midi_lo, spec_midi_hi, spec_raw, spec_salience,
        }
    } else {
        let path2 = path.clone();
        let app2 = app.clone();
        let meta = tokio::task::spawn_blocking(move || {
            scan_peaks_cached(&path2, |frac| {
                let _ = app2.emit("load-progress", (5.0 + frac * 85.0) as u8);
            })
        })
        .await
        .map_err(|e| e.to_string())??;

        {
            let empty = Arc::new(Vec::<f32>::new());
            let mut inner = eng.inner.lock().unwrap();
            inner.sample_rate = meta.sample_rate;
            inner.channels = meta.channels;
            inner.source_path = path.clone();
            inner.original = Arc::clone(&empty);
            inner.warped = Arc::clone(&empty);
            inner.stretches = Vec::new();
            inner.playback_rate = 1.0;
            inner.original_duration_secs = meta.duration_secs;
            inner.warped_duration_secs = meta.duration_secs;
            inner.warped_peaks = meta.peaks.clone();
            inner.pcm_ready = false;
        }
        eng.position_fp.store(0, Ordering::Relaxed);
        let _ = app.emit("load-progress", 95u8);

        let eng3 = Arc::clone(&eng);
        let app3 = app.clone();
        tokio::task::spawn_blocking(move || {
            if pcm_cache_is_fresh(&path) {
                let _ = app3.emit("load-phase", "Reading PCM cache…");
            } else {
                let _ = app3.emit("load-phase", "Decoding audio (slow – will cache for next time)…");
            }
            let decoded = decode_audio_file_with_progress(&path, |_| {})?;
            let original = Arc::new(decoded.samples);
            {
                let mut inner = eng3.inner.lock().unwrap();
                if inner.source_path != path { return Ok(()); }
                inner.original = Arc::clone(&original);
                inner.warped = Arc::clone(&original);
                inner.pcm_ready = true;

                if !inner.stretches.is_empty() {
                    let sr = inner.sample_rate;
                    let ch = inner.channels;
                    let segs = inner.stretches.clone();
                    drop(inner);
                    let new_warped = Arc::new(apply_stretches(&original, ch, sr, &segs));
                    let mut g = eng3.inner.lock().unwrap();
                    if g.source_path == path {
                        g.warped = Arc::clone(&new_warped);
                    }
                }
            }
            eng3.pcm_tx.send(true).ok();
            let _ = app3.emit("pcm-ready", ());
            Ok::<(), String>(())
        });

        let (spec_cols, spec_cols_per_sec, spec_midi_lo, spec_midi_hi, spec_raw, spec_salience) =
            LoadResult::spec_fields(&meta.spectrogram);
        LoadResult {
            peaks: meta.peaks,
            bass_peaks: meta.bass_peaks,
            duration: meta.duration_secs,
            sample_rate: meta.sample_rate,
            channels: meta.channels,
            spec_cols, spec_cols_per_sec, spec_midi_lo, spec_midi_hi, spec_raw, spec_salience,
        }
    };

    Ok(result)
}

#[tauri::command]
pub async fn set_stretches_audio(
    stretches: Vec<StretchSeg>,
    engine: tauri::State<'_, Arc<AudioEngine>>,
) -> Result<SetStretchesResult, String> {
    if !*engine.pcm_rx.borrow() {
        let mut rx = engine.pcm_rx.clone();
        rx.wait_for(|v| *v).await.map_err(|_| "Audio loading was cancelled".to_string())?;
    }

    let eng = Arc::clone(&engine);
    tokio::task::spawn_blocking(move || {
        let (original, sample_rate, channels) = {
            let inner = eng.inner.lock().unwrap();
            (Arc::clone(&inner.original), inner.sample_rate, inner.channels)
        };
        if original.is_empty() { return Err("no audio loaded".to_string()); }

        let new_warped = apply_stretches(&original, channels, sample_rate, &stretches);
        let new_warped_frames = new_warped.len() / channels;
        let new_warped_duration = new_warped_frames as f64 / sample_rate as f64;
        let peaks = compute_peaks(&new_warped, channels, sample_rate);
        let bass_peaks = super::decode::compute_bass_peaks(&new_warped, channels, sample_rate);
        let new_warped = Arc::new(new_warped);

        // Remap position: old warped frame → original secs → new warped frame
        let old_fp = eng.position_fp.load(Ordering::Relaxed);
        let old_warped_frame = (old_fp >> 16) as usize;
        let mut inner = eng.inner.lock().unwrap();
        let orig_t = warped_frame_to_original_secs(old_warped_frame, &inner.stretches, sample_rate);
        let new_warped_frame = original_secs_to_warped_frame(orig_t, &stretches, sample_rate);
        eng.position_fp.store((new_warped_frame as u64) << 16, Ordering::Relaxed);

        inner.stretches = stretches;
        inner.warped = Arc::clone(&new_warped);
        inner.warped_duration_secs = new_warped_duration;
        inner.warped_peaks = peaks.clone();

        let was_playing = eng.is_playing.load(Ordering::Relaxed);
        let rate = inner.playback_rate;
        if was_playing {
            eng.is_playing.store(false, Ordering::Relaxed);
            drop(eng.stream.lock().unwrap().take());
            let cur_frame = (eng.position_fp.load(Ordering::Relaxed) >> 16) as usize;
            let rt = if (rate - 1.0).abs() > 1e-6 {
                Some(Mutex::new(make_rt_state(sample_rate, channels, rate, cur_frame)))
            } else { None };
            let shared = Arc::new(StreamShared {
                buf: Arc::clone(&new_warped),
                channels,
                position_fp: Arc::clone(&eng.position_fp),
                is_playing: Arc::clone(&eng.is_playing),
                rate,
                rt,
            });
            match build_stream(shared, sample_rate, channels) {
                Ok(s) => {
                    eng.is_playing.store(true, Ordering::Relaxed);
                    *eng.stream.lock().unwrap() = Some(s);
                }
                Err(e) => return Err(e),
            }
        }

        Ok(SetStretchesResult { peaks, bass_peaks, warped_duration: new_warped_duration, cursor_orig: orig_t })
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn set_playback_rate(
    rate: f64,
    app: AppHandle,
    engine: tauri::State<'_, Arc<AudioEngine>>,
) -> Result<(), String> {
    let rate = rate.clamp(0.1, 2.0);
    let eng = Arc::clone(&engine);

    let (warped, sample_rate, channels, stretches, was_playing) = {
        let inner = eng.inner.lock().unwrap();
        if (inner.playback_rate - rate).abs() < 1e-6 { return Ok(()); }
        (
            Arc::clone(&inner.warped),
            inner.sample_rate,
            inner.channels,
            inner.stretches.clone(),
            eng.is_playing.load(Ordering::Relaxed),
        )
    };

    eng.inner.lock().unwrap().playback_rate = rate;

    if was_playing {
        eng.is_playing.store(false, Ordering::Relaxed);
        drop(eng.stream.lock().unwrap().take());
        let cur_frame = (eng.position_fp.load(Ordering::Relaxed) >> 16) as usize;
        let rt = if (rate - 1.0).abs() > 1e-6 {
            Some(Mutex::new(make_rt_state(sample_rate, channels, rate, cur_frame)))
        } else { None };
        let shared = Arc::new(StreamShared {
            buf: warped,
            channels,
            position_fp: Arc::clone(&eng.position_fp),
            is_playing: Arc::clone(&eng.is_playing),
            rate,
            rt,
        });
        tokio::task::spawn_blocking(move || {
            match build_stream(shared, sample_rate, channels) {
                Ok(s) => {
                    eng.is_playing.store(true, Ordering::Relaxed);
                    *eng.stream.lock().unwrap() = Some(s);
                    spawn_position_emitter(app, Arc::clone(&eng.position_fp), Arc::clone(&eng.is_playing), stretches, sample_rate);
                }
                Err(e) => log::error!("set_playback_rate: rebuild stream failed: {e}"),
            }
        })
        .await
        .map_err(|e| e.to_string())?;
    }

    Ok(())
}

#[tauri::command]
pub async fn play_audio(
    app: AppHandle,
    engine: tauri::State<'_, Arc<AudioEngine>>,
) -> Result<(), String> {
    if engine.is_playing.load(Ordering::Relaxed) { return Ok(()); }
    {
        let inner = engine.inner.lock().unwrap();
        if inner.source_path.is_empty() { return Err("no audio loaded".to_string()); }
    }

    if !*engine.pcm_rx.borrow() {
        let mut rx = engine.pcm_rx.clone();
        rx.wait_for(|v| *v).await.map_err(|_| "Audio loading was cancelled".to_string())?;
    }

    let eng = Arc::clone(&engine);
    tokio::task::spawn_blocking(move || {
        let inner = eng.inner.lock().unwrap();
        let total_frames = inner.warped.len() / inner.channels;
        let cur_frame = (eng.position_fp.load(Ordering::Relaxed) >> 16) as usize;
        if cur_frame >= total_frames {
            eng.position_fp.store(0, Ordering::Relaxed);
        }

        let rate = inner.playback_rate;
        let sample_rate = inner.sample_rate;
        let channels = inner.channels;
        let stretches = inner.stretches.clone();
        let warped = Arc::clone(&inner.warped);
        drop(inner);

        let cur_frame = (eng.position_fp.load(Ordering::Relaxed) >> 16) as usize;
        let rt = if (rate - 1.0).abs() > 1e-6 {
            Some(Mutex::new(make_rt_state(sample_rate, channels, rate, cur_frame)))
        } else { None };
        let shared = Arc::new(StreamShared {
            buf: warped,
            channels,
            position_fp: Arc::clone(&eng.position_fp),
            is_playing: Arc::clone(&eng.is_playing),
            rate,
            rt,
        });

        let stream = build_stream(shared, sample_rate, channels)?;
        eng.is_playing.store(true, Ordering::Relaxed);
        *eng.stream.lock().unwrap() = Some(stream);
        spawn_position_emitter(app, Arc::clone(&eng.position_fp), Arc::clone(&eng.is_playing), stretches, sample_rate);
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn pause_audio(
    app: AppHandle,
    engine: tauri::State<'_, Arc<AudioEngine>>,
) -> Result<(), String> {
    let eng = Arc::clone(&engine);
    let (sr, stretches) = {
        let inner = eng.inner.lock().unwrap();
        (inner.sample_rate, inner.stretches.clone())
    };
    eng.is_playing.store(false, Ordering::Relaxed);
    drop(eng.stream.lock().unwrap().take());

    let warped_frame = (eng.position_fp.load(Ordering::Relaxed) >> 16) as usize;
    let t = warped_frame_to_original_secs(warped_frame, &stretches, sr);
    let _ = app.emit("audio-position", PositionEvent { t, playing: false });
    Ok(())
}

#[tauri::command]
pub async fn seek_audio(
    t: f64,
    app: AppHandle,
    engine: tauri::State<'_, Arc<AudioEngine>>,
) -> Result<(), String> {
    let eng = Arc::clone(&engine);
    let (sr, stretches) = {
        let inner = eng.inner.lock().unwrap();
        (inner.sample_rate, inner.stretches.clone())
    };
    let warped_frame = original_secs_to_warped_frame(t, &stretches, sr);
    eng.position_fp.store((warped_frame as u64) << 16, Ordering::Relaxed);

    let actual_t = warped_frame_to_original_secs(warped_frame, &stretches, sr);
    let playing = eng.is_playing.load(Ordering::Relaxed);
    let _ = app.emit("audio-position", PositionEvent { t: actual_t, playing });
    Ok(())
}
