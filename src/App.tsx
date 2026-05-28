import { useState } from "react";
import HomeScreen from "./HomeScreen";
import ProjectEditor from "./ProjectEditor";
import { Project } from "./types";
import "./App.css";

export default function App() {
  const [project, setProject] = useState<Project | null>(null);

  if (project) {
    return (
      <ProjectEditor
        project={project}
        onProjectChange={setProject}
        onBack={() => setProject(null)}
      />
    );
  }

  return <HomeScreen onProjectOpen={setProject} />;
}
