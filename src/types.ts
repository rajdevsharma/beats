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
  bakedWavPath?: string;
}

export interface BeatsFileData {
  version: number;
  mp3_path: string;
  beats?: number[];
  stretches?: Stretch[];
  baked_wav_path?: string;
}
