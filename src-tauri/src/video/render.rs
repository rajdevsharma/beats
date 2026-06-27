// Offline Synthesia-style piano-roll frame renderer.
//
// Every frame is a pure function of (scene, time): particles and key lights
// are computed analytically from note onset times with per-note seeded RNG,
// so frames can be rendered in parallel in any order.

use tiny_skia::{
    BlendMode, Color, FillRule, GradientStop, LineCap, LinearGradient, Paint, PathBuilder, Pixmap,
    Point, RadialGradient, Rect, SpreadMode, Stroke, Transform,
};

pub struct SceneNote {
    pub start: f64, // seconds, clip-relative (0 = first video frame)
    pub end: f64,
    pub pitch: u8,
    pub vel: f32,
    pub track: usize,
}

pub struct SceneTrack {
    pub color: [u8; 3],
    pub is_piano: bool,
}

pub struct Scene {
    pub width: u32,
    pub height: u32,
    pub horizontal: bool,
    notes: Vec<SceneNote>,
    tracks: Vec<SceneTrack>,
    beats: Vec<f64>, // clip-relative seconds
    // Layout
    lookahead: f64,    // seconds visible ahead of the hit line
    pps: f32,          // pixels per second along the scroll axis
    kb_size: f32,      // keyboard strip size (height in vertical mode, width in horizontal)
    hit: f32,          // hit-line coordinate (y in vertical, x in horizontal)
    key_extent: f32,   // pixel extent across keys (width in vertical, height in horizontal)
    // Per-second buckets of note indices that need drawing during that second
    buckets: Vec<Vec<u32>>,
    // Peripheral-vision performance cues (independently toggleable)
    beat_pulse: bool,
    orchestra_bars: bool,
    tempo_pendulum: bool,
    orch_norm: f32, // normalizer for orchestral concurrent-energy → [0,1]
    // Orientation cues (independently toggleable)
    progress_bar: bool,
    next_note_cue: bool,
    countdown_pips: bool,
    clip_dur: f64,             // total clip length, for the progress bar
    piano_onsets: Vec<f64>,    // sorted distinct piano onset times (for cues)
}

const PARTICLE_MAX_LIFE: f64 = 0.85;
const FLASH_LIFE: f64 = 0.22;
/// Keyboard key-light color — keys light up pink, pianoforte part only.
const KEY_PINK: [u8; 3] = [255, 105, 175];

// Piano-part effect tuning (Rousseau-style: beams, ember fountains, sheen).
const EMIT_DT: f32 = 0.045; // seconds between ember emissions per held note
const SPARK_LIFE: f32 = 0.8; // max ember lifetime
const BEAM_FADE_OUT: f64 = 0.35; // beam linger after note release
const PIANO_ROSE: [u8; 3] = [255, 170, 205]; // rose accent matching the key lights
const PIANO_WARM: [u8; 3] = [255, 240, 224]; // warm white ember core
const LO_PITCH: i32 = 21; // A0
const HI_PITCH: i32 = 108; // C8
const N_WHITE: f32 = 52.0;
const BLACK_PCS: [bool; 12] = [
    false, true, false, true, false, false, true, false, true, false, true, false,
];

fn is_black(pitch: i32) -> bool {
    BLACK_PCS[(pitch.rem_euclid(12)) as usize]
}

/// Number of white keys strictly below `pitch`, counting from A0.
fn white_index(pitch: i32) -> i32 {
    let mut n = 0;
    let mut p = LO_PITCH;
    while p < pitch {
        if !is_black(p) {
            n += 1;
        }
        p += 1;
    }
    n
}

/// Position and size of a key along the keyboard axis, in [0, key_extent].
/// Returns (offset, size). Black keys get small per-group offsets so the
/// keyboard reads like a real piano rather than a uniform grid.
fn key_span(pitch: i32, key_extent: f32) -> (f32, f32) {
    let wkw = key_extent / N_WHITE;
    if !is_black(pitch) {
        (white_index(pitch) as f32 * wkw, wkw)
    } else {
        let boundary = white_index(pitch + 1) as f32 * wkw;
        let shift = match pitch.rem_euclid(12) {
            1 => -0.10,  // C#
            3 => 0.10,   // D#
            6 => -0.12,  // F#
            10 => 0.12,  // A#
            _ => 0.0,    // G#
        } * wkw;
        let bw = wkw * 0.58;
        (boundary + shift - bw / 2.0, bw)
    }
}

// ── Small color / RNG helpers ───────────────────────────────────────────────

fn rgba(c: [u8; 3], a: f32) -> Color {
    Color::from_rgba8(c[0], c[1], c[2], (a.clamp(0.0, 1.0) * 255.0) as u8)
}

fn lighten(c: [u8; 3], f: f32) -> [u8; 3] {
    [
        (c[0] as f32 + (255.0 - c[0] as f32) * f) as u8,
        (c[1] as f32 + (255.0 - c[1] as f32) * f) as u8,
        (c[2] as f32 + (255.0 - c[2] as f32) * f) as u8,
    ]
}

fn darken(c: [u8; 3], f: f32) -> [u8; 3] {
    [
        (c[0] as f32 * (1.0 - f)) as u8,
        (c[1] as f32 * (1.0 - f)) as u8,
        (c[2] as f32 * (1.0 - f)) as u8,
    ]
}

/// splitmix64 — deterministic per-note randomness.
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E3779B97F4A7C15).wrapping_add(1))
    }
    fn next(&mut self) -> f32 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        ((z ^ (z >> 31)) >> 40) as f32 / (1u64 << 24) as f32
    }
    /// uniform in [lo, hi)
    fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + self.next() * (hi - lo)
    }
}

fn round_rect(pb: &mut PathBuilder, x: f32, y: f32, w: f32, h: f32, r: f32) {
    let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
    if r < 0.5 {
        if let Some(rect) = Rect::from_xywh(x, y, w, h) {
            pb.push_rect(rect);
        }
        return;
    }
    let k = 0.5523 * r;
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.cubic_to(x + w - r + k, y, x + w, y + r - k, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.cubic_to(x + w, y + h - r + k, x + w - r + k, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.cubic_to(x + r - k, y + h, x, y + h - r + k, x, y + h - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    pb.close();
}

fn fill_round_rect(pm: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, r: f32, paint: &Paint) {
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    let mut pb = PathBuilder::new();
    round_rect(&mut pb, x, y, w, h, r);
    if let Some(path) = pb.finish() {
        pm.fill_path(&path, paint, FillRule::Winding, Transform::identity(), None);
    }
}

fn fill_rect(pm: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, paint: &Paint) {
    if let Some(rect) = Rect::from_xywh(x, y, w.max(0.01), h.max(0.01)) {
        pm.fill_rect(rect, paint, Transform::identity(), None);
    }
}

fn solid(color: Color, blend: BlendMode) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color(color);
    p.blend_mode = blend;
    p.anti_alias = true;
    p
}

fn linear_grad(
    from: (f32, f32),
    to: (f32, f32),
    stops: Vec<GradientStop>,
    blend: BlendMode,
) -> Option<Paint<'static>> {
    let shader = LinearGradient::new(
        Point::from_xy(from.0, from.1),
        Point::from_xy(to.0, to.1),
        stops,
        SpreadMode::Pad,
        Transform::identity(),
    )?;
    let mut p = Paint::default();
    p.shader = shader;
    p.blend_mode = blend;
    p.anti_alias = true;
    Some(p)
}

/// Soft additive glow disc at (cx, cy).
fn glow_disc(pm: &mut Pixmap, cx: f32, cy: f32, radius: f32, color: [u8; 3], alpha: f32) {
    if radius <= 0.3 || alpha <= 0.003 {
        return;
    }
    let shader = RadialGradient::new(
        Point::from_xy(cx, cy),
        Point::from_xy(cx, cy),
        radius,
        vec![
            GradientStop::new(0.0, rgba(color, alpha)),
            GradientStop::new(0.35, rgba(color, alpha * 0.55)),
            GradientStop::new(0.7, rgba(color, alpha * 0.18)),
            GradientStop::new(1.0, rgba(color, 0.0)),
        ],
        SpreadMode::Pad,
        Transform::identity(),
    );
    let Some(shader) = shader else { return };
    let mut paint = Paint::default();
    paint.shader = shader;
    paint.blend_mode = BlendMode::Plus;
    paint.anti_alias = true;
    let mut pb = PathBuilder::new();
    pb.push_circle(cx, cy, radius);
    if let Some(path) = pb.finish() {
        pm.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
    }
}

impl Scene {
    pub fn new(
        notes: Vec<SceneNote>,
        tracks: Vec<SceneTrack>,
        mut beats: Vec<f64>,
        width: u32,
        height: u32,
        horizontal: bool,
        clip_dur: f64,
        beat_pulse: bool,
        orchestra_bars: bool,
        tempo_pendulum: bool,
        progress_bar: bool,
        next_note_cue: bool,
        countdown_pips: bool,
    ) -> Scene {
        let (kb_size, hit, key_extent, lookahead) = if horizontal {
            let kb = (width as f32 * 0.085).clamp(110.0, 220.0);
            (kb, kb, height as f32, 6.5)
        } else {
            let kb = (height as f32 * 0.17).clamp(100.0, 260.0);
            (kb, height as f32 - (height as f32 * 0.17).clamp(100.0, 260.0), width as f32, 3.4)
        };
        let scroll_extent = if horizontal {
            width as f32 - kb_size
        } else {
            hit
        };
        let pps = scroll_extent / lookahead as f32;
        beats.sort_by(|a, b| a.partial_cmp(b).unwrap());

        // Normalizer for the orchestra energy bars: peak concurrent non-piano
        // velocity-sum across the clip, via a +start/-end sweep.
        let mut events: Vec<(f64, f32)> = Vec::new();
        for n in &notes {
            if !tracks[n.track].is_piano {
                events.push((n.start, n.vel));
                events.push((n.end, -n.vel));
            }
        }
        events.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        let (mut running, mut orch_norm) = (0.0f32, 1e-3f32);
        for (_, dv) in &events {
            running += dv;
            if running > orch_norm { orch_norm = running; }
        }

        // Distinct piano onset times (sorted) for the next-note + countdown cues.
        let mut piano_onsets: Vec<f64> = notes
            .iter()
            .filter(|n| tracks[n.track].is_piano)
            .map(|n| n.start)
            .collect();
        piano_onsets.sort_by(|a, b| a.partial_cmp(b).unwrap());
        piano_onsets.dedup_by(|a, b| (*a - *b).abs() < 0.03);

        // Bucket notes by integer second of "needs drawing" interval:
        // visible from (start - lookahead) until particles finish (end + life).
        let n_buckets = (clip_dur.ceil() as usize + 2).max(1);
        let mut buckets: Vec<Vec<u32>> = vec![Vec::new(); n_buckets];
        for (i, n) in notes.iter().enumerate() {
            let from = (n.start - lookahead - 0.5).floor().max(0.0) as usize;
            let to = ((n.end + PARTICLE_MAX_LIFE + 0.5).ceil() as usize).min(n_buckets - 1);
            for b in from..=to {
                buckets[b].push(i as u32);
            }
        }

        Scene {
            width,
            height,
            horizontal,
            notes,
            tracks,
            beats,
            lookahead,
            pps,
            kb_size,
            hit,
            key_extent,
            buckets,
            beat_pulse,
            orchestra_bars,
            tempo_pendulum,
            orch_norm,
            progress_bar,
            next_note_cue,
            countdown_pips,
            clip_dur,
            piano_onsets,
        }
    }

    /// Map clip time offset (note_time - now) to the scroll-axis pixel coordinate.
    /// Vertical: returns y (hit line at bottom of fall area, future above).
    /// Horizontal: returns x (hit line at left, future to the right).
    fn time_to_axis(&self, dt: f64) -> f32 {
        if self.horizontal {
            self.hit + dt as f32 * self.pps
        } else {
            self.hit - dt as f32 * self.pps
        }
    }

    pub fn render_frame(&self, t: f64) -> Vec<u8> {
        let mut pm = Pixmap::new(self.width, self.height).expect("pixmap");
        self.draw_background(&mut pm);
        self.draw_beat_lines(&mut pm, t);

        let active: &[u32] = self
            .buckets
            .get(t.max(0.0) as usize)
            .map(|v| v.as_slice())
            .unwrap_or(&[]);

        // Notes: non-piano first (additive), then light beams under the piano
        // notes, piano on top (always visible).
        self.draw_notes(&mut pm, t, active, false);
        self.draw_piano_beams(&mut pm, t, active);
        self.draw_notes(&mut pm, t, active, true);

        self.draw_hit_line(&mut pm, t);
        self.draw_keyboard(&mut pm, t, active);
        self.draw_particles(&mut pm, t, active);
        self.draw_conductor(&mut pm, t);

        // Peripheral-vision performance cues, drawn on top (edge-anchored, big).
        if self.orchestra_bars { self.draw_orchestra_bars(&mut pm, t, active); }
        if self.tempo_pendulum { self.draw_tempo_pendulum(&mut pm, t); }
        // Orientation cues.
        if self.next_note_cue { self.draw_next_note_cue(&mut pm, t, active); }
        if self.countdown_pips { self.draw_countdown_pips(&mut pm, t, active); }
        if self.beat_pulse { self.draw_beat_pulse(&mut pm, t); }
        if self.progress_bar { self.draw_progress_bar(&mut pm, t); }

        pm.take()
    }

    // ── Note geometry (shared by orientation cues) ──────────────────────────
    // Screen rect of a note at frame time t, clamped at the hit line — mirrors
    // the geometry in draw_notes.
    fn note_box(&self, pitch: u8, start: f64, end: f64, t: f64) -> (f32, f32, f32, f32) {
        let (lane, lane_w) = key_span(pitch as i32, self.key_extent);
        let head = self.time_to_axis(start - t);
        let tail = self.time_to_axis(end - t);
        if self.horizontal {
            let x0 = head.max(self.hit);
            let x1 = tail.min(self.width as f32 + 20.0);
            let ky = self.key_extent - lane - lane_w;
            (x0, ky + 0.5, (x1 - x0).max(2.0), (lane_w - 1.0).max(1.5))
        } else {
            let y1 = head.min(self.hit);
            let y0 = tail.max(-20.0);
            (lane + 0.5, y0, (lane_w - 1.0).max(1.5), (y1 - y0).max(2.0))
        }
    }

    // ── Progress bar (macro orientation: where in the piece) ────────────────
    fn draw_progress_bar(&self, pm: &mut Pixmap, t: f64) {
        if self.clip_dur <= 0.0 { return; }
        let w = self.width as f32;
        let frac = (t / self.clip_dur).clamp(0.0, 1.0) as f32;
        let pad = w * 0.04;
        let bw = w - pad * 2.0;
        let y = 14.0;
        let h = 9.0;
        // Track
        fill_round_rect(pm, pad, y, bw, h, h / 2.0, &solid(Color::from_rgba8(255, 255, 255, 32), BlendMode::SourceOver));
        // Coarse structure ticks (eighths of the clip)
        for i in 1..8 {
            let tx = pad + bw * (i as f32 / 8.0);
            fill_rect(pm, tx - 0.5, y - 2.0, 1.0, h + 4.0, &solid(Color::from_rgba8(255, 255, 255, 40), BlendMode::SourceOver));
        }
        // Fill
        let fillw = bw * frac;
        if fillw > 1.0 {
            if let Some(p) = linear_grad(
                (pad, 0.0), (pad + bw, 0.0),
                vec![
                    GradientStop::new(0.0, rgba([90, 200, 255], 0.85)),
                    GradientStop::new(1.0, rgba([150, 230, 255], 0.95)),
                ],
                BlendMode::SourceOver,
            ) {
                fill_round_rect(pm, pad, y, fillw, h, h / 2.0, &p);
            }
        }
        // Playhead knob
        let hx = pad + fillw;
        glow_disc(pm, hx, y + h / 2.0, 16.0, [150, 230, 255], 0.5);
        let mut pb = PathBuilder::new();
        pb.push_circle(hx, y + h / 2.0, h * 0.9);
        if let Some(path) = pb.finish() {
            pm.fill_path(&path, &solid(rgba([220, 245, 255], 1.0), BlendMode::SourceOver),
                FillRule::Winding, Transform::identity(), None);
        }
    }

    // ── Next-note cue (micro: highlight the immediate upcoming piano notes) ──
    fn draw_next_note_cue(&self, pm: &mut Pixmap, t: f64, active: &[u32]) {
        // Find the nearest upcoming piano onset (still ahead of the hit line).
        let mut next = f64::INFINITY;
        for &ni in active {
            let n = &self.notes[ni as usize];
            if !self.tracks[n.track].is_piano { continue; }
            if n.start >= t - 0.03 && n.start < next { next = n.start; }
        }
        if !next.is_finite() { return; }
        let lead = (next - t) as f32;
        if lead > self.lookahead as f32 { return; }
        let prox = (1.0 - lead / self.lookahead as f32).clamp(0.0, 1.0);
        let pulse = 0.55 + 0.45 * ((t * 7.0).sin() as f32);
        let alpha = (0.35 + prox * 0.55) * pulse;
        let cue = [120, 235, 255];

        for &ni in active {
            let n = &self.notes[ni as usize];
            if !self.tracks[n.track].is_piano { continue; }
            if (n.start - next).abs() > 0.03 { continue; }
            let (x, y, bw, bh) = self.note_box(n.pitch, n.start, n.end, t);

            // Guide line in the lane from the note down to the hit line.
            if self.horizontal {
                let cy = y + bh / 2.0;
                fill_rect(pm, self.hit, cy - 0.75, (x - self.hit).max(0.0), 1.5, &solid(rgba(cue, 0.18 * pulse), BlendMode::Plus));
            } else {
                let cx = x + bw / 2.0;
                fill_rect(pm, cx - 0.75, y + bh, 1.5, (self.hit - (y + bh)).max(0.0), &solid(rgba(cue, 0.18 * pulse), BlendMode::Plus));
            }

            // Glow + outline ring around the note.
            glow_disc(pm, x + bw / 2.0, y + bh / 2.0, (bw.max(bh)) * 1.6 + 8.0, cue, 0.25 * alpha);
            let mut pb = PathBuilder::new();
            round_rect(&mut pb, x - 3.5, y - 3.5, bw + 7.0, bh + 7.0, (bw.min(bh) * 0.4 + 3.0).min(8.0));
            if let Some(path) = pb.finish() {
                let stroke = Stroke { width: 2.2, ..Stroke::default() };
                pm.stroke_path(&path, &solid(rgba(lighten(cue, 0.3), (0.6 + prox * 0.4) * pulse), BlendMode::Plus),
                    &stroke, Transform::identity(), None);
            }
        }
    }

    // ── Re-entry countdown (meso: count yourself back in after a rest) ──────
    fn draw_countdown_pips(&self, pm: &mut Pixmap, t: f64, active: &[u32]) {
        // Skip if the piano is currently playing — only count during rests.
        for &ni in active {
            let n = &self.notes[ni as usize];
            if self.tracks[n.track].is_piano && t >= n.start && t <= n.end { return; }
        }
        // Next piano onset.
        let idx = self.piano_onsets.partition_point(|o| *o <= t);
        if idx >= self.piano_onsets.len() { return; }
        let next = self.piano_onsets[idx];
        let gap = next - t;
        if gap > self.lookahead + 1.0 { return; } // too far off; don't clutter

        // Beats remaining until the entrance (those strictly after t, up to next).
        let bstart = self.beats.partition_point(|b| *b <= t);
        let bend = self.beats.partition_point(|b| *b <= next + 1e-6);
        let beats_left = bend.saturating_sub(bstart);
        if beats_left == 0 || beats_left > 12 { return; }

        // Row of pips centered horizontally, just inside the note field above
        // the keyboard. The imminent pip pulses; passed beats are gone already
        // (we only draw the remaining count).
        let w = self.width as f32;
        let h = self.height as f32;
        let n = beats_left.min(8);
        let r = (w.min(h) * 0.012).max(7.0);
        let gapx = r * 3.2;
        let total = gapx * (n.saturating_sub(1)) as f32;
        // Center of the note field.
        let cx0 = if self.horizontal { self.kb_size + (w - self.kb_size) / 2.0 } else { w / 2.0 };
        let start_x = cx0 - total / 2.0;
        let cy = if self.horizontal { h - h * 0.12 } else { self.hit - h * 0.10 };
        let color = [255, 210, 120];

        // Fraction toward the next beat → the nearest pip swells.
        let next_beat = self.beats.get(bstart).copied().unwrap_or(next);
        let to_next_beat = (next_beat - t).max(0.0);
        for i in 0..n {
            let px = start_x + gapx * i as f32;
            let imminent = i == 0;
            let swell = if imminent { 1.0 + 0.5 * (1.0 - (to_next_beat as f32 / 0.6).min(1.0)) } else { 1.0 };
            let a = if imminent { 0.95 } else { 0.4 + 0.5 * (1.0 - i as f32 / n as f32) };
            glow_disc(pm, px, cy, r * swell * 2.2, color, 0.3 * a);
            let mut pb = PathBuilder::new();
            pb.push_circle(px, cy, r * swell);
            if let Some(path) = pb.finish() {
                pm.fill_path(&path, &solid(rgba(lighten(color, if imminent { 0.4 } else { 0.0 }), a), BlendMode::Plus),
                    FillRule::Winding, Transform::identity(), None);
            }
        }
    }

    // ── Peripheral beat pulse ───────────────────────────────────────────────
    // A full-perimeter glow that swells just before each beat and decays after,
    // identical for every beat — designed to register in peripheral vision.
    fn beat_pulse_intensity(&self, t: f64) -> f32 {
        const RAMP: f64 = 0.14;  // anticipatory build-up before the beat
        const DECAY: f64 = 0.30; // fade after the beat
        if self.beats.is_empty() { return 0.0; }
        let i = self.beats.partition_point(|b| *b <= t);
        let mut v = 0.0f32;
        if i > 0 {
            let dt = t - self.beats[i - 1];
            if dt >= 0.0 && dt < DECAY {
                let x = 1.0 - (dt / DECAY) as f32; // 1 at beat → 0
                v = v.max(x * x);
            }
        }
        if i < self.beats.len() {
            let dt = self.beats[i] - t;
            if dt >= 0.0 && dt < RAMP {
                let x = 1.0 - (dt / RAMP) as f32; // 0 → 1 at beat
                v = v.max(x * x);
            }
        }
        v
    }

    fn draw_beat_pulse(&self, pm: &mut Pixmap, t: f64) {
        let intensity = self.beat_pulse_intensity(t);
        if intensity <= 0.003 { return; }
        let w = self.width as f32;
        let h = self.height as f32;
        let color = [255, 196, 120]; // warm amber
        let a = intensity * 0.55;
        let band = (w.min(h) * 0.16).max(40.0);

        // Four edge gradients fading inward (additive bloom).
        let edge = |pm: &mut Pixmap, from: (f32, f32), to: (f32, f32), x: f32, y: f32, bw: f32, bh: f32| {
            if let Some(p) = linear_grad(
                from, to,
                vec![
                    GradientStop::new(0.0, rgba(color, a)),
                    GradientStop::new(1.0, rgba(color, 0.0)),
                ],
                BlendMode::Plus,
            ) {
                fill_rect(pm, x, y, bw, bh, &p);
            }
        };
        edge(pm, (0.0, 0.0), (0.0, band), 0.0, 0.0, w, band);                 // top
        edge(pm, (0.0, h), (0.0, h - band), 0.0, h - band, w, band);          // bottom
        edge(pm, (0.0, 0.0), (band, 0.0), 0.0, 0.0, band, h);                 // left
        edge(pm, (w, 0.0), (w - band, 0.0), w - band, 0.0, band, h);          // right

        // Crisp bright rim at the very edge.
        let rim = solid(rgba(color, a), BlendMode::Plus);
        let rt = 3.0;
        fill_rect(pm, 0.0, 0.0, w, rt, &rim);
        fill_rect(pm, 0.0, h - rt, w, rt, &rim);
        fill_rect(pm, 0.0, 0.0, rt, h, &rim);
        fill_rect(pm, w - rt, 0.0, rt, h, &rim);
    }

    // ── Orchestra energy bars ───────────────────────────────────────────────
    // Big edge bars whose extent = current orchestral loudness, colored by the
    // dominant section, with a bloom on strong entrances. Vertical layout puts
    // them on the left/right edges; horizontal layout uses top/bottom (the left
    // edge is the keyboard there).
    fn draw_orchestra_bars(&self, pm: &mut Pixmap, t: f64, active: &[u32]) {
        let mut energy = 0.0f32;
        let mut cr = 0.0f32; let mut cg = 0.0f32; let mut cb = 0.0f32;
        let mut onset = 0.0f32;
        for &ni in active {
            let n = &self.notes[ni as usize];
            if self.tracks[n.track].is_piano { continue; }
            if t < n.start || t > n.end { continue; }
            let c = self.tracks[n.track].color;
            energy += n.vel;
            cr += c[0] as f32 * n.vel; cg += c[1] as f32 * n.vel; cb += c[2] as f32 * n.vel;
            let since = (t - n.start) as f32;
            if since >= 0.0 && since < 0.08 { onset += n.vel; } // recent entrance
        }
        if energy <= 1e-4 { return; }
        let frac = (energy / self.orch_norm).clamp(0.0, 1.0).powf(0.7);
        let color = [
            (cr / energy) as u8,
            (cg / energy) as u8,
            (cb / energy) as u8,
        ];
        let bloom = (onset / self.orch_norm).clamp(0.0, 1.0);

        let w = self.width as f32;
        let h = self.height as f32;
        let thick = (w.min(h) * 0.045).max(16.0);

        let draw_bar = |pm: &mut Pixmap, vertical: bool, near_edge: bool| {
            // Bar grows from the screen edge; gradient bright at the edge.
            if vertical {
                let bw = thick;
                let bh = frac * h;
                let x = if near_edge { 0.0 } else { w - bw };
                let y = h - bh;
                let (g0, g1) = if near_edge { ((0.0, 0.0), (bw, 0.0)) } else { ((w, 0.0), (w - bw, 0.0)) };
                if let Some(p) = linear_grad(g0, g1,
                    vec![GradientStop::new(0.0, rgba(color, 0.85)), GradientStop::new(1.0, rgba(color, 0.05))],
                    BlendMode::Plus) { fill_rect(pm, x, y, bw, bh, &p); }
                // bright cap + entrance bloom at the leading end
                fill_rect(pm, x, y, bw, 4.0, &solid(rgba(lighten(color, 0.6), 0.9), BlendMode::Plus));
                if bloom > 0.02 { glow_disc(pm, x + bw / 2.0, y, thick * (2.0 + bloom * 3.0), lighten(color, 0.4), 0.5 * bloom); }
            } else {
                let bw = frac * w;
                let bh = thick;
                let y = if near_edge { 0.0 } else { h - bh };
                let (g0, g1) = if near_edge { ((0.0, 0.0), (0.0, bh)) } else { ((0.0, h), (0.0, h - bh)) };
                if let Some(p) = linear_grad(g0, g1,
                    vec![GradientStop::new(0.0, rgba(color, 0.85)), GradientStop::new(1.0, rgba(color, 0.05))],
                    BlendMode::Plus) { fill_rect(pm, 0.0, y, bw, bh, &p); }
                fill_rect(pm, bw - 4.0, y, 4.0, bh, &solid(rgba(lighten(color, 0.6), 0.9), BlendMode::Plus));
                if bloom > 0.02 { glow_disc(pm, bw, y + bh / 2.0, thick * (2.0 + bloom * 3.0), lighten(color, 0.4), 0.5 * bloom); }
            }
        };

        if self.horizontal {
            draw_bar(pm, false, true);  // top
            draw_bar(pm, false, false); // bottom
        } else {
            draw_bar(pm, true, true);   // left
            draw_bar(pm, true, false);  // right
        }
    }

    // ── Tempo pendulum ──────────────────────────────────────────────────────
    // A large metronome-style bob sweeping side to side, reaching an extreme
    // (and flashing) exactly on each beat — a big motion cue for tempo.
    fn draw_tempo_pendulum(&self, pm: &mut Pixmap, t: f64) {
        if self.beats.len() < 2 { return; }
        let beats = &self.beats;
        let n = beats.len();
        let i = beats.partition_point(|b| *b <= t);
        let (b0, b1, idx) = if i == 0 {
            (beats[0] - (beats[1] - beats[0]).max(0.2), beats[0], 0usize)
        } else if i >= n {
            (beats[n - 1], beats[n - 1] + (beats[n - 1] - beats[n - 2]).max(0.2), n - 1)
        } else {
            (beats[i - 1], beats[i], i - 1)
        };
        let p = (((t - b0) / (b1 - b0).max(1e-3)).clamp(0.0, 1.0)) as f32;
        let pe = p * p * (3.0 - 2.0 * p); // smoothstep
        // Alternate which side we sweep toward; s ∈ [-1,1], extreme on the beat.
        let dir = if idx % 2 == 0 { 1.0f32 } else { -1.0 };
        let s = dir * (2.0 * pe - 1.0);

        let w = self.width as f32;
        let h = self.height as f32;
        // Baseline near the bottom of the note field (just above the keyboard in
        // vertical mode; along the bottom edge in horizontal mode).
        let base_y = if self.horizontal { h - h * 0.07 } else { self.hit - h * 0.05 };
        let span = if self.horizontal { (w - self.kb_size) * 0.42 } else { w * 0.34 };
        let center_x = if self.horizontal { self.kb_size + (w - self.kb_size) / 2.0 } else { w / 2.0 };
        let sag = h * 0.05;
        let bob_x = center_x + s * span;
        let bob_y = base_y - sag * (s * s); // low at center, rising to the beats
        let pivot = (center_x, base_y + h * 0.10);

        let accent = [120, 200, 255]; // cool blue, distinct from the warm beat pulse

        // Arm
        let mut pb = PathBuilder::new();
        pb.move_to(pivot.0, pivot.1);
        pb.line_to(bob_x, bob_y);
        if let Some(path) = pb.finish() {
            let paint = solid(rgba(accent, 0.5), BlendMode::Plus);
            let stroke = Stroke { width: 3.0, line_cap: LineCap::Round, ..Stroke::default() };
            pm.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
        }

        // Bob with a beat flash as it nears an extreme (|s| → 1).
        let r = (w.min(h) * 0.022).max(10.0);
        let beatness = (s.abs() - 0.7).max(0.0) / 0.3; // 0..1 near the turnaround
        glow_disc(pm, bob_x, bob_y, r * (2.4 + beatness * 1.6), accent, 0.45 + beatness * 0.4);
        let mut pb = PathBuilder::new();
        pb.push_circle(bob_x, bob_y, r);
        if let Some(path) = pb.finish() {
            pm.fill_path(&path, &solid(rgba(lighten(accent, 0.4), 0.95), BlendMode::Plus),
                FillRule::Winding, Transform::identity(), None);
        }
    }

    // ── Background ──────────────────────────────────────────────────────────

    fn draw_background(&self, pm: &mut Pixmap) {
        let w = self.width as f32;
        let h = self.height as f32;
        let stops = vec![
            GradientStop::new(0.0, Color::from_rgba8(10, 10, 20, 255)),
            GradientStop::new(0.75, Color::from_rgba8(13, 12, 26, 255)),
            GradientStop::new(1.0, Color::from_rgba8(20, 16, 34, 255)),
        ];
        let paint = if self.horizontal {
            linear_grad((w, 0.0), (0.0, 0.0), stops, BlendMode::Source)
        } else {
            linear_grad((0.0, 0.0), (0.0, h), stops, BlendMode::Source)
        };
        if let Some(paint) = paint {
            fill_rect(pm, 0.0, 0.0, w, h, &paint);
        }

        // Faint lane shading for black-key lanes keeps pitch readable.
        let lane = solid(Color::from_rgba8(255, 255, 255, 7), BlendMode::SourceOver);
        for p in LO_PITCH..=HI_PITCH {
            if is_black(p) {
                continue;
            }
            // octave boundary line at each C
            if p.rem_euclid(12) == 0 {
                let (off, _) = key_span(p, self.key_extent);
                if self.horizontal {
                    let y = self.key_extent - off;
                    fill_rect(pm, self.kb_size, y - 0.5, self.width as f32 - self.kb_size, 1.0, &lane);
                } else {
                    fill_rect(pm, off, 0.0, 1.0, self.hit, &lane);
                }
            }
        }
    }

    fn draw_beat_lines(&self, pm: &mut Pixmap, t: f64) {
        let soft = solid(Color::from_rgba8(160, 190, 255, 22), BlendMode::SourceOver);
        let core = solid(Color::from_rgba8(200, 215, 255, 64), BlendMode::SourceOver);
        for &b in &self.beats {
            let dt = b - t;
            if dt < -0.2 || dt > self.lookahead + 0.2 {
                continue;
            }
            let a = self.time_to_axis(dt);
            if self.horizontal {
                fill_rect(pm, a - 3.0, 0.0, 7.0, self.height as f32, &soft);
                fill_rect(pm, a - 1.0, 0.0, 2.0, self.height as f32, &core);
            } else {
                fill_rect(pm, 0.0, a - 3.0, self.width as f32, 7.0, &soft);
                fill_rect(pm, 0.0, a - 1.0, self.width as f32, 2.0, &core);
            }
        }
    }

    // ── Notes ───────────────────────────────────────────────────────────────

    fn draw_notes(&self, pm: &mut Pixmap, t: f64, active: &[u32], piano_pass: bool) {
        for &ni in active {
            let n = &self.notes[ni as usize];
            let track = &self.tracks[n.track];
            if track.is_piano != piano_pass {
                continue;
            }
            // Cull: gone once fully consumed, not yet visible past lookahead.
            if n.end < t || n.start > t + self.lookahead {
                continue;
            }

            let sounding = t >= n.start && t <= n.end;
            let (lane, lane_w) = key_span(n.pitch as i32, self.key_extent);
            let head = self.time_to_axis(n.start - t); // leading edge (hits the line)
            let tail = self.time_to_axis(n.end - t);

            let color = track.color;
            let vel = n.vel.clamp(0.0, 1.0);

            // Rect in scroll coords, clamped at the hit line as the note is consumed.
            let (x, y, w, h);
            if self.horizontal {
                let x0 = head.max(self.hit);
                let x1 = tail.min(self.width as f32 + 20.0);
                let ky = self.key_extent - lane - lane_w;
                x = x0;
                y = ky + 0.5;
                w = (x1 - x0).max(2.0);
                h = (lane_w - 1.0).max(1.5);
            } else {
                let y1 = head.min(self.hit); // bottom (leading edge)
                let y0 = tail.max(-20.0); // top
                x = lane + 0.5;
                y = y0;
                w = (lane_w - 1.0).max(1.5);
                h = (y1 - y0).max(2.0);
            }
            if w <= 0.0 || h <= 0.0 {
                continue;
            }

            let base_alpha = 0.55 + vel * 0.45;
            let radius = (w.min(h) * 0.3).min(5.0);

            // Glow halo — stronger while sounding.
            let glow_a = if sounding { 0.30 } else { 0.13 } * base_alpha;
            let halo = if piano_pass { lighten(color, 0.25) } else { color };
            let g1 = solid(rgba(halo, glow_a), BlendMode::Plus);
            fill_round_rect(pm, x - 5.0, y - 5.0, w + 10.0, h + 10.0, radius + 5.0, &g1);
            let g2 = solid(rgba(halo, glow_a * 0.45), BlendMode::Plus);
            fill_round_rect(pm, x - 10.0, y - 10.0, w + 20.0, h + 20.0, radius + 10.0, &g2);

            // Body: gradient bright at the leading edge.
            let bright = lighten(color, if sounding { 0.55 } else { 0.30 });
            let dark = darken(color, 0.25);
            let body_a = if sounding { 1.0 } else { base_alpha };
            let stops = vec![
                GradientStop::new(0.0, rgba(dark, body_a * 0.88)),
                GradientStop::new(1.0, rgba(bright, body_a)),
            ];
            let grad = if self.horizontal {
                // leading edge is at left (x)
                linear_grad((x + w, y), (x, y), stops, BlendMode::SourceOver)
            } else {
                // leading edge at bottom (y + h)
                linear_grad((x, y), (x, y + h), stops, BlendMode::SourceOver)
            };
            let body_paint =
                grad.unwrap_or_else(|| solid(rgba(color, body_a), BlendMode::SourceOver));
            fill_round_rect(pm, x, y, w, h, radius, &body_paint);

            // Crisp 1px edge so stacked notes stay separable.
            let mut pb = PathBuilder::new();
            round_rect(&mut pb, x + 0.5, y + 0.5, w - 1.0, h - 1.0, radius);
            if let Some(path) = pb.finish() {
                let edge = solid(rgba(lighten(color, 0.6), 0.5 * body_a), BlendMode::SourceOver);
                let stroke = Stroke { width: 1.0, ..Stroke::default() };
                pm.stroke_path(&path, &edge, &stroke, Transform::identity(), None);
            }

            // Bright cap at the leading edge (Synthesia-style attack marker).
            let cap = solid(rgba(lighten(color, 0.75), 0.85 * body_a), BlendMode::Plus);
            if self.horizontal {
                fill_round_rect(pm, x, y, 3.0_f32.min(w), h, radius, &cap);
            } else {
                let ch = 3.0_f32.min(h);
                fill_round_rect(pm, x, y + h - ch, w, ch, radius, &cap);
            }

            // Piano-only: animated sheen — a soft band of light travelling
            // along the note so long notes shimmer instead of sitting flat.
            if piano_pass {
                let cycle = (t * 0.55 + (ni % 97) as f64 * 0.0103).fract() as f32;
                if (0.14..=0.86).contains(&cycle) {
                    let stops = vec![
                        GradientStop::new(cycle - 0.12, rgba(PIANO_ROSE, 0.0)),
                        GradientStop::new(cycle, rgba(PIANO_ROSE, 0.4 * body_a)),
                        GradientStop::new(cycle + 0.12, rgba(PIANO_ROSE, 0.0)),
                    ];
                    let grad = if self.horizontal {
                        linear_grad((x, y), (x + w, y), stops, BlendMode::Plus)
                    } else {
                        linear_grad((x, y), (x, y + h), stops, BlendMode::Plus)
                    };
                    if let Some(p) = grad {
                        fill_round_rect(pm, x, y, w, h, radius, &p);
                    }
                }
            }
        }
    }

    // ── Piano light beams ───────────────────────────────────────────────────
    // While a piano note sounds, a soft shaft of light rises from its key into
    // the note field, swelling at the attack and lingering briefly on release.
    fn draw_piano_beams(&self, pm: &mut Pixmap, t: f64, active: &[u32]) {
        for &ni in active {
            let n = &self.notes[ni as usize];
            if !self.tracks[n.track].is_piano {
                continue;
            }
            if t < n.start || t > n.end + BEAM_FADE_OUT {
                continue;
            }
            let age = (t - n.start) as f32;
            let vel = n.vel.clamp(0.0, 1.0);
            let fade_in = (age / 0.06).min(1.0);
            let fade_out = if t > n.end {
                (1.0 - (t - n.end) / BEAM_FADE_OUT) as f32
            } else {
                1.0
            };
            // Swell on the attack, settle while held
            let intensity =
                0.14 * (0.35 + 0.65 * vel) * fade_in * fade_out * (1.0 + 1.1 * (-age * 4.0).exp());

            let (lane, lane_w) = key_span(n.pitch as i32, self.key_extent);
            let beams: [(f32, f32); 2] = [
                (lane_w * 2.4, intensity),        // wide soft shaft
                (lane_w * 0.95, intensity * 1.5), // bright core
            ];
            if self.horizontal {
                let cy = self.key_extent - lane - lane_w / 2.0;
                let len = (self.width as f32 - self.kb_size) * 0.42;
                for (bw, ia) in beams {
                    if let Some(p) = linear_grad(
                        (self.hit, 0.0),
                        (self.hit + len, 0.0),
                        vec![
                            GradientStop::new(0.0, rgba(PIANO_ROSE, ia)),
                            GradientStop::new(1.0, rgba(PIANO_ROSE, 0.0)),
                        ],
                        BlendMode::Plus,
                    ) {
                        fill_rect(pm, self.hit, cy - bw / 2.0, len, bw, &p);
                    }
                }
            } else {
                let cx = lane + lane_w / 2.0;
                let len = self.hit * 0.55;
                for (bw, ia) in beams {
                    if let Some(p) = linear_grad(
                        (0.0, self.hit),
                        (0.0, self.hit - len),
                        vec![
                            GradientStop::new(0.0, rgba(PIANO_ROSE, ia)),
                            GradientStop::new(1.0, rgba(PIANO_ROSE, 0.0)),
                        ],
                        BlendMode::Plus,
                    ) {
                        fill_rect(pm, cx - bw / 2.0, self.hit - len, bw, len, &p);
                    }
                }
            }
        }
    }

    // ── Hit line ────────────────────────────────────────────────────────────

    fn draw_hit_line(&self, pm: &mut Pixmap, _t: f64) {
        let glow_color = [255, 190, 110]; // warm amber
        let core_color = Color::from_rgba8(255, 232, 190, 240);

        if self.horizontal {
            let x = self.hit;
            let h = self.height as f32;
            // Wide soft bloom into the note field
            if let Some(p) = linear_grad(
                (x, 0.0),
                (x + 46.0, 0.0),
                vec![
                    GradientStop::new(0.0, rgba(glow_color, 0.16)),
                    GradientStop::new(1.0, rgba(glow_color, 0.0)),
                ],
                BlendMode::Plus,
            ) {
                fill_rect(pm, x, 0.0, 46.0, h, &p);
            }
            // Tight bloom both sides
            if let Some(p) = linear_grad(
                (x - 9.0, 0.0),
                (x + 9.0, 0.0),
                vec![
                    GradientStop::new(0.0, rgba(glow_color, 0.0)),
                    GradientStop::new(0.5, rgba(glow_color, 0.5)),
                    GradientStop::new(1.0, rgba(glow_color, 0.0)),
                ],
                BlendMode::Plus,
            ) {
                fill_rect(pm, x - 9.0, 0.0, 18.0, h, &p);
            }
            fill_rect(pm, x - 1.0, 0.0, 2.0, h, &solid(core_color, BlendMode::SourceOver));
        } else {
            let y = self.hit;
            let w = self.width as f32;
            if let Some(p) = linear_grad(
                (0.0, y),
                (0.0, y - 56.0),
                vec![
                    GradientStop::new(0.0, rgba(glow_color, 0.30)),
                    GradientStop::new(1.0, rgba(glow_color, 0.0)),
                ],
                BlendMode::Plus,
            ) {
                fill_rect(pm, 0.0, y - 56.0, w, 56.0, &p);
            }
            if let Some(p) = linear_grad(
                (0.0, y - 10.0),
                (0.0, y + 10.0),
                vec![
                    GradientStop::new(0.0, rgba(glow_color, 0.0)),
                    GradientStop::new(0.5, rgba(glow_color, 0.75)),
                    GradientStop::new(1.0, rgba(glow_color, 0.0)),
                ],
                BlendMode::Plus,
            ) {
                fill_rect(pm, 0.0, y - 10.0, w, 20.0, &p);
            }
            fill_rect(pm, 0.0, y - 1.5, w, 3.0, &solid(core_color, BlendMode::SourceOver));
        }
    }

    // ── Keyboard ────────────────────────────────────────────────────────────

    /// Velocity of the loudest currently-sounding *pianoforte* note on `pitch`.
    /// Only the piano part lights up the keyboard — it's the part being played.
    fn key_light(&self, t: f64, active: &[u32], pitch: i32) -> Option<f32> {
        let mut best: Option<f32> = None;
        for &ni in active {
            let n = &self.notes[ni as usize];
            if n.pitch as i32 != pitch || t < n.start || t > n.end {
                continue;
            }
            if !self.tracks[n.track].is_piano {
                continue;
            }
            best = Some(best.map_or(n.vel, |v: f32| v.max(n.vel)));
        }
        best
    }

    /// Wide, gentle pink bloom over a pressed key — two stacked radial
    /// gradients with a long tail so it reads as fuzzy light, not a spotlight.
    fn key_bloom(&self, pm: &mut Pixmap, cx: f32, cy: f32, key_w: f32, vel: f32) {
        let v = 0.6 + vel * 0.4;
        let c = lighten(KEY_PINK, 0.25);
        glow_disc(pm, cx, cy, key_w * 5.5, c, 0.10 * v);
        glow_disc(pm, cx, cy, key_w * 3.0, c, 0.16 * v);
    }

    fn draw_keyboard(&self, pm: &mut Pixmap, t: f64, active: &[u32]) {
        if self.horizontal {
            self.draw_keyboard_horizontal(pm, t, active);
        } else {
            self.draw_keyboard_vertical(pm, t, active);
        }
    }

    fn draw_keyboard_vertical(&self, pm: &mut Pixmap, t: f64, active: &[u32]) {
        let w = self.width as f32;
        let kb_y = self.hit;
        let kb_h = self.kb_size;
        let felt_h = 4.0;
        let keys_y = kb_y + felt_h;
        let keys_h = kb_h - felt_h;
        let black_h = keys_h * 0.62;

        // Case behind the keys
        fill_rect(pm, 0.0, kb_y, w, kb_h, &solid(Color::from_rgba8(8, 8, 10, 255), BlendMode::Source));
        // Red felt strip — the classic piano detail
        if let Some(p) = linear_grad(
            (0.0, kb_y),
            (0.0, kb_y + felt_h),
            vec![
                GradientStop::new(0.0, Color::from_rgba8(140, 22, 28, 255)),
                GradientStop::new(1.0, Color::from_rgba8(70, 10, 14, 255)),
            ],
            BlendMode::SourceOver,
        ) {
            fill_rect(pm, 0.0, kb_y, w, felt_h, &p);
        }

        // White keys
        for p in LO_PITCH..=HI_PITCH {
            if is_black(p) {
                continue;
            }
            let (kx, kw) = key_span(p, self.key_extent);
            let lit = self.key_light(t, active, p);
            let (top, bottom) = if lit.is_some() {
                let tint = lighten(KEY_PINK, 0.40);
                (tint, darken(tint, 0.22))
            } else {
                ([248, 248, 251], [205, 206, 214])
            };
            if let Some(paint) = linear_grad(
                (0.0, keys_y),
                (0.0, keys_y + keys_h),
                vec![
                    GradientStop::new(0.0, rgba(top, 1.0)),
                    GradientStop::new(0.82, rgba(bottom, 1.0)),
                    GradientStop::new(0.83, rgba(darken(bottom, 0.18), 1.0)), // front edge
                    GradientStop::new(1.0, rgba(darken(bottom, 0.08), 1.0)),
                ],
                BlendMode::SourceOver,
            ) {
                fill_round_rect(pm, kx + 0.5, keys_y, kw - 1.0, keys_h - 1.0, 2.5, &paint);
            }
            if lit.is_some() {
                // pressed key sits slightly "deeper": darken its top
                let shade = solid(Color::from_rgba8(0, 0, 0, 36), BlendMode::SourceOver);
                fill_rect(pm, kx + 0.5, keys_y, kw - 1.0, 5.0, &shade);
            }
        }

        // Black keys
        for p in LO_PITCH..=HI_PITCH {
            if !is_black(p) {
                continue;
            }
            let (kx, kw) = key_span(p, self.key_extent);
            let lit = self.key_light(t, active, p);
            let (top, bottom) = if lit.is_some() {
                (lighten(darken(KEY_PINK, 0.1), 0.1), darken(KEY_PINK, 0.55))
            } else {
                ([52, 52, 60], [14, 14, 18])
            };
            if let Some(paint) = linear_grad(
                (0.0, keys_y),
                (0.0, keys_y + black_h),
                vec![
                    GradientStop::new(0.0, rgba(top, 1.0)),
                    GradientStop::new(1.0, rgba(bottom, 1.0)),
                ],
                BlendMode::SourceOver,
            ) {
                fill_round_rect(pm, kx, keys_y, kw, black_h, 2.5, &paint);
            }
            // Glossy front face
            let gloss = solid(
                rgba(if lit.is_some() { lighten(KEY_PINK, 0.35) } else { [95, 95, 110] }, 0.9),
                BlendMode::SourceOver,
            );
            fill_round_rect(pm, kx + 1.5, keys_y + black_h - 6.0, kw - 3.0, 4.0, 1.5, &gloss);
        }

        // Soft pink bloom over lit keys, spilling up across the hit line
        for p in LO_PITCH..=HI_PITCH {
            if let Some(vel) = self.key_light(t, active, p) {
                let (kx, kw) = key_span(p, self.key_extent);
                self.key_bloom(pm, kx + kw / 2.0, kb_y, self.key_extent / N_WHITE, vel);
            }
        }
    }

    fn draw_keyboard_horizontal(&self, pm: &mut Pixmap, t: f64, active: &[u32]) {
        let h = self.height as f32;
        let kb_w = self.kb_size;
        let felt_w = 4.0;
        let keys_w = kb_w - felt_w;
        let black_w = keys_w * 0.62;

        fill_rect(pm, 0.0, 0.0, kb_w, h, &solid(Color::from_rgba8(8, 8, 10, 255), BlendMode::Source));
        if let Some(p) = linear_grad(
            (kb_w, 0.0),
            (kb_w - felt_w, 0.0),
            vec![
                GradientStop::new(0.0, Color::from_rgba8(140, 22, 28, 255)),
                GradientStop::new(1.0, Color::from_rgba8(70, 10, 14, 255)),
            ],
            BlendMode::SourceOver,
        ) {
            fill_rect(pm, kb_w - felt_w, 0.0, felt_w, h, &p);
        }

        // Keys run along y; high pitch at top. Key "front" faces right (the hit line).
        for pass in 0..2 {
            for p in LO_PITCH..=HI_PITCH {
                let black = is_black(p);
                if (pass == 0) == black {
                    continue; // whites in pass 0, blacks in pass 1
                }
                let (off, sz) = key_span(p, self.key_extent);
                let ky = self.key_extent - off - sz;
                let lit = self.key_light(t, active, p);
                let kw = if black { black_w } else { keys_w };
                let kx = keys_w - kw; // keys anchored at the felt/hit side
                let (near, far) = if black {
                    if lit.is_some() {
                        (lighten(darken(KEY_PINK, 0.1), 0.1), darken(KEY_PINK, 0.55))
                    } else {
                        ([52, 52, 60], [14, 14, 18])
                    }
                } else if lit.is_some() {
                    let tint = lighten(KEY_PINK, 0.40);
                    (tint, darken(tint, 0.22))
                } else {
                    ([248, 248, 251], [205, 206, 214])
                };
                if let Some(paint) = linear_grad(
                    (keys_w, 0.0),
                    (kx, 0.0),
                    vec![
                        GradientStop::new(0.0, rgba(near, 1.0)),
                        GradientStop::new(1.0, rgba(far, 1.0)),
                    ],
                    BlendMode::SourceOver,
                ) {
                    let (gy, gh) = if black {
                        (ky, sz)
                    } else {
                        (ky + 0.5, sz - 1.0)
                    };
                    fill_round_rect(pm, kx, gy, kw - if black { 0.0 } else { 0.5 }, gh, 2.5, &paint);
                }
            }
        }

        for p in LO_PITCH..=HI_PITCH {
            if let Some(vel) = self.key_light(t, active, p) {
                let (off, sz) = key_span(p, self.key_extent);
                let cy = self.key_extent - off - sz / 2.0;
                self.key_bloom(pm, kb_w, cy, self.key_extent / N_WHITE, vel);
            }
        }
    }

    // ── Piano ember fountain ────────────────────────────────────────────────
    // While a piano note is held, embers keep streaming off the hit point —
    // rising in vertical mode, trailing comet-style in horizontal mode.
    // Emissions happen on a fixed clock from the note onset, each seeded by
    // (note, emission index), so any frame can reconstruct them exactly.
    fn draw_piano_embers(&self, pm: &mut Pixmap, t: f64, ni: u32) {
        let n = &self.notes[ni as usize];
        if t < n.start {
            return;
        }
        let vel = n.vel.clamp(0.0, 1.0);
        let rel_t = (t - n.start) as f32;
        let emit_until = (n.end - n.start) as f32;
        let k_min = (((rel_t - SPARK_LIFE) / EMIT_DT).ceil() as i64).max(0);
        let k_max = (rel_t.min(emit_until) / EMIT_DT).floor() as i64;
        if k_max < k_min {
            return;
        }

        let (lane, lane_w) = key_span(n.pitch as i32, self.key_extent);
        let (ox, oy) = if self.horizontal {
            (self.hit, self.key_extent - lane - lane_w / 2.0)
        } else {
            (lane + lane_w / 2.0, self.hit)
        };

        let per_emit = 1 + (vel * 1.6) as usize;
        for k in k_min..=k_max {
            let e_age = rel_t - k as f32 * EMIT_DT;
            let mut rng = Rng::new((ni as u64).wrapping_mul(1315423911) ^ (k as u64).wrapping_mul(2654435761));
            for _ in 0..per_emit {
                let life = rng.range(0.4, SPARK_LIFE);
                let jitter = rng.range(-0.45, 0.45) * lane_w;
                let speed = rng.range(35.0, 105.0) * (0.6 + 0.6 * vel);
                let drift = rng.range(-16.0, 16.0);
                let phase = rng.range(0.0, 6.28);
                let size0 = rng.range(1.0, 2.4);
                let rose = rng.next() < 0.45;
                if e_age < 0.0 || e_age > life {
                    continue;
                }
                let kk = e_age / life; // 0→1
                let (px, py) = if self.horizontal {
                    // comet tail: stream back along the travel direction
                    (ox + speed * e_age + 24.0 * e_age * e_age, oy + jitter + drift * e_age)
                } else {
                    // ember rise: drift up with gentle buoyant acceleration
                    (ox + jitter + drift * e_age, oy - speed * e_age - 24.0 * e_age * e_age)
                };
                let twinkle = 0.7 + 0.3 * (e_age * 28.0 + phase).sin();
                let fade = (1.0 - kk) * twinkle;
                let color = if rose { PIANO_ROSE } else { PIANO_WARM };
                let size = size0 * (1.0 - kk * 0.5);
                glow_disc(pm, px, py, size * 3.2, color, 0.22 * fade);
                let mut pb = PathBuilder::new();
                pb.push_circle(px, py, size);
                if let Some(path) = pb.finish() {
                    let core = solid(rgba(lighten(color, 0.5), 0.85 * fade), BlendMode::Plus);
                    pm.fill_path(&path, &core, FillRule::Winding, Transform::identity(), None);
                }
            }
        }
    }

    // ── Conductor ───────────────────────────────────────────────────────────

    /// Baton tip position at time `t`. The tip sweeps side to side, landing
    /// at an ictus exactly on every beat (alternating direction), tracing a
    /// rising arc between beats — the classic lateral conducting gesture.
    fn baton_tip(&self, t: f64, cx: f32, base_y: f32) -> (f32, f32) {
        let beats = &self.beats;
        let n = beats.len();
        let i = beats.partition_point(|b| *b <= t);
        let (b0, b1, idx) = if i == 0 {
            (beats[0] - (beats[1] - beats[0]).max(0.2), beats[0], 0usize)
        } else if i >= n {
            (beats[n - 1], beats[n - 1] + (beats[n - 1] - beats[n - 2]).max(0.2), n - 1)
        } else {
            (beats[i - 1], beats[i], i - 1)
        };
        let p = (((t - b0) / (b1 - b0).max(1e-3)).clamp(0.0, 1.0)) as f32;
        let dir = if idx % 2 == 0 { 1.0f32 } else { -1.0 };
        let pe = p * p * (3.0 - 2.0 * p); // smoothstep ease
        const SPAN: f32 = 62.0;
        const LIFT: f32 = 44.0;
        (
            cx + dir * (2.0 * pe - 1.0) * SPAN,
            base_y - LIFT * 4.0 * p * (1.0 - p),
        )
    }

    fn draw_conductor(&self, pm: &mut Pixmap, t: f64) {
        if self.beats.len() < 2 {
            return;
        }
        let cx = self.width as f32 - 150.0;
        let sy = 150.0; // shoulder height
        let base_y = sy - 46.0; // baton tip height at the ictus

        let body = rgba([222, 226, 240], 0.42);
        let stroke_line = |pm: &mut Pixmap, x0: f32, y0: f32, x1: f32, y1: f32, w: f32, c: Color| {
            let mut pb = PathBuilder::new();
            pb.move_to(x0, y0);
            pb.line_to(x1, y1);
            if let Some(path) = pb.finish() {
                let paint = solid(c, BlendMode::SourceOver);
                let stroke = Stroke { width: w, line_cap: LineCap::Round, ..Stroke::default() };
                pm.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
            }
        };

        // Figure: head, torso, resting left arm
        let mut pb = PathBuilder::new();
        pb.push_circle(cx, sy - 26.0, 9.0);
        if let Some(path) = pb.finish() {
            pm.fill_path(&path, &solid(body, BlendMode::SourceOver), FillRule::Winding, Transform::identity(), None);
        }
        stroke_line(pm, cx, sy - 14.0, cx, sy + 38.0, 5.0, body);
        stroke_line(pm, cx, sy - 8.0, cx - 24.0, sy + 18.0, 3.5, body);

        // Baton arm + baton
        let (tx, ty) = self.baton_tip(t, cx, base_y);
        let hx = cx + (tx - cx) * 0.45;
        let hy = (sy - 8.0) + (ty - (sy - 8.0)) * 0.45;
        stroke_line(pm, cx, sy - 8.0, hx, hy, 3.5, body);
        stroke_line(pm, hx, hy, tx, ty, 2.0, rgba([255, 244, 215], 0.9));

        // Motion trail: ghost tips at recent times
        for k in 1..=5 {
            let (gx, gy) = self.baton_tip(t - k as f64 * 0.035, cx, base_y);
            glow_disc(pm, gx, gy, 7.0, [255, 210, 140], 0.16 * (1.0 - k as f32 / 6.0));
        }
        // Glowing tip
        glow_disc(pm, tx, ty, 13.0, [255, 215, 150], 0.55);

        // Ictus flash right on the beat
        let i = self.beats.partition_point(|b| *b <= t);
        if i > 0 {
            let age = t - self.beats[i - 1];
            if age < 0.15 {
                let k = (age / 0.15) as f32;
                let (fx, fy) = self.baton_tip(self.beats[i - 1] + 1e-4, cx, base_y);
                glow_disc(pm, fx, fy, 16.0 + 22.0 * k, [255, 225, 170], 0.5 * (1.0 - k));
            }
        }
    }

    // ── Particles ───────────────────────────────────────────────────────────

    fn draw_particles(&self, pm: &mut Pixmap, t: f64, active: &[u32]) {
        for &ni in active {
            let n = &self.notes[ni as usize];
            let track = &self.tracks[n.track];
            if track.is_piano {
                self.draw_piano_embers(pm, t, ni);
            }
            let age = (t - n.start) as f32;
            if age < 0.0 || age as f64 > PARTICLE_MAX_LIFE {
                continue;
            }
            let color = track.color;
            let vel = n.vel.clamp(0.0, 1.0);
            let (lane, lane_w) = key_span(n.pitch as i32, self.key_extent);

            // Impact point on the hit line
            let (cx, cy) = if self.horizontal {
                (self.hit, self.key_extent - lane - lane_w / 2.0)
            } else {
                (lane + lane_w / 2.0, self.hit)
            };

            // Impact flash: bright burst + expanding shatter ring.
            // The piano part hits harder: bigger flash and a second, delayed
            // shockwave ring.
            let boost = if track.is_piano { 1.45 } else { 1.0 };
            if (age as f64) < FLASH_LIFE {
                let k = age / FLASH_LIFE as f32; // 0→1
                glow_disc(
                    pm,
                    cx,
                    cy,
                    (10.0 + 26.0 * k) * boost,
                    lighten(color, 0.6),
                    (0.55 + vel * 0.3) * (1.0 - k),
                );
            }
            let n_rings = if track.is_piano { 2 } else { 1 };
            for r_i in 0..n_rings {
                let r_age = age - r_i as f32 * 0.06;
                if r_age < 0.0 || (r_age as f64) >= FLASH_LIFE {
                    continue;
                }
                let k = r_age / FLASH_LIFE as f32;
                let ring_r = (4.0 + 30.0 * k) * boost;
                let mut pb = PathBuilder::new();
                pb.push_circle(cx, cy, ring_r);
                if let Some(path) = pb.finish() {
                    let paint = solid(rgba(lighten(color, 0.5), 0.5 * (1.0 - k)), BlendMode::Plus);
                    let stroke = Stroke { width: 1.6 * (1.0 - k) + 0.4, ..Stroke::default() };
                    pm.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
                }
            }

            // Shards: seeded fan of glowing sparks with gravity
            let mut rng = Rng::new((ni as u64) << 17 | n.pitch as u64);
            let count = (6.0 + vel * 10.0) as usize;
            for _ in 0..count {
                let life = rng.range(0.35, PARTICLE_MAX_LIFE as f32);
                let speed = rng.range(70.0, 230.0) * (0.5 + vel * 0.8);
                // Vertical mode: fan upward. Horizontal: fan rightward (back into the field).
                let angle = if self.horizontal {
                    rng.range(-1.0, 1.0) // radians around +x
                } else {
                    -std::f32::consts::FRAC_PI_2 + rng.range(-1.0, 1.0)
                };
                let size0 = rng.range(1.4, 3.2);
                let drift = rng.range(-20.0, 20.0);
                if age > life {
                    continue;
                }
                let k = age / life; // 0→1
                let gx = if self.horizontal { -140.0 } else { drift }; // decelerate / drift
                let gy = if self.horizontal { 220.0 } else { 620.0 }; // gravity
                let px = cx + (angle.cos() * speed + if self.horizontal { 0.0 } else { drift }) * age
                    + 0.5 * gx * age * age * if self.horizontal { 1.0 } else { 0.0 };
                let py = cy + angle.sin() * speed * age + 0.5 * gy * age * age;
                let fade = 1.0 - k;
                let size = size0 * (1.0 - k * 0.6);
                glow_disc(pm, px, py, size * 3.0, color, 0.30 * fade);
                let mut pb = PathBuilder::new();
                pb.push_circle(px, py, size);
                if let Some(path) = pb.finish() {
                    let core = solid(rgba(lighten(color, 0.7), 0.9 * fade), BlendMode::Plus);
                    pm.fill_path(&path, &core, FillRule::Winding, Transform::identity(), None);
                }
            }
        }
    }
}
