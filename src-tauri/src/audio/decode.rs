use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use std::fs::File;

pub struct DecodedAudio {
    pub samples: Vec<f32>,
    pub channels: usize,
    pub sample_rate: u32,
    pub duration_secs: f64,
}

pub struct AudioMeta {
    pub channels: usize,
    pub sample_rate: u32,
    pub duration_secs: f64,
    pub peaks: Vec<Vec<f32>>,
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
    let mut peaks: Vec<Vec<f32>> = vec![Vec::new(); channels];
    let mut chunk_max = vec![0.0f32; channels];
    let mut frames_in_chunk = 0usize;
    let mut total_frames = 0usize;

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
                    for c in 0..channels {
                        let v = samps[f * channels + c].abs();
                        if v > chunk_max[c] { chunk_max[c] = v; }
                    }
                    frames_in_chunk += 1;
                    if frames_in_chunk >= chunk {
                        for c in 0..channels {
                            peaks[c].push(chunk_max[c]);
                            chunk_max[c] = 0.0;
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

    // Flush the last partial chunk.
    if frames_in_chunk > 0 {
        for c in 0..channels {
            peaks[c].push(chunk_max[c]);
        }
    }

    progress_cb(1.0);

    let duration_secs = total_frames as f64 / sample_rate as f64;
    Ok(AudioMeta { channels, sample_rate, duration_secs, peaks })
}

/// Full decode to PCM. Used by stretch/rate processing.
pub fn decode_audio_file_with_progress(
    path: &str,
    mut progress_cb: impl FnMut(f32),
) -> Result<DecodedAudio, String> {
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

    Ok(DecodedAudio { samples: all_samples, channels, sample_rate, duration_secs })
}

pub fn decode_audio_file(path: &str) -> Result<DecodedAudio, String> {
    decode_audio_file_with_progress(path, |_| {})
}
