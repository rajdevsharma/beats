pub mod render;

use crate::audio::decode::{decode_audio_file_with_progress, DecodedAudio};
use crate::audio::{apply_stretches, write_wav, StretchSeg};
use rayon::prelude::*;
use render::{Scene, SceneNote, SceneTrack};
use serde::{Deserialize, Serialize};
use std::io::Write;
use tauri::Emitter;

#[derive(Deserialize)]
pub struct VideoNote {
    /// Onset in the output (stretched-audio) timeline, seconds.
    pub start: f64,
    pub dur: f64,
    pub pitch: u8,
    pub vel: f32,
    pub track: usize,
}

#[derive(Deserialize)]
pub struct VideoTrack {
    pub color: [u8; 3],
    pub is_piano: bool,
}

#[derive(Deserialize)]
pub struct VideoOptions {
    pub orientation: String, // "vertical" | "horizontal"
    pub start: f64,          // clip window in output-timeline seconds
    pub end: f64,
    pub fps: u32,
    pub width: u32,
    pub height: u32,
    #[serde(default)]
    pub beat_pulse: bool,
    #[serde(default)]
    pub orchestra_bars: bool,
    #[serde(default)]
    pub tempo_pendulum: bool,
    #[serde(default)]
    pub progress_bar: bool,
    #[serde(default)]
    pub next_note_cue: bool,
    #[serde(default)]
    pub countdown_pips: bool,
    /// Playback speed as a fraction (1.0 = normal, 0.8 = 80% for slow practice).
    /// Audio is time-stretched (pitch-preserved) and the visuals slowed to match.
    #[serde(default = "default_speed")]
    pub speed: f64,
    /// Optional full-screen background video. The first <video length> seconds
    /// are used; the visualization is composited on top with a transparent field.
    #[serde(default)]
    pub bg_video_path: Option<String>,
    /// Background dimmer, 0..1 (1 = full brightness). Lower = competes less.
    #[serde(default = "default_brightness")]
    pub bg_brightness: f64,
}

fn default_brightness() -> f64 { 1.0 }

fn default_speed() -> f64 { 1.0 }

#[derive(Serialize, Clone)]
struct VideoProgress {
    pct: u8,
    stage: &'static str,
}

fn ffmpeg_video_encoder() -> Vec<String> {
    // Prefer Apple hardware encoding; fall back to libx264.
    let has_vt = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("h264_videotoolbox"))
        .unwrap_or(false);
    if has_vt {
        vec![
            "-c:v".into(), "h264_videotoolbox".into(),
            "-b:v".into(), "10M".into(),
        ]
    } else {
        vec![
            "-c:v".into(), "libx264".into(),
            "-preset".into(), "veryfast".into(),
            "-crf".into(), "18".into(),
        ]
    }
}

#[tauri::command]
pub async fn export_video(
    mp3_path: String,
    stretches: Vec<StretchSeg>,
    output_path: String,
    tracks: Vec<VideoTrack>,
    notes: Vec<VideoNote>,
    beats: Vec<f64>,
    options: VideoOptions,
    app: tauri::AppHandle,
) -> Result<(), String> {
    tokio::task::spawn_blocking(move || {
        let emit = |pct: u8, stage: &'static str| {
            let _ = app.emit("video-export-progress", VideoProgress { pct, stage });
        };

        // ── Stage 1: decode source audio (0–30 %) ──────────────────────────
        emit(0, "Decoding audio");
        let audio = decode_audio_file_with_progress(&mp3_path, |frac| {
            emit((frac * 30.0) as u8, "Decoding audio");
        })?;

        // ── Stage 2: apply stretches (30–36 %) ─────────────────────────────
        emit(30, "Applying stretches");
        let DecodedAudio { samples: src, channels: ch, sample_rate: sr, .. } = audio;
        let samples = apply_stretches(&src, ch, sr, &stretches);
        drop(src); // free the unstretched copy
        let out_total = samples.len() / ch;

        // ── Stage 3: trim window, write temp WAV (36–40 %) ─────────────────
        emit(36, "Writing audio track");
        let clip_start = options.start.max(0.0);
        let clip_end = options.end.min(out_total as f64 / sr as f64).max(clip_start + 0.1);
        let clip_dur = clip_end - clip_start;
        let f0 = ((clip_start * sr as f64) as usize).min(out_total);
        let f1 = ((clip_end * sr as f64) as usize).min(out_total);
        let tmp_wav = format!("{}.tmp_video.wav", output_path);
        // Practice speed: time-stretch the clip (pitch-preserving) so the audio
        // plays slower/faster; the visuals are slowed to match below.
        let speed = options.speed.clamp(0.25, 4.0);
        if (speed - 1.0).abs() > 1e-6 {
            let stretched = crate::audio::rubberband::stretch_offline(
                &samples[f0 * ch..f1 * ch], ch, sr, 1.0 / speed,
            );
            write_wav(&stretched, ch, sr, &tmp_wav)?;
        } else {
            write_wav(&samples[f0 * ch..f1 * ch], ch, sr, &tmp_wav)?;
        }
        drop(samples);

        // ── Stage 4: build scene ───────────────────────────────────────────
        let scene_tracks: Vec<SceneTrack> = tracks
            .iter()
            .map(|t| SceneTrack { color: t.color, is_piano: t.is_piano })
            .collect();
        let scene_notes: Vec<SceneNote> = notes
            .iter()
            .filter(|n| n.track < scene_tracks.len())
            .map(|n| SceneNote {
                start: n.start - clip_start,
                end: n.start + n.dur.max(0.05) - clip_start,
                pitch: n.pitch,
                vel: n.vel,
                track: n.track,
            })
            .collect();
        // Keep a margin of beats outside the window so the conductor's swing
        // has correct phase at the clip edges.
        let scene_beats: Vec<f64> = beats
            .iter()
            .map(|b| b - clip_start)
            .filter(|b| *b > -30.0 && *b < clip_dur + 30.0)
            .collect();
        let scene = Scene::new(
            scene_notes,
            scene_tracks,
            scene_beats,
            options.width,
            options.height,
            options.orientation == "horizontal",
            clip_dur,
            options.beat_pulse,
            options.orchestra_bars,
            options.tempo_pendulum,
            options.progress_bar,
            options.next_note_cue,
            options.countdown_pips,
            options.bg_video_path.is_some(),
        );

        // ── Stage 5: render frames → ffmpeg (40–100 %) ─────────────────────
        emit(40, "Rendering video");
        let fps = options.fps.max(1);
        // At speed s the video lasts clip_dur/s; frame i shows content time
        // (i/fps)*s, so the visuals slow in lock-step with the stretched audio.
        let frame_count = (clip_dur / speed * fps as f64).ceil() as u64;
        let size_arg = format!("{}x{}", options.width, options.height);
        let fps_arg = fps.to_string();
        let out_dur = clip_dur / speed; // final video length in seconds
        let w = options.width;
        let h = options.height;

        // The rawvideo frames + stretched wav are always inputs. With a bg video
        // we add it as input 0 and composite our (transparent-field) frames on top.
        let mut args: Vec<String> = vec!["-y".into()];

        let has_bg = options.bg_video_path.as_ref().is_some_and(|p| !p.is_empty());
        if let Some(bg) = options.bg_video_path.as_ref().filter(|_| has_bg) {
            // Input 0: bg video, decode-limited to the output duration (we only
            // ever use its first <out_dur> seconds).
            args.extend(["-t".into(), format!("{out_dur:.3}"), "-i".into(), bg.clone()]);
        }
        // Input N: raw RGBA frames on stdin.
        args.extend([
            "-f".into(), "rawvideo".into(),
            "-pix_fmt".into(), "rgba".into(),
            "-video_size".into(), size_arg,
            "-framerate".into(), fps_arg,
            "-i".into(), "pipe:0".into(),
        ]);
        // Input N+1: the trimmed, (optionally) stretched wav.
        args.extend(["-i".into(), tmp_wav.clone()]);

        if has_bg {
            let b = options.bg_brightness.clamp(0.05, 1.0);
            // [0] bg: cover-fit to frame, match fps, dim via output white point.
            // [1] fg: our frames (already W×H rgba). Overlay fg on bg.
            let fc = format!(
                "[0:v]scale={w}:{h}:force_original_aspect_ratio=increase,crop={w}:{h},fps={fps},\
                 colorlevels=romax={b}:gomax={b}:bomax={b},setsar=1[bg];\
                 [bg][1:v]overlay=0:0:eof_action=pass:format=auto[outv]",
                w = w, h = h, fps = fps, b = b
            );
            args.extend([
                "-filter_complex".into(), fc,
                "-map".into(), "[outv]".into(),
                "-map".into(), "2:a".into(),
            ]);
        }

        args.extend(ffmpeg_video_encoder());
        args.extend([
            "-pix_fmt".into(), "yuv420p".into(),
            "-c:a".into(), "aac".into(),
            "-b:a".into(), "192k".into(),
            "-movflags".into(), "+faststart".into(),
            "-shortest".into(),
            output_path.clone(),
        ]);

        let mut child = std::process::Command::new("ffmpeg")
            .args(&args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("ffmpeg not found: {e}. Install with: brew install ffmpeg"))?;

        // Drain stderr on a thread, keep the tail for error reporting.
        let stderr = child.stderr.take();
        let err_tail = std::thread::spawn(move || {
            use std::io::{BufRead, BufReader};
            let mut tail: Vec<String> = Vec::new();
            if let Some(stderr) = stderr {
                for line in BufReader::new(stderr).lines().flatten() {
                    tail.push(line);
                    if tail.len() > 12 {
                        tail.remove(0);
                    }
                }
            }
            tail.join("\n")
        });

        let mut stdin = child.stdin.take().ok_or("ffmpeg stdin unavailable")?;

        // Render in parallel chunks, write in order. 24 frames @1080p ≈ 200 MB peak.
        const CHUNK: u64 = 24;
        let mut write_err: Option<String> = None;
        let mut done: u64 = 0;
        'outer: for chunk_start in (0..frame_count).step_by(CHUNK as usize) {
            let chunk_end = (chunk_start + CHUNK).min(frame_count);
            let frames: Vec<Vec<u8>> = (chunk_start..chunk_end)
                .into_par_iter()
                .map(|i| scene.render_frame(i as f64 / fps as f64 * speed))
                .collect();
            for f in &frames {
                if let Err(e) = stdin.write_all(f) {
                    write_err = Some(format!("ffmpeg pipe closed: {e}"));
                    break 'outer;
                }
            }
            done = chunk_end;
            let pct = 40.0 + (done as f64 / frame_count as f64) * 59.0;
            emit(pct as u8, "Rendering video");
        }
        drop(stdin); // EOF → ffmpeg finalizes the file

        let status = child.wait().map_err(|e| format!("ffmpeg error: {e}"))?;
        let tail = err_tail.join().unwrap_or_default();
        let _ = std::fs::remove_file(&tmp_wav);

        if let Some(e) = write_err {
            return Err(format!("{e}\n{tail}"));
        }
        if !status.success() {
            return Err(format!("ffmpeg exited with status {status}\n{tail}"));
        }
        if done < frame_count {
            return Err("export ended early".into());
        }
        emit(100, "Done");
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_scene(horizontal: bool) -> Scene {
        let tracks = vec![
            SceneTrack { color: [255, 255, 255], is_piano: true },
            SceneTrack { color: [80, 160, 255], is_piano: false },
            SceneTrack { color: [255, 90, 90], is_piano: false },
            SceneTrack { color: [255, 200, 60], is_piano: false },
        ];
        let mut notes = Vec::new();
        // Piano: arpeggio hitting around t=5
        for (i, p) in [60u8, 64, 67, 72, 76, 63, 55, 48].iter().enumerate() {
            notes.push(SceneNote {
                start: 4.2 + i as f64 * 0.22,
                end: 4.2 + i as f64 * 0.22 + 0.8,
                pitch: *p,
                vel: 0.85,
                track: 0,
            });
        }
        // Strings: sustained chord crossing the hit line at t=5
        for p in [43u8, 50, 57, 62] {
            notes.push(SceneNote { start: 4.0, end: 9.0, pitch: p, vel: 0.6, track: 1 });
        }
        // Winds + brass: falling notes still above the line
        for (i, p) in [70u8, 74, 77, 81, 84].iter().enumerate() {
            notes.push(SceneNote {
                start: 5.6 + i as f64 * 0.4,
                end: 6.0 + i as f64 * 0.4,
                pitch: *p,
                vel: 0.7,
                track: 2 + (i % 2),
            });
        }
        // A doubling: orchestra on the same pitch as piano — piano must stay visible
        notes.push(SceneNote { start: 4.2, end: 6.0, pitch: 60, vel: 0.9, track: 1 });
        // Late piano re-entrance after a rest (exercises the countdown cue).
        for p in [60u8, 67] {
            notes.push(SceneNote { start: 9.0, end: 10.0, pitch: p, vel: 0.9, track: 0 });
        }
        let beats: Vec<f64> = (0..20).map(|i| i as f64 * 0.55).collect();
        Scene::new(notes, tracks, beats, 1280, 720, horizontal, 12.0, true, true, true, true, true, true, false)
    }

    #[test]
    fn renders_sample_frames() {
        for (name, horizontal) in [("vertical", false), ("horizontal", true)] {
            let scene = test_scene(horizontal);
            for t in [5.05f64, 6.5, 7.5] {
                let data = scene.render_frame(t);
                assert_eq!(data.len(), 1280 * 720 * 4);
                let pm = tiny_skia::Pixmap::from_vec(
                    data,
                    tiny_skia::IntSize::from_wh(1280, 720).unwrap(),
                )
                .unwrap();
                let path = std::env::temp_dir().join(format!("beats_video_{name}_t{t}.png"));
                pm.save_png(&path).unwrap();
            }
        }
    }

    #[test]
    fn ffmpeg_pipeline_produces_mp4() {
        let dir = std::env::temp_dir();
        let wav = dir.join("beats_video_test.wav");
        let out = dir.join("beats_video_test.mp4");
        write_wav(&vec![0.0f32; 44100 * 2 * 3], 2, 44100, wav.to_str().unwrap()).unwrap();

        let scene = test_scene(false);
        let mut args: Vec<String> = vec![
            "-y".into(),
            "-f".into(), "rawvideo".into(),
            "-pix_fmt".into(), "rgba".into(),
            "-video_size".into(), "1280x720".into(),
            "-framerate".into(), "30".into(),
            "-i".into(), "pipe:0".into(),
            "-i".into(), wav.to_str().unwrap().into(),
        ];
        args.extend(ffmpeg_video_encoder());
        args.extend([
            "-pix_fmt".into(), "yuv420p".into(),
            "-c:a".into(), "aac".into(),
            "-shortest".into(),
            out.to_str().unwrap().into(),
        ]);
        let mut child = std::process::Command::new("ffmpeg")
            .args(&args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("ffmpeg must be installed");
        let mut stdin = child.stdin.take().unwrap();
        for i in 0..90 {
            let frame = scene.render_frame(4.0 + i as f64 / 30.0);
            stdin.write_all(&frame).unwrap();
        }
        drop(stdin);
        let status = child.wait().unwrap();
        assert!(status.success());
        let size = std::fs::metadata(&out).unwrap().len();
        assert!(size > 50_000, "mp4 suspiciously small: {size} bytes");
    }

    // End-to-end composite over a real background video. Skips unless
    // BEATS_BG_TEST_VIDEO points at a video file, so it doesn't depend on any
    // committed asset. Mirrors the export_video filter_complex.
    #[test]
    fn composite_over_bg_video() {
        let Ok(bg) = std::env::var("BEATS_BG_TEST_VIDEO") else {
            eprintln!("skip: set BEATS_BG_TEST_VIDEO to run");
            return;
        };
        let dir = std::env::temp_dir();
        let wav = dir.join("beats_bg_test.wav");
        let out = dir.join("beats_bg_composite.mp4");
        write_wav(&vec![0.0f32; 44100 * 2 * 3], 2, 44100, wav.to_str().unwrap()).unwrap();

        // transparent_bg = true (last arg)
        let mut notes = Vec::new();
        for (i, p) in [60u8, 64, 67, 72, 76].iter().enumerate() {
            notes.push(SceneNote { start: 4.2 + i as f64 * 0.25, end: 4.2 + i as f64 * 0.25 + 0.9, pitch: *p, vel: 0.85, track: 0 });
        }
        for p in [50u8, 57, 62] { notes.push(SceneNote { start: 4.0, end: 9.0, pitch: p, vel: 0.6, track: 1 }); }
        let tracks = vec![
            SceneTrack { color: [255, 255, 255], is_piano: true },
            SceneTrack { color: [80, 160, 255], is_piano: false },
        ];
        let beats: Vec<f64> = (0..20).map(|i| i as f64 * 0.55).collect();
        let scene = Scene::new(notes, tracks, beats, 1280, 720, false, 12.0, true, true, false, true, true, true, true);

        let (w, h, fps, b) = (1280, 720, 30, 0.5f64);
        let fc = format!(
            "[0:v]scale={w}:{h}:force_original_aspect_ratio=increase,crop={w}:{h},fps={fps},\
             colorlevels=romax={b}:gomax={b}:bomax={b},setsar=1[bg];\
             [bg][1:v]overlay=0:0:eof_action=pass:format=auto[outv]"
        );
        let mut args: Vec<String> = vec![
            "-y".into(), "-t".into(), "3".into(), "-i".into(), bg,
            "-f".into(), "rawvideo".into(), "-pix_fmt".into(), "rgba".into(),
            "-video_size".into(), "1280x720".into(), "-framerate".into(), "30".into(), "-i".into(), "pipe:0".into(),
            "-i".into(), wav.to_str().unwrap().into(),
            "-filter_complex".into(), fc,
            "-map".into(), "[outv]".into(), "-map".into(), "2:a".into(),
        ];
        args.extend(ffmpeg_video_encoder());
        args.extend(["-pix_fmt".into(), "yuv420p".into(), "-c:a".into(), "aac".into(), "-shortest".into(), out.to_str().unwrap().into()]);

        let mut child = std::process::Command::new("ffmpeg")
            .args(&args).stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
            .spawn().expect("ffmpeg");
        let mut stdin = child.stdin.take().unwrap();
        for i in 0..90 { stdin.write_all(&scene.render_frame(4.0 + i as f64 / 30.0)).unwrap(); }
        drop(stdin);
        assert!(child.wait().unwrap().success());
        assert!(std::fs::metadata(&out).unwrap().len() > 50_000);
        eprintln!("composite written: {}", out.display());
    }
}
