use serde::{Deserialize, Serialize};
use std::fs;

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
}

#[tauri::command]
fn save_project(
    path: String,
    mp3_path: String,
    beats: Vec<f64>,
    stretches: Vec<StretchSegment>,
) -> Result<(), String> {
    let project = BeatsProject { version: 1, mp3_path, beats, stretches };
    let json = serde_json::to_string_pretty(&project).map_err(|e| e.to_string())?;
    fs::write(path, json).map_err(|e| e.to_string())
}

#[tauri::command]
fn write_file(path: String, data: Vec<u8>) -> Result<(), String> {
    fs::write(path, data).map_err(|e| e.to_string())
}

#[tauri::command]
fn load_project(path: String) -> Result<BeatsProject, String> {
    let content = fs::read_to_string(path).map_err(|e| e.to_string())?;
    serde_json::from_str(&content).map_err(|e| e.to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![save_project, load_project, write_file])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
