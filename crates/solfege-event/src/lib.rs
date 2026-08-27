//! Internal events shared by every host adapter.

pub use solfege_core::GestureControl;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Articulation {
    Attack,
    Legato,
    Release,
    Custom(u16),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Event {
    NoteOn {
        note: u8,
        velocity: f32,
        note_id: i32,
    },
    NoteOff {
        note: u8,
        velocity: f32,
        note_id: i32,
    },
    PolyPressure {
        note: u8,
        value: f32,
        note_id: i32,
    },
    ChannelPressure {
        channel: u8,
        value: f32,
    },
    ControlChange {
        channel: u8,
        controller: u8,
        value: f32,
    },
    PitchBend {
        channel: u8,
        value: f32,
    },
    Sustain(bool),
    Sostenuto(bool),
    NoteExpression {
        note_id: i32,
        pitch: f32,
        pressure: f32,
        slide: f32,
    },
    /// Continuous pitch in Hz. It is not a MIDI note or a semitone offset.
    Pitch {
        note_id: i32,
        hz: f32,
    },
    Expression {
        note_id: i32,
        value: f32,
    },
    Gesture {
        note_id: i32,
        control: GestureControl,
        value: f32,
    },
    Articulation {
        note_id: i32,
        articulation: Articulation,
    },
    Parameter {
        id: u32,
        value: f32,
    },
    Transport {
        playing: bool,
        tempo: f32,
        sample_position: i64,
    },
    AllNotesOff,
}

pub type SolfegeEvent = Event;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TimedEvent {
    pub frame_offset: u32,
    pub event: Event,
}

impl TimedEvent {
    pub const fn immediate(event: Event) -> Self {
        Self {
            frame_offset: 0,
            event,
        }
    }

    pub const fn at_sample(sample_offset: u32, event: Event) -> Self {
        Self {
            frame_offset: sample_offset,
            event,
        }
    }

    pub const fn sample_offset(self) -> u32 {
        self.frame_offset
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_events_preserve_note_id_and_sample_offset() {
        let event = TimedEvent::at_sample(
            37,
            Event::Gesture {
                note_id: 9184,
                control: GestureControl::BowPressure,
                value: 0.72,
            },
        );
        assert_eq!(event.sample_offset(), 37);
        assert!(matches!(event.event, Event::Gesture { note_id: 9184, .. }));
    }
}
