use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use std::fs::File;

pub struct DecodedAudio {
    /// Interleaved f32 PCM samples (L R L R … for stereo).
    pub samples: Vec<f32>,
    pub channels: usize,
    pub sample_rate: u32,
    pub duration_secs: f64,
}

pub fn decode_audio_file(path: &str) -> Result<DecodedAudio, String> {
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
    let track = format
        .default_track()
        .ok_or("no audio track")?;

    let track_id = track.id;
    let channels = track
        .codec_params
        .channels
        .ok_or("no channel info")?
        .count();
    let sample_rate = track.codec_params.sample_rate.ok_or("no sample rate")?;

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("decoder: {e}"))?;

    let mut all_samples: Vec<f32> = Vec::new();

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymphoniaError::ResetRequired) => continue,
            Err(SymphoniaError::IoError(_)) => break,
            Err(e) => return Err(format!("packet: {e}")),
        };
        if packet.track_id() != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(decoded) => {
                let spec = *decoded.spec();
                let mut buf = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
                buf.copy_interleaved_ref(decoded);
                all_samples.extend_from_slice(buf.samples());
            }
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(e) => return Err(format!("decode: {e}")),
        }
    }

    let num_frames = all_samples.len() / channels;
    let duration_secs = num_frames as f64 / sample_rate as f64;

    Ok(DecodedAudio {
        samples: all_samples,
        channels,
        sample_rate,
        duration_secs,
    })
}
