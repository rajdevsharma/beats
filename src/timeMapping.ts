import { Stretch } from "./types";

export interface TimeSegment {
  origStart: number;
  origEnd: number;
  stretchedStart: number;
  stretchedEnd: number;
  factor: number;
}

/** Build an ordered list of segments covering the full original timeline. */
export function buildSegments(totalDuration: number, stretches: Stretch[]): TimeSegment[] {
  const sorted = [...stretches].sort((a, b) => a.start - b.start);
  const segments: TimeSegment[] = [];
  let origCursor = 0;
  let stretchedCursor = 0;

  for (const s of sorted) {
    // Normal segment before this stretch
    if (s.start > origCursor) {
      const dur = s.start - origCursor;
      segments.push({
        origStart: origCursor, origEnd: s.start,
        stretchedStart: stretchedCursor, stretchedEnd: stretchedCursor + dur,
        factor: 1,
      });
      stretchedCursor += dur;
      origCursor = s.start;
    }
    // Stretched segment
    const origDur = s.end - s.start;
    const stretchedDur = origDur * s.factor;
    segments.push({
      origStart: s.start, origEnd: s.end,
      stretchedStart: stretchedCursor, stretchedEnd: stretchedCursor + stretchedDur,
      factor: s.factor,
    });
    stretchedCursor += stretchedDur;
    origCursor = s.end;
  }

  // Trailing normal segment
  if (origCursor < totalDuration) {
    const dur = totalDuration - origCursor;
    segments.push({
      origStart: origCursor, origEnd: totalDuration,
      stretchedStart: stretchedCursor, stretchedEnd: stretchedCursor + dur,
      factor: 1,
    });
    stretchedCursor += dur;
  }

  return segments;
}

/** Total duration of the audio after all stretches are applied. */
export function stretchedDuration(totalDuration: number, stretches: Stretch[]): number {
  return stretches.reduce(
    (dur, s) => dur + (s.end - s.start) * (s.factor - 1),
    totalDuration
  );
}

/** Map a time position in the original audio to its position in the stretched audio.
 *  NOTE: callers in hot paths must pass an already-sorted `stretches` array
 *  (sorted by `start`); this function does not sort, to avoid per-call allocation. */
export function originalToStretched(t: number, stretches: Stretch[]): number {
  if (stretches.length === 0) return t; // fast path: no stretches → identity
  let offset = 0;
  for (const s of stretches) {
    if (t <= s.start) break;
    if (t <= s.end) {
      return s.start + (t - s.start) * s.factor + offset;
    }
    offset += (s.end - s.start) * (s.factor - 1);
  }
  return t + offset;
}

/** Map a time position in the stretched audio back to the original audio. */
export function stretchedToOriginal(t: number, segments: TimeSegment[]): number {
  for (const seg of segments) {
    if (t <= seg.stretchedEnd) {
      const elapsed = t - seg.stretchedStart;
      return seg.origStart + elapsed / seg.factor;
    }
  }
  // Past end — clamp
  const last = segments[segments.length - 1];
  return last?.origEnd ?? t;
}

/** Reposition beats from original-audio time to stretched-audio time. */
export function repositionBeats(beats: number[], stretches: Stretch[]): number[] {
  return beats.map(b => originalToStretched(b, stretches));
}
