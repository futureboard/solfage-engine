//! MIDI 1 byte-stream translation at the platform boundary.

use solfege_event::{Event, GestureControl};

pub fn parse_channel_message(bytes: &[u8]) -> Option<Event> {
    let (&status, data) = bytes.split_first()?;
    let channel = status & 0x0f;
    match (status & 0xf0, data) {
        (0x80, [note, velocity, ..]) => Some(Event::NoteOff {
            note: *note,
            velocity: *velocity as f32 / 127.0,
            note_id: -1,
        }),
        (0x90, [note, 0, ..]) => Some(Event::NoteOff {
            note: *note,
            velocity: 0.0,
            note_id: -1,
        }),
        (0x90, [note, velocity, ..]) => Some(Event::NoteOn {
            note: *note,
            velocity: *velocity as f32 / 127.0,
            note_id: -1,
        }),
        (0xb0, [64, value, ..]) => Some(Event::Sustain(*value >= 64)),
        (0xb0, [controller, value, ..]) => {
            parse_controller(channel, *controller, *value as f32 / 127.0)
        }
        (0xd0, [value, ..]) => Some(Event::ChannelPressure {
            channel,
            value: *value as f32 / 127.0,
        }),
        (0xe0, [low, high, ..]) => {
            let raw = ((*high as u16) << 7) | *low as u16;
            Some(Event::PitchBend {
                channel,
                value: (raw as f32 - 8192.0) / 8192.0,
            })
        }
        _ => None,
    }
}

fn parse_controller(channel: u8, controller: u8, value: f32) -> Option<Event> {
    let event = match controller {
        // These are semantic adapter choices. Unknown controllers remain
        // available as raw compatibility events for host-specific mappings.
        1 => Event::Gesture {
            note_id: -1,
            control: GestureControl::VibratoDepth,
            value,
        },
        2 => Event::Gesture {
            note_id: -1,
            control: GestureControl::BreathPressure,
            value,
        },
        11 => Event::Expression { note_id: -1, value },
        74 => Event::Gesture {
            note_id: -1,
            control: GestureControl::BowPosition,
            value,
        },
        _ => Event::ControlChange {
            channel,
            controller,
            value,
        },
    };
    Some(event)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn velocity_zero_note_on_becomes_note_off() {
        assert!(matches!(
            parse_channel_message(&[0x90, 60, 0]),
            Some(Event::NoteOff { note: 60, .. })
        ));
    }

    #[test]
    fn common_performance_controllers_are_semantic_events() {
        assert!(matches!(
            parse_channel_message(&[0xb0, 74, 100]),
            Some(Event::Gesture {
                control: GestureControl::BowPosition,
                ..
            })
        ));
    }
}
