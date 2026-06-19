pub mod decode;
pub mod engine;
pub mod rubberband;

use decode::decode_audio_file_with_progress;
use rubberband::stretch_offline;
use serde::Deserialize;
use tauri::Emitter;

pub use engine::AudioEngine;

#[derive(Deserialize, Clone)]
pub struct StretchSeg {
    pub start: f64,
    pub end: f64,
    pub factor: f64,
}

/// Apply all stretch segments to interleaved f32 PCM, return stretched samples.
pub fn apply_stretches(samples: &[f32], channels: usize, sample_rate: u32, stretches: &[StretchSeg]) -> Vec<f32> {
    if stretches.is_empty() {
        return samples.to_vec();
    }

    let ch = channels;
    let sr = sample_rate;
    let total_frames = samples.len() / ch;
    let total_dur = total_frames as f64 / sr as f64;

    let mut segs = stretches.to_vec();
    segs.sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap());

    let frame_range = |start_s: f64, end_s: f64| -> (usize, usize) {
        let s = (start_s * sr as f64).round() as usize;
        let e = (end_s * sr as f64).round() as usize;
        (s.min(total_frames), e.min(total_frames))
    };

    let mut result: Vec<f32> = Vec::with_capacity(samples.len());
    let mut cursor = 0.0f64;

    for seg in &segs {
        if seg.start > cursor + 1e-6 {
            let (s, e) = frame_range(cursor, seg.start);
            result.extend_from_slice(&samples[s * ch..e * ch]);
        }
        let (s, e) = frame_range(seg.start, seg.end);
        if e > s {
            let stretched = stretch_offline(&samples[s * ch..e * ch], ch, sr, seg.factor);
            result.extend(stretched);
        }
        cursor = seg.end;
    }

    if cursor < total_dur - 1e-6 {
        let (s, e) = frame_range(cursor, total_dur);
        result.extend_from_slice(&samples[s * ch..e * ch]);
    }

    result
}

pub(crate) fn write_wav(samples: &[f32], channels: usize, sample_rate: u32, path: &str) -> Result<(), String> {
    let spec = hound::WavSpec {
        channels: channels as u16,
        sample_rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer = hound::WavWriter::create(path, spec).map_err(|e| e.to_string())?;
    for &s in samples {
        writer.write_sample(s).map_err(|e| e.to_string())?;
    }
    writer.finalize().map_err(|e| e.to_string())
}

// ── Tauri commands ─────────────────────────────────────────────────────────

#[tauri::command]
pub async fn export_mp3(
    mp3_path: String,
    stretches: Vec<StretchSeg>,
    output_path: String,
    app: tauri::AppHandle,
) -> Result<(), String> {
    tokio::task::spawn_blocking(move || {
        let emit = |pct: u8| { let _ = app.emit("export-progress", pct); };

        // Stage 1: decode (0–70 %)
        emit(0);
        let audio = decode_audio_file_with_progress(&mp3_path, |frac| {
            emit((frac * 70.0) as u8);
        })?;
        let duration_secs = audio.duration_secs;

        // Stage 2: apply stretches (70–82 %)
        emit(70);
        let samples = apply_stretches(&audio.samples, audio.channels, audio.sample_rate, &stretches);

        // Stage 3: write temp WAV (82–88 %)
        emit(82);
        let tmp_wav = format!("{}.tmp_export.wav", output_path);
        write_wav(&samples, audio.channels, audio.sample_rate, &tmp_wav)?;

        // Stage 4: ffmpeg encode (88–100 %)
        // Use -progress pipe:1 for structured per-frame progress on stdout.
        emit(88);
        let mut child = std::process::Command::new("ffmpeg")
            .args([
                "-y", "-i", &tmp_wav,
                "-q:a", "2",
                "-progress", "pipe:1",
                "-nostats",
                &output_path,
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("ffmpeg not found: {e}. Install with: brew install ffmpeg"))?;

        if let Some(stdout) = child.stdout.take() {
            use std::io::{BufRead, BufReader};
            for line in BufReader::new(stdout).lines().flatten() {
                // ffmpeg -progress emits "out_time_us=<microseconds>"
                if let Some(us_str) = line.strip_prefix("out_time_us=") {
                    if let Ok(us) = us_str.trim().parse::<i64>() {
                        if duration_secs > 0.0 {
                            let frac = (us as f64 / 1_000_000.0 / duration_secs).clamp(0.0, 1.0);
                            emit((88.0 + frac * 11.0) as u8);
                        }
                    }
                }
            }
        }

        let status = child.wait().map_err(|e| format!("ffmpeg error: {e}"))?;
        let _ = std::fs::remove_file(&tmp_wav);

        if status.success() {
            emit(100);
            Ok(())
        } else {
            Err(format!("ffmpeg exited with status {status}"))
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn get_audio_duration(path: String) -> Result<f64, String> {
    tokio::task::spawn_blocking(move || {
        let audio = decode_audio_file_with_progress(&path, |_| {})?;
        Ok(audio.duration_secs)
    })
    .await
    .map_err(|e| e.to_string())?
}
