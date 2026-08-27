//! Immutable modulation route vocabulary.

use solfege_core::GestureState;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModulationSource {
    Velocity,
    Key,
    PitchBend,
    ChannelPressure,
    PolyPressure,
    ControlChange(u8),
    MpePressure,
    MpeSlide,
    MpePitch,
    AmpEnvelope,
    ModEnvelope,
    Lfo(u8),
    Random,
    Macro(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModulationDestination {
    Pitch,
    ContinuousPitch,
    Gain,
    Pan,
    FilterCutoff,
    Resonance,
    BowPressure,
    BowVelocity,
    BowPosition,
    BowDirection,
    VibratoDepth,
    VibratoRate,
    SampleStart,
    LoopPosition,
    Macro(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Polarity {
    Unipolar,
    Bipolar,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transform {
    Linear,
    Invert,
    Square,
}

#[derive(Debug, Clone, Copy)]
pub struct ModulationRoute {
    pub source: ModulationSource,
    pub destination: ModulationDestination,
    pub amount: f32,
    pub transform: Transform,
    pub smoothing_seconds: f32,
    pub polarity: Polarity,
}

impl ModulationRoute {
    pub fn evaluate(&self, source: f32) -> f32 {
        let input = match self.polarity {
            Polarity::Unipolar => source.clamp(0.0, 1.0),
            Polarity::Bipolar => source.clamp(-1.0, 1.0),
        };
        let transformed = match self.transform {
            Transform::Linear => input,
            Transform::Invert => match self.polarity {
                Polarity::Unipolar => 1.0 - input,
                Polarity::Bipolar => -input,
            },
            Transform::Square => input.signum() * input * input,
        };
        transformed * self.amount
    }
}

/// A fixed-capacity ramp used by the engine and voices for block/sample-rate
/// control smoothing. It stores no queues and performs no allocation while
/// advancing, so a gesture update can be applied on the audio thread.
#[derive(Debug, Clone, Copy)]
pub struct SmoothedValue {
    current: f32,
    target: f32,
    step: f32,
    remaining: u32,
}

impl SmoothedValue {
    pub const fn new(value: f32) -> Self {
        Self {
            current: value,
            target: value,
            step: 0.0,
            remaining: 0,
        }
    }

    pub fn set_target(&mut self, value: f32, samples: u32) {
        let value = if value.is_finite() {
            value
        } else {
            self.current
        };
        self.target = value;
        if samples == 0 {
            self.current = value;
            self.step = 0.0;
            self.remaining = 0;
        } else {
            self.step = (value - self.current) / samples as f32;
            self.remaining = samples;
        }
    }

    #[inline]
    pub fn advance(&mut self) -> f32 {
        if self.remaining != 0 {
            self.current += self.step;
            self.remaining -= 1;
            if self.remaining == 0 {
                self.current = self.target;
            }
        }
        self.current
    }

    pub const fn value(&self) -> f32 {
        self.current
    }
}

/// Interpolates all fields of a [`GestureState`] together. This is the
/// semantic-control equivalent of a parameter ramp and is deliberately kept
/// separate from MIDI CC handling.
#[derive(Debug, Clone, Copy)]
pub struct GestureInterpolator {
    current: GestureState,
    target: GestureState,
    step: GestureState,
    remaining: u32,
}

impl GestureInterpolator {
    pub const fn new(state: GestureState) -> Self {
        Self {
            current: state,
            target: state,
            step: GestureState {
                pitch_hz: 0.0,
                pressure: 0.0,
                velocity: 0.0,
                position: 0.0,
                expression: 0.0,
                vibrato_depth: 0.0,
                vibrato_rate: 0.0,
                attack: 0.0,
                release: 0.0,
                instrument: [0.0; solfege_core::INSTRUMENT_GESTURE_CHANNELS],
            },
            remaining: 0,
        }
    }

    pub fn set_target(&mut self, target: GestureState, samples: u32) {
        let target = target.sanitized();
        self.target = target;
        if samples == 0 {
            self.current = target;
            self.step = zero_state();
            self.remaining = 0;
            return;
        }
        self.step = difference(target, self.current, samples);
        self.remaining = samples;
    }

    #[inline]
    pub fn advance(&mut self) -> GestureState {
        if self.remaining != 0 {
            self.current = add(self.current, self.step);
            self.remaining -= 1;
            if self.remaining == 0 {
                self.current = self.target;
            }
        }
        self.current
    }

    pub const fn current(&self) -> GestureState {
        self.current
    }
}

fn zero_state() -> GestureState {
    GestureState {
        pitch_hz: 0.0,
        pressure: 0.0,
        velocity: 0.0,
        position: 0.0,
        expression: 0.0,
        vibrato_depth: 0.0,
        vibrato_rate: 0.0,
        attack: 0.0,
        release: 0.0,
        instrument: [0.0; solfege_core::INSTRUMENT_GESTURE_CHANNELS],
    }
}

fn difference(target: GestureState, current: GestureState, samples: u32) -> GestureState {
    let scale = 1.0 / samples as f32;
    let mut instrument = [0.0; solfege_core::INSTRUMENT_GESTURE_CHANNELS];
    for (step, (&to, &from)) in instrument
        .iter_mut()
        .zip(target.instrument.iter().zip(current.instrument.iter()))
    {
        *step = (to - from) * scale;
    }
    GestureState {
        pitch_hz: (target.pitch_hz - current.pitch_hz) * scale,
        pressure: (target.pressure - current.pressure) * scale,
        velocity: (target.velocity - current.velocity) * scale,
        position: (target.position - current.position) * scale,
        expression: (target.expression - current.expression) * scale,
        vibrato_depth: (target.vibrato_depth - current.vibrato_depth) * scale,
        vibrato_rate: (target.vibrato_rate - current.vibrato_rate) * scale,
        attack: (target.attack - current.attack) * scale,
        release: (target.release - current.release) * scale,
        instrument,
    }
}

fn add(left: GestureState, right: GestureState) -> GestureState {
    let mut instrument = [0.0; solfege_core::INSTRUMENT_GESTURE_CHANNELS];
    for ((value, &left), &right) in instrument
        .iter_mut()
        .zip(left.instrument.iter())
        .zip(right.instrument.iter())
    {
        *value = left + right;
    }
    GestureState {
        pitch_hz: left.pitch_hz + right.pitch_hz,
        pressure: left.pressure + right.pressure,
        velocity: left.velocity + right.velocity,
        position: left.position + right.position,
        expression: left.expression + right.expression,
        vibrato_depth: left.vibrato_depth + right.vibrato_depth,
        vibrato_rate: left.vibrato_rate + right.vibrato_rate,
        attack: left.attack + right.attack,
        release: left.release + right.release,
        instrument,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gesture_ramp_reaches_target_without_allocating() {
        let mut ramp = GestureInterpolator::new(GestureState::default());
        let mut target = GestureState {
            pitch_hz: 660.0,
            ..GestureState::default()
        };
        target.set(solfege_core::GestureControl::BowPressure, 1.0);
        ramp.set_target(target, 4);
        for _ in 0..4 {
            ramp.advance();
        }
        assert_eq!(ramp.current(), target);
    }
}
