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

/// Decode an audio file to interleaved f32 PCM.
/// `progress_cb` is called with a 0.0–1.0 fraction as decoding proceeds.
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

    // Pre-allocate using n_frames if available — avoids repeated reallocs for long files.
    let expected_samples = track
        .codec_params
        .n_frames
        .map(|n| n as usize * channels)
        .unwrap_or(0);
    let mut all_samples: Vec<f32> = Vec::with_capacity(expected_samples);

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("decoder: {e}"))?;

    // Reuse SampleBuffer across packets to avoid per-packet heap allocation.
    let mut sample_buf: Option<SampleBuffer<f32>> = None;

    // Emit progress roughly every 2 seconds of decoded audio.
    let progress_interval = (sample_rate as usize * 2 * channels).max(1);
    let mut next_progress_at = progress_interval;

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymphoniaError::ResetRequired) => continue,
            Err(SymphoniaError::IoError(_)) => break, // EOF
            Err(e) => return Err(format!("packet: {e}")),
        };
        if packet.track_id() != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(decoded) => {
                let spec = *decoded.spec();
                let buf = sample_buf.get_or_insert_with(|| {
                    SampleBuffer::<f32>::new(decoded.capacity() as u64, spec)
                });
                // Grow the buffer if a packet is larger than expected (rare).
                if decoded.frames() > buf.capacity() {
                    *buf = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
                }
                buf.copy_interleaved_ref(decoded);
                all_samples.extend_from_slice(buf.samples());

                // Emit progress based on decoded fraction vs expected total.
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

    Ok(DecodedAudio {
        samples: all_samples,
        channels,
        sample_rate,
        duration_secs,
    })
}

/// Convenience wrapper with no progress reporting.
pub fn decode_audio_file(path: &str) -> Result<DecodedAudio, String> {
    decode_audio_file_with_progress(path, |_| {})
}
