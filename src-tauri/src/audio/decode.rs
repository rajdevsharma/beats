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

pub struct SpectrogramData {
    pub data_b64: String,  // base64 of flat u8 [col * bins + bin]
    pub cols: usize,
    pub bins: usize,
    pub cols_per_sec: f32,
    raw: Vec<u8>,          // original bytes, kept for peaks-cache writing
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

fn to_base64(data: &[u8]) -> String {
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

const STFT_SIZE: usize = 2048;
const STFT_BINS: usize = 64;

struct StftAccumulator {
    hop: usize,
    cols_per_sec: f32,
    hann: Vec<f32>,
    log_bin_idxs: Vec<usize>,
    ring: Vec<f32>,           // circular, length = STFT_SIZE
    ring_write: usize,        // index of next write slot (= oldest slot to read)
    frames_since_hop: usize,
    fft_plan: std::sync::Arc<dyn rustfft::Fft<f32>>,
    fft_input: Vec<Complex<f32>>,
    fft_scratch: Vec<Complex<f32>>,
    cols: Vec<u8>,            // flat: [col * STFT_BINS + bin]
    col_count: usize,
}

impl StftAccumulator {
    fn new(sample_rate: u32) -> Self {
        let hop = (sample_rate / 20).max(1) as usize; // 20 cols/sec
        let cols_per_sec = sample_rate as f32 / hop as f32;

        let hann: Vec<f32> = (0..STFT_SIZE)
            .map(|i| 0.5 * (1.0 - (2.0 * std::f32::consts::PI * i as f32
                / (STFT_SIZE - 1) as f32).cos()))
            .collect();

        // Log-spaced frequency bins from 40 Hz to 8000 Hz
        let log_bin_idxs: Vec<usize> = (0..STFT_BINS)
            .map(|b| {
                let f = 40.0_f32 * (8000.0_f32 / 40.0_f32)
                    .powf(b as f32 / (STFT_BINS - 1) as f32);
                let idx = (f * STFT_SIZE as f32 / sample_rate as f32).round() as usize;
                idx.min(STFT_SIZE / 2 - 1)
            })
            .collect();

        let mut planner = FftPlanner::<f32>::new();
        let fft_plan = planner.plan_fft_forward(STFT_SIZE);
        let scratch_len = fft_plan.get_inplace_scratch_len();

        StftAccumulator {
            hop,
            cols_per_sec,
            hann,
            log_bin_idxs,
            ring: vec![0.0; STFT_SIZE],
            ring_write: 0,
            frames_since_hop: 0,
            fft_plan,
            fft_input: vec![Complex::default(); STFT_SIZE],
            fft_scratch: vec![Complex::default(); scratch_len],
            cols: Vec::new(),
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

    fn emit_col(&mut self) {
        // Build windowed FFT input from ring buffer (oldest sample first)
        let start = self.ring_write; // oldest position
        for i in 0..STFT_SIZE {
            let s = self.ring[(start + i) % STFT_SIZE];
            self.fft_input[i] = Complex { re: s * self.hann[i], im: 0.0 };
        }
        self.fft_plan.process_with_scratch(&mut self.fft_input, &mut self.fft_scratch);

        // Normalize: peak magnitude ≈ STFT_SIZE/2 for full-scale signal
        let norm = (STFT_SIZE as f32) / 2.0;
        for &bi in &self.log_bin_idxs {
            let mag = self.fft_input[bi].norm();
            let db = 20.0 * (mag / norm).max(1e-10_f32).log10();
            // Per-bin energy in orchestral music typically sits at -60 to -20 dB.
            // Map that range to [0, 1] then apply gamma 0.35 to spread mid-level
            // content across the full color palette instead of clustering at the low end.
            let v = ((db + 60.0) / 60.0).clamp(0.0, 1.0).powf(0.35);
            self.cols.push((v * 255.0) as u8);
        }
        self.col_count += 1;
    }

    fn finish(self) -> SpectrogramData {
        let data_b64 = to_base64(&self.cols);
        SpectrogramData {
            data_b64,
            cols: self.col_count,
            bins: STFT_BINS,
            cols_per_sec: self.cols_per_sec,
            raw: self.cols,
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
//   4 bytes  spec_bins (u32 LE)
//   4 bytes  cols_per_sec (f32 LE)
//   channels × n_peaks × 4 bytes  peaks     (f32 LE)
//   channels × n_bass  × 4 bytes  bass_peaks (f32 LE)
//   spec_cols × spec_bins bytes    spectrogram (u8)

const PEAKS_MAGIC: &[u8; 8] = b"PEAKS002";

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
        w.write_all(&(meta.spectrogram.cols as u32).to_le_bytes())?;
        w.write_all(&(meta.spectrogram.bins as u32).to_le_bytes())?;
        w.write_all(&meta.spectrogram.cols_per_sec.to_le_bytes())?;
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
        w.write_all(&meta.spectrogram.raw)?;
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
    r.read_exact(&mut b4).ok()?; let spec_bins    = u32::from_le_bytes(b4) as usize;
    r.read_exact(&mut b4).ok()?; let cols_per_sec = f32::from_le_bytes(b4);

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

    let spec_len = spec_cols * spec_bins;
    let mut raw = vec![0u8; spec_len];
    r.read_exact(&mut raw).ok()?;
    let data_b64 = to_base64(&raw);

    Some(AudioMeta {
        channels,
        sample_rate,
        duration_secs,
        peaks,
        bass_peaks,
        spectrogram: SpectrogramData { data_b64, cols: spec_cols, bins: spec_bins, cols_per_sec, raw },
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
