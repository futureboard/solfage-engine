//! Thin optional bridge to the repository's pure-Rust FBMX runtime.
//!
//! Loading is deliberately separate from audio processing. A caller prepares
//! models on the control thread, installs the resulting hooks, and can then
//! bypass either performer or residual path with one boolean branch.

use std::path::Path;

use fbmx_runtime::{AudioModel, FbmxError, FbmxModel, LstmRuntime};
use solfege_event::Articulation;

/// The model's `articulation` category name for each voicebank articulation
/// id, indexed by that id.
///
/// The order is `solfege_model::voicebank`'s constant order and nothing else,
/// and the spellings are the ones the dataset writes into the model schema.
/// This table used to be written inline as `["sustain", "spiccato",
/// "pizzicato", "tremolo"]`, which was wrong three ways at once: the bank
/// spells its sustain `sustain_vibrato`, so id 0 matched no category and every
/// sustained note silently fell through to the model's declared default —
/// alphabetically `pizzicato` — while ids 1 and 2 were transposed, so
/// pizzicato notes were conditioned as spiccato and spiccato as pizzicato.
/// Only tremolo was right. `articulation_ids_match_the_voicebank` pins it.
const ARTICULATION_CATEGORY_NAMES: [&str; 4] = [
    "sustain_vibrato", // ARTICULATION_SUSTAIN_VIBRATO == 0
    "pizzicato",       // ARTICULATION_PIZZICATO       == 1
    "spiccato",        // ARTICULATION_SPICCATO        == 2
    "tremolo",         // ARTICULATION_TREMOLO         == 3
];

#[derive(Debug, Clone, Default)]
struct ResidualConditioning {
    midi_note: Option<usize>,
    velocity: Option<usize>,
    articulation_parameter: Option<usize>,
    articulation: [Option<usize>; 4],
    dynamic_parameter: Option<usize>,
    dynamic_soft: Option<usize>,
    dynamic_loud: Option<usize>,
}

impl ResidualConditioning {
    fn from_runtime(runtime: &LstmRuntime) -> Self {
        let schema = runtime.parameters().schema();
        let continuous = |name: &str| {
            schema
                .continuous
                .iter()
                .position(|parameter| parameter.name == name)
        };
        let categorical_parameter = |name: &str| {
            schema
                .categorical
                .iter()
                .position(|parameter| parameter.name == name)
        };
        let categorical = |name: &str, category: &str| {
            schema
                .categorical
                .iter()
                .find(|parameter| parameter.name == name)
                .and_then(|parameter| {
                    parameter
                        .categories
                        .iter()
                        .position(|item| item == category)
                })
        };
        Self {
            midi_note: continuous("midi_note"),
            velocity: continuous("velocity"),
            articulation_parameter: categorical_parameter("articulation"),
            articulation: [
                categorical("articulation", ARTICULATION_CATEGORY_NAMES[0]),
                categorical("articulation", ARTICULATION_CATEGORY_NAMES[1]),
                categorical("articulation", ARTICULATION_CATEGORY_NAMES[2]),
                categorical("articulation", ARTICULATION_CATEGORY_NAMES[3]),
            ],
            dynamic_parameter: categorical_parameter("dynamic"),
            dynamic_soft: categorical("dynamic", "p"),
            dynamic_loud: categorical("dynamic", "f"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct FbmxHooks {
    performer: Option<LstmRuntime>,
    /// One residual runtime per output channel.
    ///
    /// The model is a causal, recurrent, **mono** audio model: its hidden state
    /// is the recent history of one signal. Feeding it left and right samples
    /// alternately makes it see a signal at twice the rate with every other
    /// sample from the other channel — the learned time constants no longer
    /// mean what they meant in training, and the two channels contaminate each
    /// other. `residual[0]` follows the left channel and `residual[1]` the
    /// right; the mono entry point uses `residual[0]` alone.
    residual: Option<Box<[LstmRuntime; 2]>>,
    residual_conditioning: Option<ResidualConditioning>,
    performer_enabled: bool,
    residual_enabled: bool,
    residual_mix: f32,
    /// Whether the instrument is currently sounding nothing.
    ///
    /// The residual used to decide this from the block's own contents — an
    /// all-zero buffer meant "reset and skip". That made the model's state a
    /// function of the host's buffer size: a gap between two notes that falls
    /// wholly inside one 4096-frame block never produces a silent block and
    /// never resets, while the same gap at 64 frames produces several and does.
    /// The second note then starts from a different hidden state at each buffer
    /// size, which is exactly the block-size dependence the residual path is
    /// supposed to be free of. Voice activity is a property of the note events
    /// instead, so it partitions the same way at every block size.
    residual_idle: bool,
}

impl Default for FbmxHooks {
    fn default() -> Self {
        Self {
            performer: None,
            residual: None,
            residual_conditioning: None,
            performer_enabled: false,
            residual_enabled: false,
            residual_mix: 1.0,
            residual_idle: true,
        }
    }
}

/// Replace `sample` with the model's output, interpolated by `mix`.
///
/// **The model's output is the corrected signal, not a correction to add.** An
/// FBMX model with `residual = true` closes the skip connection inside itself
/// (`y = head(h) + x`) and was trained with the loss taken on that same `y`.
/// Adding `y` back onto `x` therefore sums the dry signal twice — measured on
/// the Solo Violin validation phrase as 2.7x the intended level and 17x the DC
/// offset, heard as a loud low smear over the instrument. `mix` interpolates
/// dry -> model, so `0.0` is bypass and `1.0` is the model, and a diagnostic
/// sweep across it attributes an artefact to the model or to the renderer.

/// Park the model instead of running it.
///
/// This is worth doing twice over. It is most of the cost: an LSTM(32) is
/// ~4200 multiply-accumulates per sample, and running it under an idle
/// instrument measured at ~350 us per 64-frame block — a quarter of one core
/// per track, spent on silence.
///
/// And it is also *more correct*. With a residual skip connection the model's
/// output for a zero input is `head(h)`, which is not zero unless the state is;
/// left running, an idle instrument hums whatever the model's resting output
/// happens to be. Resetting instead makes silence in give silence out exactly,
/// and starts every note from the same defined state — which is the condition
/// the model was trained under, since training resets the state at each segment.
#[inline]
fn idle_model(model: &mut LstmRuntime) {
    model.reset();
}

#[inline]
fn blend_model_output(model: &mut LstmRuntime, sample: &mut f32, mix: f32) {
    let wet = model.process_sample(*sample);
    if wet.is_finite() {
        *sample += (wet - *sample) * mix;
    }
}

impl FbmxHooks {
    pub fn from_models(
        performer: Option<FbmxModel>,
        residual: Option<FbmxModel>,
    ) -> Result<Self, FbmxError> {
        let performer = performer.map(|model| model.instantiate()).transpose()?;
        let residual = residual
            .map(|model| -> Result<Box<[LstmRuntime; 2]>, FbmxError> {
                Ok(Box::new([model.instantiate()?, model.instantiate()?]))
            })
            .transpose()?;
        let residual_conditioning = residual
            .as_ref()
            .map(|pair| ResidualConditioning::from_runtime(&pair[0]));
        Ok(Self {
            performer_enabled: performer.is_some(),
            residual_enabled: residual.is_some(),
            performer,
            residual,
            residual_conditioning,
            ..Self::default()
        })
    }

    pub fn load(
        performer_path: Option<impl AsRef<Path>>,
        residual_path: Option<impl AsRef<Path>>,
    ) -> Result<Self, FbmxError> {
        let performer = performer_path.map(FbmxModel::load).transpose()?;
        let residual = residual_path.map(FbmxModel::load).transpose()?;
        Self::from_models(performer, residual)
    }

    pub fn set_performer(&mut self, model: Option<LstmRuntime>) {
        self.performer = model;
        self.performer_enabled = self.performer.is_some();
    }

    /// Install one already-instantiated runtime, duplicated per channel.
    ///
    /// Both channels start from a defined state. Resetting only the clone left
    /// the left channel carrying whatever hidden state the caller's runtime had
    /// already accumulated, so the two channels began the first note from
    /// different histories and the stereo image drifted for as long as that
    /// state took to decay.
    pub fn set_residual(&mut self, model: Option<LstmRuntime>) {
        self.residual_conditioning = model.as_ref().map(ResidualConditioning::from_runtime);
        self.residual = model.map(|model| {
            let mut left = model;
            let mut right = left.clone();
            left.reset();
            right.reset();
            Box::new([left, right])
        });
        self.residual_enabled = self.residual.is_some();
    }

    /// Updates the residual model's note and velocity conditioning.
    ///
    /// The indexes are resolved while the model is loaded, so this remains
    /// allocation-free when called by the engine's realtime event path.
    pub fn set_residual_note(&mut self, note: f32, velocity: f32) {
        let Some(pair) = self.residual.as_mut() else {
            return;
        };
        let Some(conditioning) = self.residual_conditioning.as_ref() else {
            return;
        };
        for model in pair.iter_mut() {
            if let Some(index) = conditioning.midi_note {
                model.set_parameter_at(index, note);
            }
            if let Some(index) = conditioning.velocity {
                model.set_parameter_at(index, velocity);
            }
            if let (Some(parameter), Some(category)) = (
                conditioning.dynamic_parameter,
                if velocity < 0.5 {
                    conditioning.dynamic_soft
                } else {
                    conditioning.dynamic_loud
                },
            ) {
                model.set_category_at(parameter, category);
            }
            model.refresh_conditioning();
        }
    }

    /// Updates the residual model's articulation conditioning when the model
    /// carries a matching categorical field.
    /// Point the residual at the articulation the voicebank is playing.
    ///
    /// `Articulation::Custom(id)` carries the *voicebank's* articulation id, and
    /// `conditioning.articulation` is indexed by that same id, so the lookup is
    /// a direct index. The generic `Attack`/`Legato` names have no voicebank id
    /// and no entry in a model trained on voicebank articulations; mapping them
    /// onto slots 1 and 0 silently selected "pizzicato" for a legato phrase, so
    /// they now select nothing and the model keeps its declared default.
    pub fn set_residual_articulation(&mut self, articulation: Articulation) {
        let Some(pair) = self.residual.as_mut() else {
            return;
        };
        let Some(conditioning) = self.residual_conditioning.as_ref() else {
            return;
        };
        let slot = match articulation {
            Articulation::Custom(value) => conditioning
                .articulation
                .get(value as usize)
                .copied()
                .flatten(),
            Articulation::Attack | Articulation::Legato | Articulation::Release => None,
        };
        if let (Some(parameter), Some(category)) = (conditioning.articulation_parameter, slot) {
            for model in pair.iter_mut() {
                model.set_category_at(parameter, category);
                model.refresh_conditioning();
            }
        }
    }

    pub fn set_performer_enabled(&mut self, enabled: bool) {
        self.performer_enabled = enabled && self.performer.is_some();
    }

    pub fn set_residual_enabled(&mut self, enabled: bool) {
        self.residual_enabled = enabled && self.residual.is_some();
    }

    /// Tell the residual how many voices the instrument is sounding, before
    /// the block is handed to `apply_residual*`.
    ///
    /// The 0 -> sounding edge resets the recurrent state, so every phrase
    /// starts from the same defined history the model was trained under, at
    /// every host buffer size. While nothing is sounding the model is parked:
    /// the block is left exactly as it arrived, which for an instrument that
    /// added nothing is exact silence.
    pub fn set_active_voices(&mut self, active_voices: usize) {
        let idle = active_voices == 0;
        if self.residual_idle && !idle {
            if let Some(pair) = self.residual.as_mut() {
                for model in pair.iter_mut() {
                    idle_model(model);
                }
            }
        }
        self.residual_idle = idle;
    }

    pub fn set_residual_mix(&mut self, mix: f32) {
        self.residual_mix = if mix.is_finite() {
            mix.clamp(0.0, 1.0)
        } else {
            0.0
        };
    }

    pub const fn performer_enabled(&self) -> bool {
        self.performer_enabled
    }

    pub const fn residual_enabled(&self) -> bool {
        self.residual_enabled
    }

    /// Run one performer sample. The caller maps the returned learned curve
    /// into semantic gesture controls; this keeps FBMX independent of MIDI and
    /// of any one physical instrument.
    #[inline]
    pub fn process_performer_sample(&mut self, input: f32) -> Option<f32> {
        if self.performer_enabled {
            self.performer
                .as_mut()
                .map(|model| model.process_sample(input))
        } else {
            None
        }
    }

    /// Add a learned acoustic residual to an already-rendered **mono** buffer.
    /// With the hook disabled this is a cheap branch and no model work.
    ///
    /// Only `residual[0]` carries signal here, but both runtimes are parked
    /// over a silent block. Idling just the left one left the right runtime
    /// holding the hidden state of whatever it last processed, so the first
    /// stereo block after a mono stretch resumed from a stale history.
    #[inline]
    pub fn apply_residual(&mut self, output: &mut [f32]) {
        if !self.residual_enabled {
            return;
        }
        if self.residual_idle {
            return;
        }
        let mix = self.residual_mix;
        if let Some(pair) = self.residual.as_mut() {
            for sample in output {
                blend_model_output(&mut pair[0], sample, mix);
            }
        }
    }

    /// Apply the residual to one already-summed frame, in place.
    ///
    /// This is the entry point the engine's render loop uses, and it exists
    /// because conditioning is per note while a buffer is per block. Applying
    /// the residual to a whole block *after* the loop meant the model ran over
    /// every frame in that block under whichever note-on happened to arrive
    /// last — so a block containing two note-ons rendered its first note under
    /// the second note's conditioning, and the result changed with the host's
    /// buffer size. Per frame, the conditioning is always the one in force at
    /// that sample.
    ///
    /// `frame[0]` uses `residual[0]`; every further channel uses
    /// `residual[1]`, so each channel keeps its own recurrent history.
    #[inline]
    pub fn process_residual_frame(&mut self, frame: &mut [f32]) {
        if !self.residual_enabled || self.residual_idle {
            return;
        }
        let mix = self.residual_mix;
        if let Some(pair) = self.residual.as_mut() {
            for (channel, sample) in frame.iter_mut().enumerate() {
                let model = usize::from(channel > 0);
                blend_model_output(&mut pair[model], sample, mix);
            }
        }
    }

    /// Apply the residual to a caller-owned **interleaved** buffer.
    ///
    /// Each channel gets its own runtime, exactly as `apply_residual_stereo`
    /// does for planar buffers. Walking an interleaved buffer with one model
    /// instead feeds a causal mono network the sequence `L,R,L,R,…`: it sees a
    /// signal at twice the rate with every other sample belonging to the other
    /// channel, so the learned time constants stop meaning what they meant in
    /// training and the two channels contaminate each other's history. Channel
    /// counts above two reuse `residual[1]` for the extra channels rather than
    /// silently leaving them dry.
    #[inline]
    pub fn apply_residual_interleaved(&mut self, output: &mut [f32], channels: usize) {
        if !self.residual_enabled || channels == 0 {
            return;
        }
        if channels == 1 {
            self.apply_residual(output);
            return;
        }
        if self.residual_idle {
            return;
        }
        let mix = self.residual_mix;
        if let Some(pair) = self.residual.as_mut() {
            for frame in output.chunks_mut(channels) {
                for (channel, sample) in frame.iter_mut().enumerate() {
                    let model = if channel == 0 { 0 } else { 1 };
                    blend_model_output(&mut pair[model], sample, mix);
                }
            }
        }
    }

    /// Stereo counterpart used by hosts that keep the voicebank's original
    /// stereo image. The residual runtime is still causal and is fed in the
    /// same left/right order as an interleaved stereo buffer.
    #[inline]
    pub fn apply_residual_stereo(&mut self, left: &mut [f32], right: &mut [f32]) {
        if !self.residual_enabled {
            return;
        }
        if self.residual_idle {
            return;
        }
        let mix = self.residual_mix;
        if let Some(pair) = self.residual.as_mut() {
            let (left_model, right_model) = pair.split_at_mut(1);
            for sample in left.iter_mut() {
                blend_model_output(&mut left_model[0], sample, mix);
            }
            for sample in right.iter_mut() {
                blend_model_output(&mut right_model[0], sample, mix);
            }
        }
    }

    pub fn reset(&mut self) {
        if let Some(model) = self.performer.as_mut() {
            model.reset();
        }
        if let Some(pair) = self.residual.as_mut() {
            for model in pair.iter_mut() {
                model.reset();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The conditioning table is indexed by the voicebank's articulation id,
    /// so its order is not a style choice — it is the bank's constant order.
    /// Pinning it here means a reordering of either side fails the build
    /// rather than quietly conditioning every sustained note as pizzicato,
    /// which is what the previous hand-written order did.
    #[test]
    fn articulation_ids_match_the_voicebank() {
        use solfege_model::voicebank::{
            ARTICULATION_PIZZICATO, ARTICULATION_SPICCATO, ARTICULATION_SUSTAIN_VIBRATO,
            ARTICULATION_TREMOLO,
        };
        assert_eq!(
            ARTICULATION_CATEGORY_NAMES[ARTICULATION_SUSTAIN_VIBRATO as usize],
            "sustain_vibrato"
        );
        assert_eq!(
            ARTICULATION_CATEGORY_NAMES[ARTICULATION_PIZZICATO as usize],
            "pizzicato"
        );
        assert_eq!(
            ARTICULATION_CATEGORY_NAMES[ARTICULATION_SPICCATO as usize],
            "spiccato"
        );
        assert_eq!(
            ARTICULATION_CATEGORY_NAMES[ARTICULATION_TREMOLO as usize],
            "tremolo"
        );
    }

    #[test]
    fn disabled_residual_is_a_noop() {
        let mut hooks = FbmxHooks::default();
        let mut buffer = [0.25_f32, -0.5, 0.75];
        hooks.apply_residual(&mut buffer);
        assert_eq!(buffer, [0.25, -0.5, 0.75]);
        assert!(hooks.process_performer_sample(0.5).is_none());
    }
}
