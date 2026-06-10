import { useState } from "react";
import HomeScreen from "./HomeScreen";
import ProjectEditor from "./ProjectEditor";
import { Project } from "./types";
import "./App.css";

const LAST_PROJECT_KEY = "beats_last_project";

function loadSavedProject(): Project | null {
  try {
    const raw = localStorage.getItem(LAST_PROJECT_KEY);
    if (!raw) return null;
    const p = JSON.parse(raw) as Project;
    if (!p.midiBeats) p.midiBeats = [];
    return p;
  } catch {
    return null;
  }
}

export default function App() {
  const [project, setProject] = useState<Project | null>(loadSavedProject);

  function handleProjectChange(p: Project | null) {
    setProject(p);
    if (p) localStorage.setItem(LAST_PROJECT_KEY, JSON.stringify(p));
    else localStorage.removeItem(LAST_PROJECT_KEY);
  }

  if (project) {
    return (
      <ProjectEditor
        project={project}
        onProjectChange={handleProjectChange}
        onBack={() => handleProjectChange(null)}
      />
    );
  }

  return <HomeScreen onProjectOpen={handleProjectChange} />;
}
