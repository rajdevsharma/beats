//! Real-time audio engine: cpal playback, pitch-correct stretching, position events.

use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use serde::Serialize;
use tauri::{AppHandle, Emitter};

use super::{apply_stretches, StretchSeg};
use super::decode::decode_audio_file;

// ── Shared state between engine methods and cpal callback ─────────────────

struct StreamShared {
    warped: Arc<Vec<f32>>,
    channels: usize,
    // Frame index into `warped` (not original).  Atomics allow lock-free
    // reads from the position-event thread.
    position: Arc<AtomicU64>,
    is_playing: Arc<AtomicBool>,
}

// ── Main engine ────────────────────────────────────────────────────────────

struct Inner {
    sample_rate: u32,
    channels: usize,
    original: Vec<f32>,
    warped: Arc<Vec<f32>>,
    stretches: Vec<StretchSeg>,
    original_duration_secs: f64,
    warped_duration_secs: f64,
    /// Peaks for the *warped* waveform: one Vec<f32> per channel, ~100 pts/sec.
    warped_peaks: Vec<Vec<f32>>,
}

pub struct AudioEngine {
    inner: Mutex<Inner>,
    position: Arc<AtomicU64>,
    is_playing: Arc<AtomicBool>,
    // Keeps the cpal stream alive while playing.
    stream: Mutex<Option<cpal::Stream>>,
}

// cpal::Stream is Send on all Tauri-supported platforms.
unsafe impl Send for AudioEngine {}
unsafe impl Sync for AudioEngine {}

impl AudioEngine {
    pub fn new() -> Self {
        AudioEngine {
            inner: Mutex::new(Inner {
                sample_rate: 44100,
                channels: 2,
                original: Vec::new(),
                warped: Arc::new(Vec::new()),
                stretches: Vec::new(),
                original_duration_secs: 0.0,
                warped_duration_secs: 0.0,
                warped_peaks: Vec::new(),
            }),
            position: Arc::new(AtomicU64::new(0)),
            is_playing: Arc::new(AtomicBool::new(false)),
            stream: Mutex::new(None),
        }
    }
}

// ── Peak extraction ────────────────────────────────────────────────────────

/// Downsample interleaved PCM to per-channel peak arrays at ~100 pts/sec.
fn compute_peaks(samples: &[f32], channels: usize, sample_rate: u32) -> Vec<Vec<f32>> {
    let chunk = (sample_rate / 100).max(1) as usize; // ~10ms per peak
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

// ── cpal stream factory ───────────────────────────────────────────────────

fn build_stream(shared: Arc<StreamShared>, sample_rate: u32, channels: usize) -> Result<cpal::Stream, String> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or("no audio output device")?;

    let config = cpal::StreamConfig {
        channels: channels as u16,
        sample_rate: cpal::SampleRate(sample_rate),
        buffer_size: cpal::BufferSize::Default,
    };

    let stream = device
        .build_output_stream(
            &config,
            move |data: &mut [f32], _| {
                if !shared.is_playing.load(Ordering::Relaxed) {
                    data.fill(0.0);
                    return;
                }
                let frame = shared.position.load(Ordering::Relaxed) as usize;
                let start_sample = frame * shared.channels;
                let w = &shared.warped;
                let available = w.len().saturating_sub(start_sample);
                let to_copy = data.len().min(available);
                data[..to_copy].copy_from_slice(&w[start_sample..start_sample + to_copy]);
                data[to_copy..].fill(0.0);
                let new_frame = frame + to_copy / shared.channels;
                shared.position.store(new_frame as u64, Ordering::Relaxed);
                if new_frame * shared.channels >= w.len() {
                    shared.is_playing.store(false, Ordering::Relaxed);
                }
            },
            |err| eprintln!("cpal stream error: {err}"),
            None,
        )
        .map_err(|e| format!("build_output_stream: {e}"))?;

    stream.play().map_err(|e| format!("stream.play: {e}"))?;
    Ok(stream)
}

// ── Position event loop ───────────────────────────────────────────────────

/// Spawns a task that emits `audio-position` events at ~60fps while playing.
/// Emits one final event with `playing: false` when playback stops.
pub fn spawn_position_emitter(
    app: AppHandle,
    position: Arc<AtomicU64>,
    is_playing: Arc<AtomicBool>,
    stretches: Vec<StretchSeg>,
    sample_rate: u32,
) {
    tokio::spawn(async move {
        let mut was_playing = true;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(16)).await;
            let playing = is_playing.load(Ordering::Relaxed);
            let frame = position.load(Ordering::Relaxed) as usize;
            let t = warped_frame_to_original_secs(frame, &stretches, sample_rate);
            let _ = app.emit("audio-position", PositionEvent { t, playing });
            if !playing {
                if was_playing {
                    // One more event to confirm stop
                    was_playing = false;
                } else {
                    break;
                }
            } else {
                was_playing = true;
            }
        }
    });
}

#[derive(Serialize, Clone)]
struct PositionEvent {
    t: f64,
    playing: bool,
}

// ── Tauri command return types ─────────────────────────────────────────────

#[derive(Serialize)]
pub struct LoadResult {
    pub peaks: Vec<Vec<f32>>,
    pub duration: f64,
    pub sample_rate: u32,
    pub channels: usize,
}

#[derive(Serialize)]
pub struct SetStretchesResult {
    pub peaks: Vec<Vec<f32>>,
    pub warped_duration: f64,
}

// ── Tauri commands ─────────────────────────────────────────────────────────

#[tauri::command]
pub async fn load_audio(
    path: String,
    engine: tauri::State<'_, Arc<AudioEngine>>,
) -> Result<LoadResult, String> {
    let eng = Arc::clone(&engine);
    tokio::task::spawn_blocking(move || {
        let decoded = decode_audio_file(&path)?;
        let peaks = compute_peaks(&decoded.samples, decoded.channels, decoded.sample_rate);
        let warped = Arc::new(decoded.samples.clone());
        let warped_duration = decoded.duration_secs;
        let mut inner = eng.inner.lock().unwrap();
        inner.sample_rate = decoded.sample_rate;
        inner.channels = decoded.channels;
        inner.original = decoded.samples;
        inner.warped = Arc::clone(&warped);
        inner.stretches = Vec::new();
        inner.original_duration_secs = decoded.duration_secs;
        inner.warped_duration_secs = warped_duration;
        inner.warped_peaks = peaks.clone();
        eng.position.store(0, Ordering::Relaxed);
        eng.is_playing.store(false, Ordering::Relaxed);
        // Drop any existing stream
        drop(eng.stream.lock().unwrap().take());

        Ok(LoadResult {
            peaks,
            duration: decoded.duration_secs,
            sample_rate: decoded.sample_rate,
            channels: decoded.channels,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn set_stretches_audio(
    stretches: Vec<StretchSeg>,
    engine: tauri::State<'_, Arc<AudioEngine>>,
) -> Result<SetStretchesResult, String> {
    let eng = Arc::clone(&engine);
    tokio::task::spawn_blocking(move || {
        let (original, sample_rate, channels) = {
            let inner = eng.inner.lock().unwrap();
            (inner.original.clone(), inner.sample_rate, inner.channels)
        };
        if original.is_empty() {
            return Err("no audio loaded".to_string());
        }

        let new_warped = apply_stretches(&original, channels, sample_rate, &stretches);
        let new_warped_frames = new_warped.len() / channels;
        let new_warped_duration = new_warped_frames as f64 / sample_rate as f64;
        let peaks = compute_peaks(&new_warped, channels, sample_rate);
        let new_warped = Arc::new(new_warped);

        // Remap current position to new warped space
        let old_frame = eng.position.load(Ordering::Relaxed) as usize;
        let mut inner = eng.inner.lock().unwrap();
        let old_orig_t = warped_frame_to_original_secs(old_frame, &inner.stretches, sample_rate);
        let new_frame = original_secs_to_warped_frame(old_orig_t, &stretches, sample_rate);
        inner.stretches = stretches;
        inner.warped = Arc::clone(&new_warped);
        inner.warped_duration_secs = new_warped_duration;
        inner.warped_peaks = peaks.clone();
        eng.position.store(new_frame as u64, Ordering::Relaxed);

        // If playing, rebuild the stream with the new warped buffer
        let was_playing = eng.is_playing.load(Ordering::Relaxed);
        if was_playing {
            eng.is_playing.store(false, Ordering::Relaxed);
            drop(eng.stream.lock().unwrap().take());

            let shared = Arc::new(StreamShared {
                warped: new_warped,
                channels,
                position: Arc::clone(&eng.position),
                is_playing: Arc::clone(&eng.is_playing),
            });
            match build_stream(shared, sample_rate, channels) {
                Ok(s) => {
                    eng.is_playing.store(true, Ordering::Relaxed);
                    *eng.stream.lock().unwrap() = Some(s);
                }
                Err(e) => return Err(e),
            }
        }

        Ok(SetStretchesResult { peaks, warped_duration: new_warped_duration })
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn play_audio(
    app: AppHandle,
    engine: tauri::State<'_, Arc<AudioEngine>>,
) -> Result<(), String> {
    let eng = Arc::clone(&engine);
    tokio::task::spawn_blocking(move || {
        if eng.is_playing.load(Ordering::Relaxed) {
            return Ok(());
        }
        let inner = eng.inner.lock().unwrap();
        if inner.warped.is_empty() {
            return Err("no audio loaded".to_string());
        }
        let frame = eng.position.load(Ordering::Relaxed) as usize;
        // If at end, rewind
        let total_frames = inner.warped.len() / inner.channels;
        let start_frame = if frame >= total_frames { 0 } else { frame };
        eng.position.store(start_frame as u64, Ordering::Relaxed);

        let shared = Arc::new(StreamShared {
            warped: Arc::clone(&inner.warped),
            channels: inner.channels,
            position: Arc::clone(&eng.position),
            is_playing: Arc::clone(&eng.is_playing),
        });
        let sample_rate = inner.sample_rate;
        let channels = inner.channels;
        let stretches = inner.stretches.clone();
        drop(inner);

        let stream = build_stream(shared, sample_rate, channels)?;
        eng.is_playing.store(true, Ordering::Relaxed);
        *eng.stream.lock().unwrap() = Some(stream);

        spawn_position_emitter(app, Arc::clone(&eng.position), Arc::clone(&eng.is_playing), stretches, sample_rate);
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
    let (sr, stretches, frame) = {
        let inner = eng.inner.lock().unwrap();
        let fr = eng.position.load(Ordering::Relaxed) as usize;
        (inner.sample_rate, inner.stretches.clone(), fr)
    };
    eng.is_playing.store(false, Ordering::Relaxed);
    drop(eng.stream.lock().unwrap().take());

    let t = warped_frame_to_original_secs(frame, &stretches, sr);
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
    let frame = original_secs_to_warped_frame(t, &stretches, sr);
    eng.position.store(frame as u64, Ordering::Relaxed);

    // Emit position immediately so cursor moves right away
    let actual_t = warped_frame_to_original_secs(frame, &stretches, sr);
    let playing = eng.is_playing.load(Ordering::Relaxed);
    let _ = app.emit("audio-position", PositionEvent { t: actual_t, playing });
    Ok(())
}
