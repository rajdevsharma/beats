import { open } from "@tauri-apps/plugin-dialog";
import { invoke } from "@tauri-apps/api/core";
import { Project, BeatsFileData } from "./types";

interface Props {
  onProjectOpen: (project: Project) => void;
}

export default function HomeScreen({ onProjectOpen }: Props) {
  async function handleNewProject() {
    const selected = await open({
      title: "Select MP3 File",
      filters: [{ name: "Audio", extensions: ["mp3"] }],
      multiple: false,
    });
    if (typeof selected === "string") {
      onProjectOpen({ mp3Path: selected });
    }
  }

  async function handleOpenProject() {
    const selected = await open({
      title: "Open Beats Project",
      filters: [{ name: "Beats Project", extensions: ["beats"] }],
      multiple: false,
    });
    if (typeof selected === "string") {
      const data: BeatsFileData = await invoke("load_project", { path: selected });
      onProjectOpen({ mp3Path: data.mp3_path, beatsFilePath: selected });
    }
  }

  return (
    <div className="home-screen">
      <div className="home-header">
        <div className="app-logo">♩</div>
        <h1 className="app-name">Beats</h1>
        <p className="app-tagline">Tempo editor for live performance</p>
      </div>

      <div className="home-actions">
        <button className="action-card" onClick={handleNewProject}>
          <span className="action-icon">+</span>
          <span className="action-title">New Project</span>
          <span className="action-desc">Load an MP3 and start editing</span>
        </button>

        <button className="action-card" onClick={handleOpenProject}>
          <span className="action-icon">↗</span>
          <span className="action-title">Open Project</span>
          <span className="action-desc">Resume a saved .beats project</span>
        </button>
      </div>
    </div>
  );
}
