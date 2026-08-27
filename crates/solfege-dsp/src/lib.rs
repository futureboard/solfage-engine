//! Reusable DSP graph contracts and physical/acoustic primitives.
//!
//! The bowed-string model below is intentionally a useful approximation rather
//! than a claim of physical exactness: a fractional digital waveguide provides
//! the string resonance, a bounded nonlinear friction curve provides the bow
//! interaction, and a small bank of damped modes approximates bridge/body
//! coupling. All buffers are sized by [`BowedString::new`] before rendering.

use std::f32::consts::PI;

use solfege_core::{BodyMode, BowedStringConfig, GestureControl, GestureState};

pub trait DspNode: Send {
    fn reset(&mut self, sample_rate: f32);
    fn process_interleaved(&mut self, samples: &mut [f32], channels: usize);
}

#[derive(Debug, Clone, Copy)]
pub struct Gain {
    linear: f32,
}

impl Gain {
    pub fn new(linear: f32) -> Self {
        Self {
            linear: linear.max(0.0),
        }
    }
}

impl DspNode for Gain {
    fn reset(&mut self, _sample_rate: f32) {}

    fn process_interleaved(&mut self, samples: &mut [f32], _channels: usize) {
        for sample in samples {
            *sample *= self.linear;
        }
    }
}

#[derive(Debug, Clone)]
pub struct DigitalWaveguide {
    buffer: Vec<f32>,
    write_index: usize,
}

impl DigitalWaveguide {
    pub fn new(max_delay_samples: usize) -> Self {
        Self {
            buffer: vec![0.0; max_delay_samples.max(8) + 4],
            write_index: 0,
        }
    }

    pub fn reset(&mut self) {
        self.buffer.fill(0.0);
        self.write_index = 0;
    }

    pub fn memory_bytes(&self) -> usize {
        self.buffer.capacity() * std::mem::size_of::<f32>()
    }

    #[inline]
    pub fn read(&self, delay_samples: f32) -> f32 {
        let max_delay = (self.buffer.len() - 3) as f32;
        let delay = delay_samples.clamp(1.0, max_delay);
        let position = self.write_index as f32 - delay;
        let length = self.buffer.len() as f32;
        let wrapped = position.rem_euclid(length);
        let index = wrapped.floor() as usize;
        let fraction = wrapped - index as f32;
        let next = (index + 1) % self.buffer.len();
        self.buffer[index] + (self.buffer[next] - self.buffer[index]) * fraction
    }

    #[inline]
    pub fn write_and_advance(&mut self, value: f32) {
        self.buffer[self.write_index] = finite_or_zero(value);
        self.write_index += 1;
        if self.write_index == self.buffer.len() {
            self.write_index = 0;
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct BowFriction {
    stiffness: f32,
}

impl BowFriction {
    pub const fn new(stiffness: f32) -> Self {
        Self { stiffness }
    }

    /// A smooth saturating friction approximation. Real bow/string contact is
    /// more complicated; this curve is stable and gives pressure/velocity a
    /// continuous influence without a hard velocity-layer switch.
    #[inline]
    pub fn excite(&self, relative_velocity: f32, pressure: f32) -> f32 {
        let pressure = pressure.clamp(0.0, 1.0);
        (relative_velocity * self.stiffness).tanh() * pressure
    }
}

#[derive(Debug, Clone, Copy)]
struct ModalState {
    coefficient: f32,
    radius: f32,
    gain: f32,
    previous: f32,
    previous_previous: f32,
}

impl Default for ModalState {
    fn default() -> Self {
        Self {
            coefficient: 0.0,
            radius: 0.0,
            gain: 0.0,
            previous: 0.0,
            previous_previous: 0.0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ModalResonator {
    modes: [ModalState; 4],
    sample_rate: f32,
}

impl ModalResonator {
    pub fn new(sample_rate: f32, body_modes: [BodyMode; 4]) -> Self {
        let sample_rate = if sample_rate.is_finite() {
            sample_rate.max(1.0)
        } else {
            48_000.0
        };
        let mut resonator = Self {
            modes: [ModalState::default(); 4],
            sample_rate,
        };
        resonator.configure(body_modes);
        resonator
    }

    pub fn configure(&mut self, body_modes: [BodyMode; 4]) {
        for (state, mode) in self.modes.iter_mut().zip(body_modes) {
            *state = modal_coefficients(mode, self.sample_rate);
        }
    }

    pub fn reset(&mut self) {
        for mode in &mut self.modes {
            mode.previous = 0.0;
            mode.previous_previous = 0.0;
        }
    }

    #[inline]
    pub fn process(&mut self, input: f32) -> f32 {
        let mut output = 0.0;
        for mode in &mut self.modes {
            let current = mode.gain * input + mode.coefficient * mode.previous
                - mode.radius * mode.radius * mode.previous_previous;
            mode.previous_previous = mode.previous;
            mode.previous = finite_or_zero(current);
            output += mode.previous;
        }
        finite_or_zero(output)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct AcousticRadiation {
    state: f32,
    damping: f32,
}

impl AcousticRadiation {
    pub const fn new(damping: f32) -> Self {
        Self {
            state: 0.0,
            damping,
        }
    }

    pub fn reset(&mut self) {
        self.state = 0.0;
    }

    /// A deliberately small one-pole radiation/microphone coloration. It
    /// removes DC and gently emphasises changes at the bridge output.
    #[inline]
    pub fn process(&mut self, input: f32) -> f32 {
        let input = finite_or_zero(input);
        let damping = self.damping.clamp(0.0, 0.999_999);
        let high_pass = input - self.state;
        self.state = damping * self.state + (1.0 - damping) * input;
        finite_or_zero(high_pass)
    }
}

#[derive(Debug, Clone, Copy)]
struct NoiseGenerator {
    state: u32,
}

impl NoiseGenerator {
    const fn new(seed: u32) -> Self {
        Self {
            state: if seed == 0 { 1 } else { seed },
        }
    }

    #[inline]
    fn next(&mut self) -> f32 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.state = x;
        (x as f32 / u32::MAX as f32) * 2.0 - 1.0
    }
}

/// First physical backend: a generic bowed string with a persistent waveguide,
/// nonlinear bow contact, modal body, and simple radiation stage.
#[derive(Debug, Clone)]
pub struct BowedString {
    sample_rate: f32,
    config: BowedStringConfig,
    waveguide: DigitalWaveguide,
    friction: BowFriction,
    body: ModalResonator,
    radiation: AcousticRadiation,
    noise: NoiseGenerator,
    target_pitch_hz: f32,
    pitch_hz: f32,
    vibrato_phase: f32,
    gate: bool,
    attack_samples: u32,
    release_gain: f32,
    last_output: f32,
}

impl BowedString {
    pub fn new(sample_rate: f32, config: BowedStringConfig) -> Self {
        let sample_rate = if sample_rate.is_finite() {
            sample_rate.max(1.0)
        } else {
            48_000.0
        };
        let config = config.sanitized(sample_rate);
        let minimum = config.min_frequency_hz.max(1.0);
        let maximum_delay = (sample_rate / minimum).ceil() as usize;
        Self {
            sample_rate,
            friction: BowFriction::new(config.bow_stiffness.max(0.01)),
            body: ModalResonator::new(sample_rate, config.body_modes),
            radiation: AcousticRadiation::new(config.radiation_damping),
            waveguide: DigitalWaveguide::new(maximum_delay),
            noise: NoiseGenerator::new(0x9e37_79b9),
            config,
            target_pitch_hz: 440.0,
            pitch_hz: 440.0,
            vibrato_phase: 0.0,
            gate: false,
            attack_samples: 0,
            release_gain: 1.0,
            last_output: 0.0,
        }
    }

    pub fn reset(&mut self) {
        self.waveguide.reset();
        self.body.reset();
        self.radiation.reset();
        self.target_pitch_hz = 440.0;
        self.pitch_hz = 440.0;
        self.vibrato_phase = 0.0;
        self.gate = false;
        self.attack_samples = 0;
        self.release_gain = 1.0;
        self.last_output = 0.0;
    }

    /// Bytes owned by the physical state, excluding the containing voice's
    /// inline fields. This is a preparation-time diagnostic.
    pub fn memory_bytes(&self) -> usize {
        self.waveguide.memory_bytes()
    }

    pub fn note_on(&mut self, gesture: GestureState) {
        self.waveguide.reset();
        self.body.reset();
        self.radiation.reset();
        self.target_pitch_hz = clamp_pitch(gesture.pitch_hz, &self.config);
        self.pitch_hz = self.target_pitch_hz;
        self.vibrato_phase = 0.0;
        self.gate = true;
        self.attack_samples = 0;
        self.release_gain = 1.0;
        self.last_output = 0.0;
    }

    pub fn note_off(&mut self) {
        self.gate = false;
    }

    pub const fn is_gated(&self) -> bool {
        self.gate
    }

    pub fn is_quiet(&self) -> bool {
        !self.gate && self.release_gain < 1.0e-5 && self.last_output.abs() < 1.0e-5
    }

    #[inline]
    pub fn process(&mut self, gesture: GestureState) -> f32 {
        let gesture = gesture.sanitized();
        self.target_pitch_hz = clamp_pitch(gesture.pitch_hz, &self.config);
        self.pitch_hz += (self.target_pitch_hz - self.pitch_hz) * 0.0025;
        let vibrato_rate = gesture.vibrato_rate.min(30.0);
        self.vibrato_phase = (self.vibrato_phase + vibrato_rate / self.sample_rate).rem_euclid(1.0);
        let vibrato = (self.vibrato_phase * 2.0 * PI).sin() * gesture.vibrato_depth * 0.025;
        let pitch = clamp_pitch(self.pitch_hz * (1.0 + vibrato), &self.config);
        let delay = (self.sample_rate / pitch).clamp(2.0, (self.waveguide_len() - 4) as f32);

        let bridge_sample = self.waveguide.read(delay);
        let bow_position = gesture.get(GestureControl::BowPosition).clamp(0.02, 0.98);
        let bow_tap = self.waveguide.read((delay * bow_position).max(1.0));
        let string_velocity = 0.75 * bridge_sample + 0.25 * bow_tap;
        let bow_direction = gesture.get(GestureControl::BowDirection);
        let direction = if bow_direction <= 0.001 {
            1.0
        } else {
            bow_direction * 2.0 - 1.0
        };
        let bow_velocity = gesture.get(GestureControl::BowVelocity).max(0.02) * direction;
        let pressure = gesture
            .get(GestureControl::BowPressure)
            .max(gesture.pressure);
        let friction = self.friction.excite(
            bow_velocity * self.config.bow_friction - string_velocity,
            pressure,
        );
        let attack = if self.gate && self.attack_samples < 64 {
            self.attack_samples += 1;
            (1.0 - self.attack_samples as f32 / 64.0) * gesture.attack * 0.12
        } else {
            0.0
        };
        let noise = self.noise.next() * self.config.noise_amount * pressure;
        let feedback = bridge_sample * self.config.string_decay.clamp(0.0, 0.99999);
        let excitation = friction * 0.35 + attack + noise;
        self.waveguide.write_and_advance(feedback + excitation);

        if !self.gate {
            self.release_gain *= 0.9992;
        }
        let bridge = (bridge_sample * (1.0 - 0.35 * bow_position) + bow_tap * 0.35 * bow_position)
            * self.config.bridge_coupling
            + excitation * 0.05;
        let body = self.body.process(bridge);
        let mixed = bridge * (1.0 - self.config.body_mix.clamp(0.0, 1.0))
            + body * self.config.body_mix.clamp(0.0, 1.0);
        let radiated = self.radiation.process(mixed) * gesture.expression * self.release_gain;
        self.last_output = finite_or_zero(radiated * (0.35 + 0.65 * gesture.velocity));
        self.last_output
    }

    fn waveguide_len(&self) -> usize {
        // `DigitalWaveguide` is deliberately private storage, so use the
        // configured maximum as the same bound used by its constructor.
        (self.sample_rate / self.config.min_frequency_hz.max(1.0)).ceil() as usize + 8
    }
}

impl DspNode for BowedString {
    fn reset(&mut self, _sample_rate: f32) {
        self.reset();
    }

    fn process_interleaved(&mut self, samples: &mut [f32], channels: usize) {
        if channels == 0 {
            return;
        }
        let gesture = GestureState::default();
        for frame in samples.chunks_exact_mut(channels) {
            let value = self.process(gesture);
            frame.fill(value);
        }
    }
}

fn modal_coefficients(mode: BodyMode, sample_rate: f32) -> ModalState {
    let frequency = mode.frequency_hz.clamp(1.0, (sample_rate * 0.45).max(1.0));
    let radius = (-1.0 / (mode.decay_seconds.max(0.001) * sample_rate)).exp();
    ModalState {
        coefficient: 2.0 * radius * (2.0 * PI * frequency / sample_rate).cos(),
        radius,
        // The resonator is driven by a force-like bridge signal. Scaling the
        // input by modal loss makes `gain` an audible mode contribution rather
        // than an unbounded pole excitation (a second-order resonator's raw
        // impulse response grows roughly with 1 / (1 - radius)).
        gain: mode.gain * (1.0 - radius).max(0.0001),
        previous: 0.0,
        previous_previous: 0.0,
    }
}

fn clamp_pitch(value: f32, config: &BowedStringConfig) -> f32 {
    let minimum = config.min_frequency_hz.max(1.0);
    let maximum = config.max_frequency_hz.max(minimum);
    if value.is_finite() {
        value.clamp(minimum, maximum)
    } else {
        440.0_f32.clamp(minimum, maximum)
    }
}

#[inline]
fn finite_or_zero(value: f32) -> f32 {
    if value.is_finite() { value } else { 0.0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bowed_string_is_controllable_and_finite() {
        let mut string = BowedString::new(48_000.0, BowedStringConfig::default());
        let mut gesture = GestureState::for_note(60, 0.8);
        string.note_on(gesture);
        let first: f32 = (0..2_048).map(|_| string.process(gesture).abs()).sum();
        gesture.pitch_hz = 554.37;
        gesture.set(GestureControl::BowPressure, 0.8);
        gesture.vibrato_depth = 0.5;
        let second: f32 = (0..2_048).map(|_| string.process(gesture).abs()).sum();
        assert!(first.is_finite() && second.is_finite());
        assert!(first > 0.0 && second > 0.0);
        string.note_off();
        for _ in 0..2_048 {
            assert!(string.process(gesture).is_finite());
        }
    }

    #[test]
    fn modal_resonator_stays_finite_for_bad_input() {
        let mut body = ModalResonator::new(48_000.0, BowedStringConfig::default().body_modes);
        assert_eq!(body.process(f32::NAN), 0.0);
        assert!(body.process(1.0).is_finite());
    }
}
