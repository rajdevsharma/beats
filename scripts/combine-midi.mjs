// Combines rach2 mov 1+2+3 into a single MIDI, merging tracks by name.
// Run: node scripts/combine-midi.mjs

import pkg from '@tonejs/midi';
const { Midi } = pkg;
import { readFileSync, writeFileSync } from 'fs';
import { join, dirname } from 'path';
import { fileURLToPath } from 'url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');

const files = [
  join(root, 'public/midi/rach2_mov1.mid'),
  join(root, 'public/midi/rach2_mov2.mid'),
  join(root, 'public/midi/rach2_mov3.mid'),
];

const midis = files.map(f => new Midi(readFileSync(f)));

const GAP = 3; // silence between movements (seconds)
const offsets = [0];
for (let i = 0; i < midis.length - 1; i++) {
  offsets.push(offsets[i] + midis[i].duration + GAP);
}
const totalDuration = offsets[offsets.length - 1] + midis[midis.length - 1].duration;

console.log('Durations:');
midis.forEach((m, i) => console.log(`  Mov ${i+1}: ${m.duration.toFixed(1)}s  offset ${offsets[i].toFixed(1)}s`));
console.log(`  Total: ${totalDuration.toFixed(1)}s`);

// Collect notes per instrument name, sorted by time
const PPQ = 480;       // ticks per beat
const BPM = 120;       // beats per minute
const USPB = 500000;   // microseconds per beat (= 60,000,000 / BPM)
const TPS = (PPQ * BPM) / 60;  // ticks per second = 960

function secToTick(s) { return Math.round(s * TPS); }

// Merge tracks by name
const trackMap = new Map(); // name → [{tick, pitch, dur_ticks, vel}]

for (let mi = 0; mi < midis.length; mi++) {
  const off = offsets[mi];
  for (const track of midis[mi].tracks) {
    if (!track.notes.length) continue;
    const name = track.name.trim();
    if (!trackMap.has(name)) trackMap.set(name, []);
    const arr = trackMap.get(name);
    for (const n of track.notes) {
      arr.push({
        tick:     secToTick(n.time + off),
        durTicks: Math.max(10, secToTick(n.duration)),
        pitch:    n.midi,
        vel:      Math.round(n.velocity * 127),
      });
    }
  }
}

// Sort each track by tick
for (const arr of trackMap.values()) arr.sort((a, b) => a.tick - b.tick);

console.log(`\nTracks (${trackMap.size}):`);
for (const [name, notes] of trackMap) console.log(`  ${name}: ${notes.length} notes`);

// ── Manual MIDI serialization ─────────────────────────────────────────────

function writeVarLen(val) {
  const bytes = [];
  bytes.unshift(val & 0x7f);
  val >>= 7;
  while (val > 0) { bytes.unshift((val & 0x7f) | 0x80); val >>= 7; }
  return Buffer.from(bytes);
}

function buildTrack(name, notes) {
  const events = [];

  // Track name
  const nameBytes = Buffer.from(name, 'utf8');
  events.push({ tick: 0, data: Buffer.concat([
    Buffer.from([0xff, 0x03, nameBytes.length]), nameBytes
  ])});

  // Expand notes into note-on / note-off events
  for (const n of notes) {
    events.push({ tick: n.tick,              data: Buffer.from([0x90, n.pitch, n.vel]) });
    events.push({ tick: n.tick + n.durTicks, data: Buffer.from([0x80, n.pitch, 0x00]) });
  }

  // Sort all events by tick
  events.sort((a, b) => a.tick - b.tick || (a.data[0] === 0x80 ? 1 : -1));

  // Encode as delta-time + event bytes
  const chunks = [];
  let prevTick = 0;
  for (const ev of events) {
    const delta = ev.tick - prevTick;
    prevTick = ev.tick;
    chunks.push(writeVarLen(delta));
    chunks.push(ev.data);
  }
  // End of track
  chunks.push(Buffer.from([0x00, 0xff, 0x2f, 0x00]));

  const body = Buffer.concat(chunks);
  const header = Buffer.alloc(8);
  header.write('MTrk', 0, 'ascii');
  header.writeUInt32BE(body.length, 4);
  return Buffer.concat([header, body]);
}

function buildTempoTrack() {
  // Set tempo event
  const tempoData = Buffer.from([
    0x00, 0xff, 0x51, 0x03,
    (USPB >> 16) & 0xff, (USPB >> 8) & 0xff, USPB & 0xff,
    0x00, 0xff, 0x2f, 0x00,
  ]);
  const header = Buffer.alloc(8);
  header.write('MTrk', 0, 'ascii');
  header.writeUInt32BE(tempoData.length, 4);
  return Buffer.concat([header, tempoData]);
}

// Assemble file
const trackNames = [...trackMap.keys()];
const numTracks = trackNames.length + 1; // +1 for tempo track

const fileHeader = Buffer.alloc(14);
fileHeader.write('MThd', 0, 'ascii');
fileHeader.writeUInt32BE(6, 4);
fileHeader.writeUInt16BE(1, 8);   // format 1
fileHeader.writeUInt16BE(numTracks, 10);
fileHeader.writeUInt16BE(PPQ, 12);

const parts = [fileHeader, buildTempoTrack()];
for (const name of trackNames) {
  parts.push(buildTrack(name, trackMap.get(name)));
}

const out = Buffer.concat(parts);
const outPath = join(root, 'public/midi/rach2_all.mid');
writeFileSync(outPath, out);
console.log(`\nWritten: ${outPath} (${(out.length / 1024).toFixed(0)} KB)`);
