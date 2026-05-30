pub mod decode;
pub mod rubberband;

use decode::{decode_audio_file, DecodedAudio};
use rubberband::stretch_offline;
use serde::Deserialize;
use std::path::Path;

#[derive(Deserialize, Clone)]
pub struct StretchSeg {
    pub start: f64,
    pub end: f64,
    pub factor: f64,
}

/// Apply all stretch segments to decoded audio, return interleaved f32 samples.
pub fn apply_stretches(audio: &DecodedAudio, stretches: &[StretchSeg]) -> Vec<f32> {
    if stretches.is_empty() {
        return audio.samples.clone();
    }

    let ch = audio.channels;
    let sr = audio.sample_rate;
    let total_frames = audio.samples.len() / ch;
    let total_dur = total_frames as f64 / sr as f64;

    // Sort by start time
    let mut segs = stretches.to_vec();
    segs.sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap());

    let frame_range = |start_s: f64, end_s: f64| -> (usize, usize) {
        let s = (start_s * sr as f64).round() as usize;
        let e = (end_s * sr as f64).round() as usize;
        (s.min(total_frames), e.min(total_frames))
    };

    let mut result: Vec<f32> = Vec::with_capacity(audio.samples.len());
    let mut cursor = 0.0f64;

    for seg in &segs {
        // Normal segment before the stretch
        if seg.start > cursor + 1e-6 {
            let (s, e) = frame_range(cursor, seg.start);
            result.extend_from_slice(&audio.samples[s * ch..e * ch]);
        }
        // Stretch segment via Rubber Band
        let (s, e) = frame_range(seg.start, seg.end);
        if e > s {
            let input = &audio.samples[s * ch..e * ch];
            let stretched = stretch_offline(input, ch, sr, seg.factor);
            result.extend(stretched);
        }
        cursor = seg.end;
    }

    // Trailing normal segment
    if cursor < total_dur - 1e-6 {
        let (s, e) = frame_range(cursor, total_dur);
        result.extend_from_slice(&audio.samples[s * ch..e * ch]);
    }

    result
}

/// Write interleaved f32 samples as a 32-bit float WAV file.
fn write_wav(
    samples: &[f32],
    channels: usize,
    sample_rate: u32,
    path: &str,
) -> Result<(), String> {
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

// ── Tauri commands ────────────────────────────────────────────────────────────

/// Process the original MP3 with all stretches applied using Rubber Band
/// (offline, high-quality, pitch-preserving) and write a 32-bit float WAV.
#[tauri::command]
pub async fn bake_audio(
    mp3_path: String,
    stretches: Vec<StretchSeg>,
    output_path: String,
) -> Result<(), String> {
    tokio::task::spawn_blocking(move || {
        let audio = decode_audio_file(&mp3_path)?;
        let samples = apply_stretches(&audio, &stretches);
        write_wav(&samples, audio.channels, audio.sample_rate, &output_path)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Bake the audio (Rubber Band) then convert WAV → MP3 via ffmpeg.
/// Requires ffmpeg to be installed (brew install ffmpeg).
#[tauri::command]
pub async fn export_mp3(
    mp3_path: String,
    stretches: Vec<StretchSeg>,
    output_path: String,
) -> Result<(), String> {
    tokio::task::spawn_blocking(move || {
        // Write to a temp WAV first
        let tmp_wav = format!("{}.tmp_export.wav", output_path);
        let audio = decode_audio_file(&mp3_path)?;
        let samples = apply_stretches(&audio, &stretches);
        write_wav(&samples, audio.channels, audio.sample_rate, &tmp_wav)?;

        // Convert to MP3 with ffmpeg (-y = overwrite, -q:a 2 = ~190kbps VBR)
        let status = std::process::Command::new("ffmpeg")
            .args(["-y", "-i", &tmp_wav, "-q:a", "2", &output_path])
            .status()
            .map_err(|e| format!("ffmpeg not found: {e}. Install with: brew install ffmpeg"))?;

        // Clean up temp file regardless of ffmpeg result
        let _ = std::fs::remove_file(&tmp_wav);

        if status.success() {
            Ok(())
        } else {
            Err(format!("ffmpeg exited with status {status}"))
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Compute the duration of the original file in seconds.
#[tauri::command]
pub async fn get_audio_duration(path: String) -> Result<f64, String> {
    tokio::task::spawn_blocking(move || {
        let audio = decode_audio_file(&path)?;
        Ok(audio.duration_secs)
    })
    .await
    .map_err(|e| e.to_string())?
}
