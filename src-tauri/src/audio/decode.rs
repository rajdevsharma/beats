use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use std::fs::File;
use rustfft::{FftPlanner, num_complex::Complex};

pub struct DecodedAudio {
    pub samples: Vec<f32>,
    pub channels: usize,
    pub sample_rate: u32,
    pub duration_secs: f64,
}

/// One spectrogram layer on the shared semitone-log time/pitch grid.
/// Bytes are linear-dB-mapped u8 (DB_FLOOR..DB_CEIL), NOT gamma-corrected —
/// the frontend reconstructs dB and applies gain/floor/contrast live.
pub struct SpecLayer {
    pub bytes: Vec<u8>, // flat [col * bins + bin], bin 0 = MIDI_LO
    pub bins: usize,
}

pub struct SpectrogramData {
    pub cols: usize,
    pub cols_per_sec: f32,
    pub midi_lo: u8,
    pub midi_hi: u8,
    pub raw: SpecLayer,      // fine magnitude, RAW_BPST bins/semitone
    pub salience: SpecLayer, // harmonic-sum pitch salience, 1 bin/semitone
}

pub struct AudioMeta {
    pub channels: usize,
    pub sample_rate: u32,
    pub duration_secs: f64,
    pub peaks: Vec<Vec<f32>>,
    pub bass_peaks: Vec<Vec<f32>>,
    pub spectrogram: SpectrogramData,
}

// ── Base64 encoder (no dep) ───────────────────────────────────────────────

pub fn to_base64(data: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(CHARS[(n >> 18) as usize] as char);
        out.push(CHARS[((n >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 { out.push(CHARS[((n >> 6) & 0x3f) as usize] as char); } else { out.push('='); }
        if chunk.len() > 2 { out.push(CHARS[(n & 0x3f) as usize] as char); } else { out.push('='); }
    }
    out
}

// ── STFT accumulator ──────────────────────────────────────────────────────
// Emits one spectrogram column every `hop` mono frames.
// Uses a circular ring buffer to avoid allocating a fresh window each hop.

// A larger window than a plain rhythm-spectrogram would use: trades some time
// resolution for the frequency resolution needed to separate adjacent pitches
// in the low/mid register where melodic lines live.
const STFT_SIZE: usize = 4096;

// Pitch grid shared by both layers (A0 .. C8 — the 88-key piano range).
const MIDI_LO: u8 = 21;
const MIDI_HI: u8 = 108;
const RAW_BPST: usize = 2;   // raw layer: bins per semitone
const HARMONICS: usize = 12; // harmonics summed for the salience layer

// Non-destructive dB encoding window. 90 dB / 255 ≈ 0.35 dB per step.
const DB_FLOOR: f32 = -90.0;
const DB_CEIL: f32 = 0.0;

#[inline]
fn midi_to_freq(midi: f32) -> f32 {
    440.0 * 2.0_f32.powf((midi - 69.0) / 12.0)
}

#[inline]
fn db_to_u8(mag_norm: f32) -> u8 {
    let db = 20.0 * mag_norm.max(1e-10).log10();
    (((db - DB_FLOOR) / (DB_CEIL - DB_FLOOR)).clamp(0.0, 1.0) * 255.0) as u8
}

struct StftAccumulator {
    hop: usize,
    cols_per_sec: f32,
    hann: Vec<f32>,
    ring: Vec<f32>,           // circular, length = STFT_SIZE
    ring_write: usize,        // index of next write slot (= oldest slot to read)
    frames_since_hop: usize,
    fft_plan: std::sync::Arc<dyn rustfft::Fft<f32>>,
    fft_input: Vec<Complex<f32>>,
    fft_scratch: Vec<Complex<f32>>,
    mag: Vec<f32>,            // |X[k]| for k in 0..STFT_SIZE/2, reused per col
    // Precomputed sampling plans (fractional FFT-bin lookups):
    raw_bins: usize,
    raw_taps: Vec<(usize, f32)>,         // per raw bin: (base idx, frac)
    sal_bins: usize,
    sal_taps: Vec<Vec<(usize, f32, f32)>>, // per pitch: harmonic (idx, frac, weight)
    raw_cols: Vec<u8>,
    sal_cols: Vec<u8>,
    col_count: usize,
}

impl StftAccumulator {
    fn new(sample_rate: u32) -> Self {
        let hop = (sample_rate / 20).max(1) as usize; // 20 cols/sec
        let cols_per_sec = sample_rate as f32 / hop as f32;
        let half = STFT_SIZE / 2;
        let bin_hz = sample_rate as f32 / STFT_SIZE as f32;
        let nyquist = sample_rate as f32 / 2.0;

        let hann: Vec<f32> = (0..STFT_SIZE)
            .map(|i| 0.5 * (1.0 - (2.0 * std::f32::consts::PI * i as f32
                / (STFT_SIZE - 1) as f32).cos()))
            .collect();

        // Linear-interpolated FFT-bin lookup for an arbitrary frequency.
        let tap = |freq: f32| -> (usize, f32) {
            let pos = (freq / bin_hz).clamp(0.0, (half - 2) as f32);
            (pos.floor() as usize, pos.fract())
        };

        // Raw layer: RAW_BPST bins per semitone across the pitch range.
        let semis = (MIDI_HI - MIDI_LO) as usize;
        let raw_bins = semis * RAW_BPST + 1;
        let raw_taps: Vec<(usize, f32)> = (0..raw_bins)
            .map(|b| tap(midi_to_freq(MIDI_LO as f32 + b as f32 / RAW_BPST as f32)))
            .collect();

        // Salience layer: one bin per semitone; each is a weighted harmonic sum.
        let sal_bins = semis + 1;
        let weight = |h: usize| 1.0 / (h as f32).powf(0.8);
        let sal_taps: Vec<Vec<(usize, f32, f32)>> = (0..sal_bins)
            .map(|p| {
                let f0 = midi_to_freq((MIDI_LO + p as u8) as f32);
                (1..=HARMONICS)
                    .map(|h| f0 * h as f32)
                    .take_while(|&fh| fh < nyquist)
                    .enumerate()
                    .map(|(i, fh)| {
                        let (idx, frac) = tap(fh);
                        (idx, frac, weight(i + 1))
                    })
                    .collect()
            })
            .collect();

        let mut planner = FftPlanner::<f32>::new();
        let fft_plan = planner.plan_fft_forward(STFT_SIZE);
        let scratch_len = fft_plan.get_inplace_scratch_len();

        StftAccumulator {
            hop,
            cols_per_sec,
            hann,
            ring: vec![0.0; STFT_SIZE],
            ring_write: 0,
            frames_since_hop: 0,
            fft_plan,
            fft_input: vec![Complex::default(); STFT_SIZE],
            fft_scratch: vec![Complex::default(); scratch_len],
            mag: vec![0.0; half],
            raw_bins,
            raw_taps,
            sal_bins,
            sal_taps,
            raw_cols: Vec::new(),
            sal_cols: Vec::new(),
            col_count: 0,
        }
    }

    #[inline]
    fn push(&mut self, mono: f32) {
        self.ring[self.ring_write] = mono;
        self.ring_write = (self.ring_write + 1) % STFT_SIZE;
        self.frames_since_hop += 1;
        if self.frames_since_hop >= self.hop {
            self.frames_since_hop = 0;
            self.emit_col();
        }
    }

    #[inline]
    fn sample(&self, idx: usize, frac: f32) -> f32 {
        // Linear interp between adjacent magnitude bins.
        self.mag[idx] + (self.mag[idx + 1] - self.mag[idx]) * frac
    }

    fn emit_col(&mut self) {
        // Build windowed FFT input from ring buffer (oldest sample first)
        let start = self.ring_write; // oldest position
        for i in 0..STFT_SIZE {
            let s = self.ring[(start + i) % STFT_SIZE];
            self.fft_input[i] = Complex { re: s * self.hann[i], im: 0.0 };
        }
        self.fft_plan.process_with_scratch(&mut self.fft_input, &mut self.fft_scratch);

        let half = STFT_SIZE / 2;
        for k in 0..half {
            self.mag[k] = self.fft_input[k].norm();
        }
        let norm = (STFT_SIZE as f32) / 2.0; // ≈ peak magnitude for full-scale

        // Raw magnitude layer.
        for &(idx, frac) in &self.raw_taps {
            self.raw_cols.push(db_to_u8(self.sample(idx, frac) / norm));
        }

        // Harmonic salience layer: each pitch is the weighted average magnitude
        // of its harmonic series, so an instrument's whole overtone stack
        // collapses onto a single bright line at its fundamental.
        for taps in &self.sal_taps {
            let mut sum = 0.0;
            let mut wsum = 0.0;
            for &(idx, frac, w) in taps {
                sum += w * self.sample(idx, frac);
                wsum += w;
            }
            let avg = if wsum > 0.0 { sum / wsum } else { 0.0 };
            self.sal_cols.push(db_to_u8(avg / norm));
        }

        self.col_count += 1;
    }

    fn finish(self) -> SpectrogramData {
        SpectrogramData {
            cols: self.col_count,
            cols_per_sec: self.cols_per_sec,
            midi_lo: MIDI_LO,
            midi_hi: MIDI_HI,
            raw: SpecLayer { bytes: self.raw_cols, bins: self.raw_bins },
            salience: SpecLayer { bytes: self.sal_cols, bins: self.sal_bins },
        }
    }
}

// ── Second-order IIR lowpass (Butterworth, Q=0.707) ───────────────────────

struct BiquadLP {
    b0: f32, b1: f32, b2: f32,
    a1: f32, a2: f32,
    x1: f32, x2: f32, y1: f32, y2: f32,
}

impl BiquadLP {
    fn new(cutoff_hz: f32, sample_rate: u32) -> Self {
        use std::f32::consts::PI;
        let omega = 2.0 * PI * cutoff_hz / sample_rate as f32;
        let alpha = omega.sin() / (2.0 * 0.707_f32);
        let cos_w = omega.cos();
        let a0 = 1.0 + alpha;
        Self {
            b0: (1.0 - cos_w) / 2.0 / a0,
            b1: (1.0 - cos_w) / a0,
            b2: (1.0 - cos_w) / 2.0 / a0,
            a1: -2.0 * cos_w / a0,
            a2: (1.0 - alpha) / a0,
            x1: 0.0, x2: 0.0, y1: 0.0, y2: 0.0,
        }
    }

    fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2
              - self.a1 * self.y1 - self.a2 * self.y2;
        self.x2 = self.x1; self.x1 = x;
        self.y2 = self.y1; self.y1 = y;
        y
    }
}

/// Bass-onset peaks from interleaved PCM — a 200 Hz low-pass followed by
/// per-chunk energy flux (rising low-frequency energy). Mirrors the inline
/// computation in `scan_peaks`/`decode_audio_file_full` so a re-derived
/// (e.g. stretched) buffer produces a matching bass lane.
pub(crate) fn compute_bass_peaks(samples: &[f32], channels: usize, sample_rate: u32) -> Vec<Vec<f32>> {
    let chunk = (sample_rate / 100).max(1) as usize;
    let chunk_f = chunk as f64;
    let total_frames = samples.len() / channels;
    let mut out: Vec<Vec<f32>> = vec![Vec::with_capacity(total_frames / chunk + 1); channels];
    let mut filters: Vec<BiquadLP> = (0..channels).map(|_| BiquadLP::new(200.0, sample_rate)).collect();
    let mut sum_sq = vec![0.0f64; channels];
    let mut prev_energy = vec![0.0f64; channels];
    let mut frames_in_chunk = 0usize;

    for f in 0..total_frames {
        for c in 0..channels {
            let bv = filters[c].process(samples[f * channels + c]) as f64;
            sum_sq[c] += bv * bv;
        }
        frames_in_chunk += 1;
        if frames_in_chunk >= chunk {
            for c in 0..channels {
                let energy = sum_sq[c] / chunk_f;
                let flux = (energy - prev_energy[c]).max(0.0);
                out[c].push(flux.sqrt() as f32);
                prev_energy[c] = energy;
                sum_sq[c] = 0.0;
            }
            frames_in_chunk = 0;
        }
    }
    if frames_in_chunk > 0 {
        for c in 0..channels {
            let energy = sum_sq[c] / frames_in_chunk as f64;
            let flux = (energy - prev_energy[c]).max(0.0);
            out[c].push(flux.sqrt() as f32);
        }
    }
    out
}

fn open_format(path: &str) -> Result<
    (Box<dyn symphonia::core::formats::FormatReader>, u32, usize, u32, f64),
    String,
> {
    let file = File::open(path).map_err(|e| format!("open: {e}"))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = std::path::Path::new(path).extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(|e| format!("probe: {e}"))?;

    let format = probed.format;
    let track = format.default_track().ok_or("no audio track")?;
    let channels = track.codec_params.channels.ok_or("no channel info")?.count();
    let sample_rate = track.codec_params.sample_rate.ok_or("no sample rate")?;
    let n_frames = track.codec_params.n_frames.unwrap_or(0);
    let duration_secs = if n_frames > 0 {
        n_frames as f64 / sample_rate as f64
    } else {
        0.0
    };
    let track_id = track.id;
    Ok((format, track_id, channels, sample_rate, duration_secs))
}

/// Fast scan: compute peaks WITHOUT storing PCM.
/// Returns within 1–3 s even for a 60-min file because it skips the 1.2 GB allocation.
pub fn scan_peaks(path: &str, mut progress_cb: impl FnMut(f32)) -> Result<AudioMeta, String> {
    let file = File::open(path).map_err(|e| format!("open: {e}"))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = std::path::Path::new(path).extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(|e| format!("probe: {e}"))?;

    let mut format = probed.format;
    let track = format.default_track().ok_or("no audio track")?;
    let track_id = track.id;
    let channels = track.codec_params.channels.ok_or("no channel info")?.count();
    let sample_rate = track.codec_params.sample_rate.ok_or("no sample rate")?;
    let n_frames_hint = track.codec_params.n_frames.unwrap_or(0) as usize;

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("decoder: {e}"))?;

    let chunk = (sample_rate / 100).max(1) as usize;
    let chunk_f = chunk as f64;
    let mut peaks: Vec<Vec<f32>> = vec![Vec::new(); channels];
    let mut bass_peaks: Vec<Vec<f32>> = vec![Vec::new(); channels];
    let mut chunk_max = vec![0.0f32; channels];
    let mut bass_filters: Vec<BiquadLP> = (0..channels)
        .map(|_| BiquadLP::new(200.0, sample_rate))
        .collect();
    let mut bass_sum_sq = vec![0.0f64; channels];
    let mut bass_prev_energy = vec![0.0f64; channels];
    let mut frames_in_chunk = 0usize;
    let mut total_frames = 0usize;

    let mut stft = StftAccumulator::new(sample_rate);

    let progress_every = (sample_rate as usize * 4).max(1); // every ~4 s
    let mut next_progress = progress_every;
    let mut sample_buf: Option<SampleBuffer<f32>> = None;

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymphoniaError::ResetRequired) => continue,
            Err(SymphoniaError::IoError(_)) => break,
            Err(e) => return Err(format!("packet: {e}")),
        };
        if packet.track_id() != track_id { continue; }

        match decoder.decode(&packet) {
            Ok(decoded) => {
                let spec = *decoded.spec();
                let buf = sample_buf.get_or_insert_with(|| {
                    SampleBuffer::<f32>::new(decoded.capacity() as u64, spec)
                });
                if decoded.frames() > buf.capacity() {
                    *buf = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
                }
                buf.copy_interleaved_ref(decoded);
                let samps = buf.samples();
                let pkt_frames = samps.len() / channels;

                for f in 0..pkt_frames {
                    let mut mono = 0.0f32;
                    for c in 0..channels {
                        let s = samps[f * channels + c];
                        let v = s.abs();
                        if v > chunk_max[c] { chunk_max[c] = v; }
                        let bv = bass_filters[c].process(s) as f64;
                        bass_sum_sq[c] += bv * bv;
                        mono += s;
                    }
                    stft.push(mono / channels as f32);

                    frames_in_chunk += 1;
                    if frames_in_chunk >= chunk {
                        for c in 0..channels {
                            peaks[c].push(chunk_max[c]);
                            chunk_max[c] = 0.0;
                            let energy = bass_sum_sq[c] / chunk_f;
                            let flux = (energy - bass_prev_energy[c]).max(0.0);
                            bass_peaks[c].push(flux.sqrt() as f32);
                            bass_prev_energy[c] = energy;
                            bass_sum_sq[c] = 0.0;
                        }
                        frames_in_chunk = 0;
                    }
                }
                total_frames += pkt_frames;

                if n_frames_hint > 0 && total_frames >= next_progress {
                    let frac = (total_frames as f32 / n_frames_hint as f32).min(1.0);
                    progress_cb(frac);
                    next_progress = total_frames + progress_every;
                }
            }
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(e) => return Err(format!("decode: {e}")),
        }
    }

    if frames_in_chunk > 0 {
        for c in 0..channels {
            peaks[c].push(chunk_max[c]);
            let energy = bass_sum_sq[c] / frames_in_chunk as f64;
            let flux = (energy - bass_prev_energy[c]).max(0.0);
            bass_peaks[c].push(flux.sqrt() as f32);
        }
    }

    progress_cb(1.0);

    let duration_secs = total_frames as f64 / sample_rate as f64;
    Ok(AudioMeta {
        channels,
        sample_rate,
        duration_secs,
        peaks,
        bass_peaks,
        spectrogram: stft.finish(),
    })
}

// ── PCM cache ─────────────────────────────────────────────────────────────
// On first open we decode (slow). We then write raw f32 samples next to the
// source file as `<path>.f32pcm`. On subsequent opens we read the cache
// (~1-3 s for a 1.3 GB file) instead of re-decoding (~60-120 s).
//
// Cache format:
//   8 bytes  magic "F32PCM02"
//   4 bytes  sample_rate (u32 LE)
//   4 bytes  channels    (u32 LE)
//   8 bytes  n_samples   (u64 LE)
//   n_samples × 4 bytes  f32 LE samples (interleaved)

const CACHE_MAGIC: &[u8; 8] = b"F32PCM02";

fn cache_path(audio_path: &str) -> String {
    format!("{}.f32pcm", audio_path)
}

pub fn pcm_cache_is_fresh(audio_path: &str) -> bool {
    cache_is_fresh(audio_path)
}

fn cache_is_fresh(audio_path: &str) -> bool {
    let audio_mtime = std::fs::metadata(audio_path).and_then(|m| m.modified()).ok();
    let cache_mtime = std::fs::metadata(cache_path(audio_path)).and_then(|m| m.modified()).ok();
    matches!((audio_mtime, cache_mtime), (Some(am), Some(cm)) if cm > am)
}

fn try_load_from_cache(
    path: &str,
    mut progress_cb: impl FnMut(f32),
) -> Option<DecodedAudio> {
    use std::io::{BufReader, Read};
    if !cache_is_fresh(path) { return None; }

    let f = std::fs::File::open(cache_path(path)).ok()?;
    let mut r = BufReader::with_capacity(8 * 1024 * 1024, f);

    let mut magic = [0u8; 8];
    r.read_exact(&mut magic).ok()?;
    if &magic != CACHE_MAGIC { return None; }

    let mut hdr = [0u8; 16];
    r.read_exact(&mut hdr).ok()?;
    let sample_rate = u32::from_le_bytes(hdr[0..4].try_into().ok()?);
    let channels    = u32::from_le_bytes(hdr[4..8].try_into().ok()?) as usize;
    let n_samples   = u64::from_le_bytes(hdr[8..16].try_into().ok()?) as usize;
    if channels == 0 || sample_rate == 0 || n_samples == 0 { return None; }

    let mut samples = vec![0f32; n_samples];

    // Read directly into the f32 buffer via a byte view — avoids a second allocation.
    // Safety: f32 is Pod; we own the buffer; length is correct.
    let byte_slice = unsafe {
        std::slice::from_raw_parts_mut(samples.as_mut_ptr() as *mut u8, n_samples * 4)
    };
    const CHUNK: usize = 8 * 1024 * 1024; // 8 MB chunks for progress reporting
    let total = byte_slice.len();
    let mut pos = 0usize;
    while pos < total {
        let end = (pos + CHUNK).min(total);
        if r.read_exact(&mut byte_slice[pos..end]).is_err() { return None; }
        pos = end;
        progress_cb(pos as f32 / total as f32);
    }

    let duration_secs = (n_samples / channels) as f64 / sample_rate as f64;
    Some(DecodedAudio { samples, channels, sample_rate, duration_secs })
}

fn write_to_cache(path: &str, audio: &DecodedAudio) {
    use std::io::{BufWriter, Write};
    // Write to a temp file then atomically rename to avoid leaving a partial cache.
    let tmp = format!("{}.f32pcm.tmp", path);
    let result = (|| -> std::io::Result<()> {
        let f = std::fs::File::create(&tmp)?;
        let mut w = BufWriter::with_capacity(8 * 1024 * 1024, f);
        w.write_all(CACHE_MAGIC)?;
        w.write_all(&audio.sample_rate.to_le_bytes())?;
        w.write_all(&(audio.channels as u32).to_le_bytes())?;
        w.write_all(&(audio.samples.len() as u64).to_le_bytes())?;
        // Write f32 slice as raw bytes — same safety rationale as the read side.
        let bytes = unsafe {
            std::slice::from_raw_parts(audio.samples.as_ptr() as *const u8, audio.samples.len() * 4)
        };
        w.write_all(bytes)?;
        w.flush()
    })();
    if result.is_ok() {
        let _ = std::fs::rename(&tmp, cache_path(path));
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Full decode to PCM. Used by stretch/rate processing.
pub fn decode_audio_file_with_progress(
    path: &str,
    mut progress_cb: impl FnMut(f32),
) -> Result<DecodedAudio, String> {
    // Fast path: return cached PCM if the source file hasn't changed.
    if let Some(cached) = try_load_from_cache(path, &mut progress_cb) {
        return Ok(cached);
    }

    let file = File::open(path).map_err(|e| format!("open: {e}"))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = std::path::Path::new(path).extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(|e| format!("probe: {e}"))?;

    let mut format = probed.format;
    let track = format.default_track().ok_or("no audio track")?;
    let track_id = track.id;
    let channels = track.codec_params.channels.ok_or("no channel info")?.count();
    let sample_rate = track.codec_params.sample_rate.ok_or("no sample rate")?;

    let expected_samples = track
        .codec_params
        .n_frames
        .map(|n| n as usize * channels)
        .unwrap_or(0);
    let mut all_samples = Vec::with_capacity(expected_samples);

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("decoder: {e}"))?;

    let mut sample_buf: Option<SampleBuffer<f32>> = None;
    let progress_interval = (sample_rate as usize * 2 * channels).max(1);
    let mut next_progress_at = progress_interval;

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymphoniaError::ResetRequired) => continue,
            Err(SymphoniaError::IoError(_)) => break,
            Err(e) => return Err(format!("packet: {e}")),
        };
        if packet.track_id() != track_id { continue; }

        match decoder.decode(&packet) {
            Ok(decoded) => {
                let spec = *decoded.spec();
                let buf = sample_buf.get_or_insert_with(|| {
                    SampleBuffer::<f32>::new(decoded.capacity() as u64, spec)
                });
                if decoded.frames() > buf.capacity() {
                    *buf = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
                }
                buf.copy_interleaved_ref(decoded);
                all_samples.extend_from_slice(buf.samples());

                if expected_samples > 0 && all_samples.len() >= next_progress_at {
                    let frac = (all_samples.len() as f32 / expected_samples as f32).min(1.0);
                    progress_cb(frac);
                    next_progress_at = all_samples.len() + progress_interval;
                }
            }
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(e) => return Err(format!("decode: {e}")),
        }
    }

    progress_cb(1.0);
    let num_frames = all_samples.len() / channels;
    let duration_secs = num_frames as f64 / sample_rate as f64;
    let audio = DecodedAudio { samples: all_samples, channels, sample_rate, duration_secs };

    // Write cache for future opens. One-time cost (~2 s I/O) after the decode.
    write_to_cache(path, &audio);

    Ok(audio)
}

pub fn decode_audio_file(path: &str) -> Result<DecodedAudio, String> {
    decode_audio_file_with_progress(path, |_| {})
}

// ── Peaks cache ────────────────────────────────────────────────────────────
// Stores peaks + bass_peaks + spectrogram bytes next to the source file as
// `<path>.peaks`.  Reading it (~10 MB) takes <100 ms, skipping the full
// scan_peaks decode (~60 s for a 62-min file) on every subsequent open.
//
// Format:
//   8 bytes  magic "PEAKS002"
//   4 bytes  sample_rate (u32 LE)
//   4 bytes  channels    (u32 LE)
//   8 bytes  duration_secs (f64 LE)
//   4 bytes  n_peaks  per channel (u32 LE)
//   4 bytes  n_bass   per channel (u32 LE)
//   4 bytes  spec_cols (u32 LE)
//   4 bytes  cols_per_sec (f32 LE)
//   1 byte   midi_lo
//   1 byte   midi_hi
//   4 bytes  raw_bins (u32 LE)
//   4 bytes  sal_bins (u32 LE)
//   channels × n_peaks × 4 bytes  peaks     (f32 LE)
//   channels × n_bass  × 4 bytes  bass_peaks (f32 LE)
//   spec_cols × raw_bins bytes     raw layer (u8)
//   spec_cols × sal_bins bytes     salience layer (u8)

const PEAKS_MAGIC: &[u8; 8] = b"PEAKS003";

fn peaks_cache_path(audio_path: &str) -> String {
    format!("{}.peaks", audio_path)
}

fn peaks_cache_is_fresh(audio_path: &str) -> bool {
    let am = std::fs::metadata(audio_path).and_then(|m| m.modified()).ok();
    let cm = std::fs::metadata(peaks_cache_path(audio_path)).and_then(|m| m.modified()).ok();
    matches!((am, cm), (Some(a), Some(c)) if c > a)
}

fn write_peaks_cache(path: &str, meta: &AudioMeta) {
    use std::io::{BufWriter, Write};
    let tmp = format!("{}.peaks.tmp", path);
    let result = (|| -> std::io::Result<()> {
        let f = std::fs::File::create(&tmp)?;
        let mut w = BufWriter::with_capacity(4 * 1024 * 1024, f);
        w.write_all(PEAKS_MAGIC)?;
        w.write_all(&meta.sample_rate.to_le_bytes())?;
        w.write_all(&(meta.channels as u32).to_le_bytes())?;
        w.write_all(&meta.duration_secs.to_le_bytes())?;
        let n_peaks = meta.peaks.first().map(|v| v.len()).unwrap_or(0) as u32;
        let n_bass  = meta.bass_peaks.first().map(|v| v.len()).unwrap_or(0) as u32;
        w.write_all(&n_peaks.to_le_bytes())?;
        w.write_all(&n_bass.to_le_bytes())?;
        let spec = &meta.spectrogram;
        w.write_all(&(spec.cols as u32).to_le_bytes())?;
        w.write_all(&spec.cols_per_sec.to_le_bytes())?;
        w.write_all(&[spec.midi_lo, spec.midi_hi])?;
        w.write_all(&(spec.raw.bins as u32).to_le_bytes())?;
        w.write_all(&(spec.salience.bins as u32).to_le_bytes())?;
        for ch in &meta.peaks {
            let bytes = unsafe {
                std::slice::from_raw_parts(ch.as_ptr() as *const u8, ch.len() * 4)
            };
            w.write_all(bytes)?;
        }
        for ch in &meta.bass_peaks {
            let bytes = unsafe {
                std::slice::from_raw_parts(ch.as_ptr() as *const u8, ch.len() * 4)
            };
            w.write_all(bytes)?;
        }
        w.write_all(&spec.raw.bytes)?;
        w.write_all(&spec.salience.bytes)?;
        w.flush()
    })();
    if result.is_ok() {
        let _ = std::fs::rename(&tmp, peaks_cache_path(path));
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}

fn try_load_peaks_cache(path: &str) -> Option<AudioMeta> {
    use std::io::{BufReader, Read};
    if !peaks_cache_is_fresh(path) { return None; }

    let f = std::fs::File::open(peaks_cache_path(path)).ok()?;
    let mut r = BufReader::with_capacity(4 * 1024 * 1024, f);

    let mut magic = [0u8; 8];
    r.read_exact(&mut magic).ok()?;
    if &magic != PEAKS_MAGIC { return None; }

    let mut b4 = [0u8; 4];
    let mut b8 = [0u8; 8];

    r.read_exact(&mut b4).ok()?; let sample_rate  = u32::from_le_bytes(b4);
    r.read_exact(&mut b4).ok()?; let channels     = u32::from_le_bytes(b4) as usize;
    r.read_exact(&mut b8).ok()?; let duration_secs = f64::from_le_bytes(b8);
    r.read_exact(&mut b4).ok()?; let n_peaks      = u32::from_le_bytes(b4) as usize;
    r.read_exact(&mut b4).ok()?; let n_bass       = u32::from_le_bytes(b4) as usize;
    r.read_exact(&mut b4).ok()?; let spec_cols    = u32::from_le_bytes(b4) as usize;
    r.read_exact(&mut b4).ok()?; let cols_per_sec = f32::from_le_bytes(b4);
    let mut b2 = [0u8; 2];
    r.read_exact(&mut b2).ok()?; let midi_lo = b2[0]; let midi_hi = b2[1];
    r.read_exact(&mut b4).ok()?; let raw_bins = u32::from_le_bytes(b4) as usize;
    r.read_exact(&mut b4).ok()?; let sal_bins = u32::from_le_bytes(b4) as usize;

    if channels == 0 || sample_rate == 0 { return None; }

    let mut peaks = vec![vec![0f32; n_peaks]; channels];
    for ch in &mut peaks {
        let bytes = unsafe {
            std::slice::from_raw_parts_mut(ch.as_mut_ptr() as *mut u8, n_peaks * 4)
        };
        r.read_exact(bytes).ok()?;
    }

    let mut bass_peaks = vec![vec![0f32; n_bass]; channels];
    for ch in &mut bass_peaks {
        let bytes = unsafe {
            std::slice::from_raw_parts_mut(ch.as_mut_ptr() as *mut u8, n_bass * 4)
        };
        r.read_exact(bytes).ok()?;
    }

    let mut raw_bytes = vec![0u8; spec_cols * raw_bins];
    r.read_exact(&mut raw_bytes).ok()?;
    let mut sal_bytes = vec![0u8; spec_cols * sal_bins];
    r.read_exact(&mut sal_bytes).ok()?;

    Some(AudioMeta {
        channels,
        sample_rate,
        duration_secs,
        peaks,
        bass_peaks,
        spectrogram: SpectrogramData {
            cols: spec_cols,
            cols_per_sec,
            midi_lo,
            midi_hi,
            raw: SpecLayer { bytes: raw_bytes, bins: raw_bins },
            salience: SpecLayer { bytes: sal_bytes, bins: sal_bins },
        },
    })
}

/// Like `scan_peaks` but reads from a sidecar cache on repeated opens.
/// First call is slow (full decode); subsequent calls return in <100 ms.
pub fn scan_peaks_cached(path: &str, mut progress_cb: impl FnMut(f32)) -> Result<AudioMeta, String> {
    if let Some(cached) = try_load_peaks_cache(path) {
        progress_cb(1.0);
        return Ok(cached);
    }
    let meta = scan_peaks(path, progress_cb)?;
    write_peaks_cache(path, &meta);
    Ok(meta)
}

/// Single-pass decode: capture PCM and compute peaks + spectrogram simultaneously.
pub fn decode_audio_file_full(
    path: &str,
    mut progress_cb: impl FnMut(f32),
) -> Result<(DecodedAudio, Vec<Vec<f32>>, Vec<Vec<f32>>, SpectrogramData), String> {
    let file = File::open(path).map_err(|e| format!("open: {e}"))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = std::path::Path::new(path).extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(|e| format!("probe: {e}"))?;

    let mut format = probed.format;
    let track = format.default_track().ok_or("no audio track")?;
    let track_id = track.id;
    let channels = track.codec_params.channels.ok_or("no channel info")?.count();
    let sample_rate = track.codec_params.sample_rate.ok_or("no sample rate")?;
    let expected_samples = track.codec_params.n_frames
        .map(|n| n as usize * channels)
        .unwrap_or(0);

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("decoder: {e}"))?;

    let chunk_frames = (sample_rate / 100).max(1) as usize;
    let chunk_f = chunk_frames as f64;
    let mut peaks: Vec<Vec<f32>> = vec![Vec::new(); channels];
    let mut bass_peaks: Vec<Vec<f32>> = vec![Vec::new(); channels];
    let mut chunk_max = vec![0.0f32; channels];
    let mut bass_filters: Vec<BiquadLP> = (0..channels)
        .map(|_| BiquadLP::new(200.0, sample_rate))
        .collect();
    let mut bass_sum_sq = vec![0.0f64; channels];
    let mut bass_prev_energy = vec![0.0f64; channels];
    let mut frames_in_chunk = 0usize;
    let mut all_samples = Vec::with_capacity(expected_samples);
    let mut stft = StftAccumulator::new(sample_rate);
    let mut sample_buf: Option<SampleBuffer<f32>> = None;
    let progress_interval = (sample_rate as usize * 2 * channels).max(1);
    let mut next_progress_at = progress_interval;

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymphoniaError::ResetRequired) => continue,
            Err(SymphoniaError::IoError(_)) => break,
            Err(e) => return Err(format!("packet: {e}")),
        };
        if packet.track_id() != track_id { continue; }

        match decoder.decode(&packet) {
            Ok(decoded) => {
                let spec = *decoded.spec();
                let buf = sample_buf.get_or_insert_with(|| {
                    SampleBuffer::<f32>::new(decoded.capacity() as u64, spec)
                });
                if decoded.frames() > buf.capacity() {
                    *buf = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
                }
                buf.copy_interleaved_ref(decoded);
                let samps = buf.samples();
                let pkt_frames = samps.len() / channels;

                for f in 0..pkt_frames {
                    let mut mono = 0.0f32;
                    for c in 0..channels {
                        let s = samps[f * channels + c];
                        let v = s.abs();
                        if v > chunk_max[c] { chunk_max[c] = v; }
                        let bv = bass_filters[c].process(s) as f64;
                        bass_sum_sq[c] += bv * bv;
                        mono += s;
                    }
                    stft.push(mono / channels as f32);

                    frames_in_chunk += 1;
                    if frames_in_chunk >= chunk_frames {
                        for c in 0..channels {
                            peaks[c].push(chunk_max[c]); chunk_max[c] = 0.0;
                            let energy = bass_sum_sq[c] / chunk_f;
                            let flux = (energy - bass_prev_energy[c]).max(0.0);
                            bass_peaks[c].push(flux.sqrt() as f32);
                            bass_prev_energy[c] = energy;
                            bass_sum_sq[c] = 0.0;
                        }
                        frames_in_chunk = 0;
                    }
                }
                all_samples.extend_from_slice(samps);

                if expected_samples > 0 && all_samples.len() >= next_progress_at {
                    let frac = (all_samples.len() as f32 / expected_samples as f32).min(1.0);
                    progress_cb(frac);
                    next_progress_at = all_samples.len() + progress_interval;
                }
            }
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(e) => return Err(format!("decode: {e}")),
        }
    }

    if frames_in_chunk > 0 {
        for c in 0..channels {
            peaks[c].push(chunk_max[c]);
            let energy = bass_sum_sq[c] / frames_in_chunk as f64;
            let flux = (energy - bass_prev_energy[c]).max(0.0);
            bass_peaks[c].push(flux.sqrt() as f32);
        }
    }
    progress_cb(1.0);

    let num_frames = all_samples.len() / channels;
    let duration_secs = num_frames as f64 / sample_rate as f64;
    Ok((DecodedAudio { samples: all_samples, channels, sample_rate, duration_secs }, peaks, bass_peaks, stft.finish()))
}

/// Estimated PCM size in bytes for a file, based on codec metadata.
/// Returns None if n_frames is unknown.
pub fn estimated_pcm_bytes(path: &str) -> Option<usize> {
    let file = File::open(path).ok()?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = std::path::Path::new(path).extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .ok()?;
    let track = probed.format.default_track()?;
    let n_frames = track.codec_params.n_frames? as usize;
    let channels = track.codec_params.channels?.count();
    Some(n_frames * channels * 4)
}

#[cfg(test)]
mod spec_tests {
    use super::*;

    // Build a harmonic tone (fundamental + decaying overtones) and confirm the
    // salience layer concentrates it onto the fundamental's pitch bin while the
    // raw layer spreads energy across every harmonic.
    #[test]
    fn salience_collapses_harmonics_to_fundamental() {
        let sr = 44100u32;
        let f0 = 220.0f32; // A3 = MIDI 57
        let mut acc = StftAccumulator::new(sr);
        let n = sr as usize * 2; // 2 seconds
        for i in 0..n {
            let t = i as f32 / sr as f32;
            let mut s = 0.0;
            for h in 1..=6 {
                s += (1.0 / h as f32) * (2.0 * std::f32::consts::PI * f0 * h as f32 * t).sin();
            }
            acc.push(s * 0.2);
        }
        let spec = acc.finish();
        assert!(spec.cols > 10, "expected multiple columns");

        // Inspect a column from the steady middle of the tone.
        let col = spec.cols / 2;
        let sal = &spec.salience;
        let sb = &sal.bytes[col * sal.bins..(col + 1) * sal.bins];
        let (sal_peak_bin, _) = sb.iter().enumerate().max_by_key(|(_, &v)| v).unwrap();
        let expected = (57 - MIDI_LO) as usize; // A3
        assert!(
            (sal_peak_bin as i32 - expected as i32).abs() <= 1,
            "salience peak at bin {sal_peak_bin}, expected ~{expected} (A3)"
        );

        // The salience peak should dominate the octave-above bin. Harmonic-sum
        // salience has inherent octave ambiguity (A4 catches A3's even
        // harmonics), so we require a clear margin, not total suppression.
        let octave_up = expected + 12;
        assert!(
            sb[sal_peak_bin] as i32 - sb[octave_up] as i32 > 12,
            "fundamental ({}) should dominate octave-up ({})",
            sb[sal_peak_bin], sb[octave_up]
        );

        // Raw layer: count distinct bright peaks — a harmonic stack lights several.
        let raw = &spec.raw;
        let rb = &raw.bytes[col * raw.bins..(col + 1) * raw.bins];
        let max_raw = *rb.iter().max().unwrap();
        let bright = rb.iter().filter(|&&v| v as u16 + 30 > max_raw as u16).count();
        assert!(bright >= 3, "raw layer should show several harmonic peaks, got {bright}");
    }

    // compute_bass_peaks should pass low frequencies (and react to their onset)
    // while rejecting high frequencies — so a re-derived stretched buffer yields
    // a bass lane matching the decode-time one.
    #[test]
    fn bass_peaks_pass_low_reject_high() {
        let sr = 44100u32;
        let n = sr as usize * 2;
        let tone = |freq: f32| -> Vec<f32> {
            (0..n).map(|i| {
                // first second silent, second second a tone → clear onset
                if i < n / 2 { 0.0 }
                else { (2.0 * std::f32::consts::PI * freq * i as f32 / sr as f32).sin() * 0.5 }
            }).collect()
        };

        let low = compute_bass_peaks(&tone(80.0), 1, sr);
        let high = compute_bass_peaks(&tone(5000.0), 1, sr);
        let low_max = low[0].iter().cloned().fold(0.0f32, f32::max);
        let high_max = high[0].iter().cloned().fold(0.0f32, f32::max);

        // 80 Hz passes the 200 Hz low-pass; 5 kHz is strongly attenuated.
        assert!(low_max > high_max * 5.0,
            "low ({low_max}) should dominate high ({high_max})");

        // The onset (chunk near the midpoint) should be the brightest part.
        let mid_chunk = low[0].len() / 2;
        let onset = low[0][mid_chunk.saturating_sub(2)..(mid_chunk + 3).min(low[0].len())]
            .iter().cloned().fold(0.0f32, f32::max);
        assert!(onset > low_max * 0.5, "onset flux should be prominent");
    }
}
