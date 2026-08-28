//! A reproducible musical performance description for A/B testing the engine.
//!
//! The single-note render commands are good for isolating one source. They
//! cannot answer the question that actually decides whether the instrument
//! sounds right: *does a phrase hold together* — sustained tones, note
//! transitions, a drawn pitch line, dynamics, and a tail long enough for a
//! stateful renderer to drift.
//!
//! The phrase is a small JSON document so a validation render is a file under
//! version control rather than a command line someone has to remember, and so
//! the same phrase renders identically before and after a change. Everything
//! here is deterministic: no wall clock, no randomness, fixed event schedule.
//!
//! ```json
//! {
//!   "seconds": 30.0,
//!   "block_frames": 64,
//!   "notes": [
//!     { "start": 0.0, "duration": 2.0, "note": 67, "velocity": 0.8,
//!       "articulation": "sustain_vibrato",
//!       "pitch": [{ "t": 0.0, "cents": 0 }, { "t": 1.0, "cents": 50 }] }
//!   ],
//!   "expression": [{ "t": 0.0, "value": 0.7 }]
//! }
//! ```
//!
//! Pitch is written as **cent deviations from the note's own MIDI pitch**, the
//! same representation the DAW's `PitchCurve` uses, so a phrase authored here
//! and a phrase drawn in the editor mean the same thing.

use std::{collections::BTreeMap, fs, path::Path};

use serde_json::Value;
use solfege_event::{Articulation, Event, TimedEvent};

/// Resolution at which a pitch curve is turned into engine events. Matches the
/// DAW snapshot builder's sampling step, so a phrase rendered here exercises
/// the same event density the product produces.
const PITCH_STEP_SECONDS: f64 = 0.005;

pub struct ScheduledEvent {
    /// Absolute frame from the render start.
    pub frame: usize,
    pub event: Event,
}

pub struct Performance {
    pub seconds: f32,
    pub block_frames: usize,
    pub events: Vec<ScheduledEvent>,
}

fn number(value: Option<&Value>, default: f64) -> f64 {
    value.and_then(Value::as_f64).unwrap_or(default)
}

fn midi_note_hz(note: f64) -> f64 {
    440.0 * 2.0f64.powf((note - 69.0) / 12.0)
}

fn interpolate(points: &[(f64, f64)], t: f64) -> f64 {
    match points.binary_search_by(|(time, _)| time.total_cmp(&t)) {
        Ok(hit) => points[hit].1,
        Err(0) => points[0].1,
        Err(index) if index >= points.len() => points[points.len() - 1].1,
        Err(index) => {
            let (t0, v0) = points[index - 1];
            let (t1, v1) = points[index];
            let span = t1 - t0;
            if span <= f64::EPSILON {
                v1
            } else {
                v0 + (v1 - v0) * ((t - t0) / span)
            }
        }
    }
}

/// Parse a performance document into an ordered engine event schedule.
///
/// `articulation_of` maps the document's articulation names onto the model's
/// own ids, so this stays independent of any one instrument's articulation set.
pub fn parse(
    document: &Value,
    sample_rate: u32,
    default_articulation: u8,
    articulation_of: &dyn Fn(&str) -> Option<u8>,
) -> Result<Performance, String> {
    let seconds = number(document.get("seconds"), 10.0).max(0.1) as f32;
    let block_frames = number(document.get("block_frames"), 64.0).max(1.0) as usize;
    let frames_at = |t: f64| (t.max(0.0) * sample_rate as f64).round() as usize;

    let notes = document
        .get("notes")
        .and_then(Value::as_array)
        .ok_or_else(|| "performance document needs a \"notes\" array".to_owned())?;

    let mut events: Vec<ScheduledEvent> = Vec::with_capacity(notes.len() * 8);
    for (index, note) in notes.iter().enumerate() {
        // A distinct note id per note, so continuous pitch addresses exactly
        // one voice — the same discipline the DAW's scheduler uses.
        let note_id = 1000 + index as i32;
        let midi = number(note.get("note"), 67.0).clamp(0.0, 127.0) as u8;
        let velocity = number(note.get("velocity"), 0.8).clamp(0.0, 1.0) as f32;
        let start = frames_at(number(note.get("start"), 0.0));
        let end = start + frames_at(number(note.get("duration"), 1.0).max(0.01));

        let articulation = match note.get("articulation").and_then(Value::as_str) {
            Some(name) => {
                articulation_of(name).ok_or_else(|| format!("unknown articulation \"{name}\""))?
            }
            None => default_articulation,
        };
        events.push(ScheduledEvent {
            frame: start,
            event: Event::Articulation {
                note_id,
                articulation: Articulation::Custom(articulation as u16),
            },
        });
        events.push(ScheduledEvent {
            frame: start,
            event: Event::NoteOn {
                note: midi,
                velocity,
                note_id,
            },
        });

        if let Some(points) = note.get("pitch").and_then(Value::as_array) {
            let mut breakpoints: Vec<(f64, f64)> = points
                .iter()
                .map(|point| {
                    (
                        number(point.get("t"), 0.0).max(0.0),
                        number(point.get("cents"), 0.0),
                    )
                })
                .collect();
            breakpoints.sort_by(|a, b| a.0.total_cmp(&b.0));
            if !breakpoints.is_empty() {
                let step = (sample_rate as f64 * PITCH_STEP_SECONDS).max(1.0) as usize;
                let mut frame = start;
                while frame < end {
                    let seconds_in = (frame - start) as f64 / sample_rate as f64;
                    let cents = interpolate(&breakpoints, seconds_in);
                    events.push(ScheduledEvent {
                        frame,
                        event: Event::Pitch {
                            note_id,
                            hz: (midi_note_hz(midi as f64) * 2.0f64.powf(cents / 1200.0)) as f32,
                        },
                    });
                    frame += step;
                }
            }
        }

        events.push(ScheduledEvent {
            frame: end,
            event: Event::NoteOff {
                note: midi,
                velocity: 0.0,
                note_id,
            },
        });
    }

    if let Some(points) = document.get("expression").and_then(Value::as_array) {
        for point in points {
            events.push(ScheduledEvent {
                frame: frames_at(number(point.get("t"), 0.0)),
                event: Event::ControlChange {
                    channel: 0,
                    controller: 11,
                    value: number(point.get("value"), 1.0).clamp(0.0, 1.0) as f32,
                },
            });
        }
    }

    // Stable sort by frame keeps the per-note emission order above, so a
    // note-on always precedes its own first pitch target at the same frame.
    events.sort_by_key(|scheduled| scheduled.frame);
    Ok(Performance {
        seconds,
        block_frames,
        events,
    })
}

/// Group the schedule into per-block event lists, ready to feed the engine one
/// block at a time.
///
/// `block_frames` is passed in rather than read from `performance` because a
/// caller may override the block size (that is the whole point of a block-size
/// consistency test). Grouping by one size while rendering with another puts
/// every event in the wrong block, which looks exactly like the block-size
/// dependence such a test is trying to detect.
pub fn blocks(
    performance: &Performance,
    total_frames: usize,
    block_frames: usize,
) -> BTreeMap<usize, Vec<TimedEvent>> {
    let block_frames = block_frames.max(1);
    let mut by_block: BTreeMap<usize, Vec<TimedEvent>> = BTreeMap::new();
    for scheduled in &performance.events {
        if scheduled.frame >= total_frames {
            continue;
        }
        by_block
            .entry(scheduled.frame / block_frames)
            .or_default()
            .push(TimedEvent::at_sample(
                (scheduled.frame % block_frames) as u32,
                scheduled.event,
            ));
    }
    by_block
}

pub fn load(path: &Path) -> Result<Value, String> {
    let text = fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
    serde_json::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document() -> Value {
        serde_json::json!({
            "seconds": 2.0,
            "block_frames": 64,
            "notes": [{
                "start": 0.0, "duration": 1.0, "note": 69, "velocity": 0.8,
                "pitch": [{ "t": 0.0, "cents": 0 }, { "t": 1.0, "cents": 100 }]
            }]
        })
    }

    #[test]
    fn a_pitch_curve_becomes_absolute_frequency_events() {
        let performance = parse(&document(), 48_000, 2, &|_| Some(2)).expect("parses");
        let pitches: Vec<f32> = performance
            .events
            .iter()
            .filter_map(|scheduled| match scheduled.event {
                Event::Pitch { hz, .. } => Some(hz),
                _ => None,
            })
            .collect();
        assert!(pitches.len() > 100, "expected a dense trajectory");
        assert!(
            (pitches[0] - 440.0).abs() < 0.5,
            "starts at A4, got {}",
            pitches[0]
        );
        // Last emitted point sits one 5 ms step before the note end.
        let last = *pitches.last().unwrap();
        assert!(
            (last - 466.16).abs() < 2.0,
            "ends a semitone up, got {last}"
        );
    }

    #[test]
    fn a_note_on_precedes_its_own_first_pitch_target() {
        let performance = parse(&document(), 48_000, 2, &|_| Some(2)).expect("parses");
        let note_on = performance
            .events
            .iter()
            .position(|s| matches!(s.event, Event::NoteOn { .. }))
            .expect("note on");
        let first_pitch = performance
            .events
            .iter()
            .position(|s| matches!(s.event, Event::Pitch { .. }))
            .expect("pitch");
        assert!(
            note_on < first_pitch,
            "a pitch target for a voice that does not exist yet is silently dropped"
        );
    }

    /// An overridden block size must regroup the events, not merely change the
    /// render loop.
    ///
    /// The regression: `blocks` used to read `performance.block_frames` while
    /// the caller rendered with an override, so every event landed in the wrong
    /// block. On a 40 s phrase that moved note onsets by up to a block and made
    /// the render's RMS swing 76 % across block sizes — which reads exactly like
    /// the block-size dependence a consistency test exists to catch, while
    /// actually being a fault in the test harness.
    #[test]
    fn an_overridden_block_size_regroups_the_schedule() {
        let performance = parse(&document(), 48_000, 2, &|_| Some(2)).expect("parses");
        for block_frames in [32usize, 64, 128, 256] {
            let grouped = blocks(&performance, 96_000, block_frames);
            for (block, events) in &grouped {
                for event in events {
                    let offset = event.frame_offset as usize;
                    assert!(
                        offset < block_frames,
                        "block {block} at size {block_frames} carries offset {offset}"
                    );
                    let absolute = block * block_frames + offset;
                    assert!(
                        performance
                            .events
                            .iter()
                            .any(|scheduled| scheduled.frame == absolute),
                        "no scheduled event at frame {absolute}"
                    );
                }
            }
            let total: usize = grouped.values().map(Vec::len).sum();
            let expected = performance
                .events
                .iter()
                .filter(|scheduled| scheduled.frame < 96_000)
                .count();
            assert_eq!(total, expected, "events lost at block size {block_frames}");
        }
    }

    #[test]
    fn events_land_in_the_block_that_contains_them() {
        let performance = parse(&document(), 48_000, 2, &|_| Some(2)).expect("parses");
        let grouped = blocks(&performance, 96_000, performance.block_frames);
        for (block, events) in &grouped {
            for event in events {
                assert!(
                    (event.frame_offset as usize) < performance.block_frames,
                    "block {block} carries an offset past its own end"
                );
            }
        }
    }
}
