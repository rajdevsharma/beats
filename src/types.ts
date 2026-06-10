export interface Stretch {
  start: number;
  end: number;
  factor: number; // > 1 = slower, < 1 = faster
}

export interface Project {
  mp3Path: string;
  beatsFilePath?: string;
  beats: number[];
  stretches: Stretch[];
  midiBeats: number[];
}

export interface BeatsFileData {
  version: number;
  mp3_path: string;
  beats?: number[];
  stretches?: Stretch[];
  midi_beats?: number[];
}
