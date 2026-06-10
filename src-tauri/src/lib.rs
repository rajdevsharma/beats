mod audio;

use audio::AudioEngine;
use serde::{Deserialize, Serialize};
use std::fs;
use std::sync::Arc;

#[derive(Serialize, Deserialize, Clone)]
struct StretchSegment {
    start: f64,
    end: f64,
    factor: f64,
}

#[derive(Serialize, Deserialize)]
struct BeatsProject {
    version: u32,
    mp3_path: String,
    #[serde(default)]
    beats: Vec<f64>,
    #[serde(default)]
    stretches: Vec<StretchSegment>,
    #[serde(default)]
    midi_beats: Vec<f64>,
}

#[tauri::command]
fn save_project(
    path: String,
    mp3_path: String,
    beats: Vec<f64>,
    stretches: Vec<StretchSegment>,
    midi_beats: Vec<f64>,
) -> Result<(), String> {
    let project = BeatsProject { version: 1, mp3_path, beats, stretches, midi_beats };
    let json = serde_json::to_string_pretty(&project).map_err(|e| e.to_string())?;
    fs::write(&path, &json).map_err(|e| e.to_string())?;

    // Write timestamped backup alongside the project file
    let p = std::path::Path::new(&path);
    if let Some(parent) = p.parent() {
        let backup_dir = parent.join("beats-backups");
        if fs::create_dir_all(&backup_dir).is_ok() {
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let stem = p.file_name().unwrap_or_default().to_string_lossy();
            let backup_name = format!("{}.{}", stem, ts);
            let _ = fs::write(backup_dir.join(backup_name), &json);
        }
    }

    Ok(())
}

#[tauri::command]
fn load_project(path: String) -> Result<BeatsProject, String> {
    let content = fs::read_to_string(path).map_err(|e| e.to_string())?;
    serde_json::from_str(&content).map_err(|e| e.to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Capture Rust panics in the log file instead of silently killing the process.
    std::panic::set_hook(Box::new(|info| {
        let msg = info.to_string();
        log::error!("PANIC: {}", msg);
        eprintln!("PANIC: {}", msg);
    }));

    tauri::Builder::default()
        .manage(Arc::new(AudioEngine::new()))
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(log::LevelFilter::Info)
                .build(),
        )
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .invoke_handler(tauri::generate_handler![
            save_project,
            load_project,
            audio::export_mp3,
            audio::get_audio_duration,
            audio::engine::load_audio,
            audio::engine::set_stretches_audio,
            audio::engine::set_playback_rate,
            audio::engine::play_audio,
            audio::engine::pause_audio,
            audio::engine::seek_audio,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
