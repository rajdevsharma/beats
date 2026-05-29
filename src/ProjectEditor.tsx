import { useState } from "react";
import { save } from "@tauri-apps/plugin-dialog";
import { invoke } from "@tauri-apps/api/core";
import { Project } from "./types";
import WaveformEditor from "./WaveformEditor";

interface Props {
  project: Project;
  onProjectChange: (project: Project) => void;
  onBack: () => void;
}

function basename(filePath: string): string {
  return filePath.split(/[\\/]/).pop() ?? filePath;
}

export default function ProjectEditor({ project, onProjectChange, onBack }: Props) {
  const [saving, setSaving] = useState(false);

  async function doSave(path: string) {
    setSaving(true);
    try {
      await invoke("save_project", {
        path,
        mp3Path: project.mp3Path,
        beats: project.beats,
      });
      onProjectChange({ ...project, beatsFilePath: path });
    } finally {
      setSaving(false);
    }
  }

  async function handleSave() {
    if (project.beatsFilePath) {
      await doSave(project.beatsFilePath);
    } else {
      await handleSaveAs();
    }
  }

  async function handleSaveAs() {
    const path = await save({
      title: "Save Beats Project",
      filters: [{ name: "Beats Project", extensions: ["beats"] }],
      defaultPath: basename(project.mp3Path).replace(/\.mp3$/i, "") + ".beats",
    });
    if (path) {
      await doSave(path);
    }
  }

  function handleBeatsChange(beats: number[]) {
    onProjectChange({ ...project, beats });
  }

  const mp3Name = basename(project.mp3Path);
  const saved = !!project.beatsFilePath;

  return (
    <div className="editor-screen">
      <div className="editor-toolbar">
        <button className="toolbar-btn back-btn" onClick={onBack}>
          ← Back
        </button>
        <span className="toolbar-title">
          {mp3Name}
          {!saved && <span className="unsaved-dot" title="Unsaved" />}
        </span>
        <div className="toolbar-actions">
          <button className="toolbar-btn" onClick={handleSave} disabled={saving}>
            {saving ? "Saving…" : "Save"}
          </button>
          <button className="toolbar-btn" onClick={handleSaveAs} disabled={saving}>
            Save As…
          </button>
        </div>
      </div>

      <div className="editor-body">
        <WaveformEditor
          mp3Path={project.mp3Path}
          beats={project.beats}
          onBeatsChange={handleBeatsChange}
        />
      </div>
    </div>
  );
}
