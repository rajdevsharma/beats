use serde::{Deserialize, Serialize};
use std::fs;

#[derive(Serialize, Deserialize)]
struct BeatsProject {
    version: u32,
    mp3_path: String,
}

#[tauri::command]
fn save_project(path: String, mp3_path: String) -> Result<(), String> {
    let project = BeatsProject { version: 1, mp3_path };
    let json = serde_json::to_string_pretty(&project).map_err(|e| e.to_string())?;
    fs::write(path, json).map_err(|e| e.to_string())
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
        .invoke_handler(tauri::generate_handler![save_project, load_project])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
