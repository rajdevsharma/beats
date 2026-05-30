//! Minimal FFI bindings to the Rubber Band C API.
//! Install with: brew install rubberband

#![allow(non_camel_case_types, dead_code)]

use std::ffi::c_void;

type RubberBandState = *mut c_void;

// Options used for offline high-quality mode
pub const OPT_OFFLINE: i32 = 0x00000000; // RubberBandOptionProcessOffline
pub const OPT_REALTIME: i32 = 0x00000001; // RubberBandOptionProcessRealTime
pub const OPT_ENGINE_FINER: i32 = 0x20000000; // R3 engine — best quality
pub const OPT_ENGINE_FASTER: i32 = 0x00000000; // R2 engine — lower latency

#[link(name = "rubberband")]
extern "C" {
    fn rubberband_new(
        sample_rate: u32,
        channels: u32,
        options: i32,
        initial_time_ratio: f64,
        initial_pitch_scale: f64,
    ) -> RubberBandState;

    fn rubberband_delete(state: RubberBandState);

    fn rubberband_set_time_ratio(state: RubberBandState, ratio: f64);

    fn rubberband_study(
        state: RubberBandState,
        input: *const *const f32,
        samples: u32,
        final_block: i32,
    );

    fn rubberband_process(
        state: RubberBandState,
        input: *const *const f32,
        samples: u32,
        final_block: i32,
    );

    fn rubberband_available(state: RubberBandState) -> i32;

    fn rubberband_retrieve(
        state: RubberBandState,
        output: *const *mut f32,
        samples: u32,
    ) -> u32;
}

pub struct Stretcher {
    state: RubberBandState,
    pub channels: usize,
}

// Safety: Stretcher is only used from a single thread (Tauri blocking task).
unsafe impl Send for Stretcher {}

impl Stretcher {
    pub fn new(sample_rate: u32, channels: usize, options: i32, time_ratio: f64) -> Self {
        let state = unsafe {
            rubberband_new(sample_rate, channels as u32, options, time_ratio, 1.0)
        };
        Self { state, channels }
    }

    pub fn set_time_ratio(&mut self, ratio: f64) {
        unsafe { rubberband_set_time_ratio(self.state, ratio); }
    }

    /// Study the entire segment (offline mode only — improves quality).
    pub fn study(&mut self, channels_data: &[Vec<f32>]) {
        let ptrs: Vec<*const f32> = channels_data.iter().map(|v| v.as_ptr()).collect();
        let frames = channels_data[0].len();
        unsafe { rubberband_study(self.state, ptrs.as_ptr(), frames as u32, 1); }
    }

    /// Process the entire segment.
    pub fn process(&mut self, channels_data: &[Vec<f32>]) {
        let ptrs: Vec<*const f32> = channels_data.iter().map(|v| v.as_ptr()).collect();
        let frames = channels_data[0].len();
        unsafe { rubberband_process(self.state, ptrs.as_ptr(), frames as u32, 1); }
    }

    /// Drain all available output samples into interleaved Vec<f32>.
    pub fn drain_interleaved(&mut self) -> Vec<f32> {
        let ch = self.channels;
        let mut out = Vec::new();
        loop {
            let avail = unsafe { rubberband_available(self.state) };
            if avail <= 0 { break; }
            let n = avail as usize;
            let mut bufs: Vec<Vec<f32>> = vec![vec![0f32; n]; ch];
            let ptrs: Vec<*mut f32> = bufs.iter_mut().map(|v| v.as_mut_ptr()).collect();
            let got = unsafe {
                rubberband_retrieve(self.state, ptrs.as_ptr(), n as u32)
            } as usize;
            for f in 0..got {
                for c in 0..ch {
                    out.push(bufs[c][f]);
                }
            }
        }
        out
    }
}

impl Drop for Stretcher {
    fn drop(&mut self) {
        unsafe { rubberband_delete(self.state); }
    }
}

/// High-quality offline time stretch of an interleaved f32 slice.
/// `time_ratio` > 1 = slower, < 1 = faster. Pitch is preserved.
pub fn stretch_offline(
    interleaved: &[f32],
    channels: usize,
    sample_rate: u32,
    time_ratio: f64,
) -> Vec<f32> {
    let frames = interleaved.len() / channels;

    // Deinterleave
    let channel_data: Vec<Vec<f32>> = (0..channels)
        .map(|c| (0..frames).map(|f| interleaved[f * channels + c]).collect())
        .collect();

    let options = OPT_OFFLINE | OPT_ENGINE_FINER;
    let mut s = Stretcher::new(sample_rate, channels, options, time_ratio);

    s.study(&channel_data);
    s.process(&channel_data);
    s.drain_interleaved()
}
