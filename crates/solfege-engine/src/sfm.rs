//! Control-thread SFM preparation helpers shared by tools and hosts.

use solfege_core::{BodyMode, BowedStringConfig};
use solfege_model::{PhysicalProfile, SfmError};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SfmMode {
    /// Run only the deterministic physical instrument. This path is always
    /// available, including in builds without the optional FBMX feature.
    PhysicalOnly,
    /// Run only the embedded indexed acoustic voicebank.
    VoicebankOnly,
    /// Run the physical instrument and add the embedded causal FBMX residual
    /// when the feature and section are available; otherwise physical output
    /// remains a valid fallback.
    Hybrid,
}

/// The step a model load reached.
///
/// Carried on every failure so a host can say *which* part of a package is
/// broken. "Failed to load model" is not a usable diagnosis for a 146 MB file
/// with seven independently checksummed sections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SfmLoadStage {
    Opening,
    Validating,
    LoadingPhysicalModel,
    LoadingVoicebank,
    LoadingNeuralModel,
    PreparingEngine,
}

impl SfmLoadStage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Opening => "Opening",
            Self::Validating => "Validating",
            Self::LoadingPhysicalModel => "Loading physical model",
            Self::LoadingVoicebank => "Loading voicebank",
            Self::LoadingNeuralModel => "Loading neural model",
            Self::PreparingEngine => "Preparing engine",
        }
    }
}

#[derive(Debug, Error)]
pub enum SfmEngineError {
    #[error("SFM format error: {0}")]
    Format(#[from] SfmError),
    #[error("physical instrument preparation failed: {0}")]
    Physical(String),
    #[error("FBMX preparation failed: {0}")]
    Fbmx(String),
    /// The package parsed, but the paired `INDX`/`AUDO` voicebank a sampled
    /// instrument needs is not complete. Distinguishes a physical-only package
    /// from a half-built one, which the caller cannot tell from a bare `None`.
    #[error(
        "SFM voicebank sections are incomplete: INDX {}, AUDO {}",
        if *index { "present" } else { "missing" },
        if *audio { "present" } else { "missing" }
    )]
    VoicebankSectionsMissing { index: bool, audio: bool },
    #[error("SFM load was cancelled")]
    Cancelled,
}

impl SfmEngineError {
    /// The stage this failure came from, for a message that names the step.
    pub fn stage(&self) -> SfmLoadStage {
        match self {
            Self::Format(SfmError::Io(_)) => SfmLoadStage::Opening,
            Self::Format(SfmError::InvalidPhysicalProfile(_)) | Self::Physical(_) => {
                SfmLoadStage::LoadingPhysicalModel
            }
            Self::Format(SfmError::InvalidVoicebank(_))
            | Self::Format(SfmError::InvalidAcousticAsset(_))
            | Self::VoicebankSectionsMissing { .. } => SfmLoadStage::LoadingVoicebank,
            Self::Fbmx(_) => SfmLoadStage::LoadingNeuralModel,
            Self::Cancelled | Self::Format(_) => SfmLoadStage::Validating,
        }
    }
}

pub fn to_bowed_string_config(profile: &PhysicalProfile) -> BowedStringConfig {
    BowedStringConfig {
        min_frequency_hz: profile.min_frequency_hz,
        max_frequency_hz: profile.max_frequency_hz,
        string_decay: profile.string_decay,
        bow_friction: profile.bow_friction,
        bow_stiffness: profile.bow_stiffness,
        bridge_coupling: profile.bridge_coupling,
        body_mix: profile.body_mix,
        noise_amount: profile.noise_amount,
        radiation_damping: profile.radiation_damping,
        body_modes: profile
            .body_modes
            .map(|mode| BodyMode::new(mode.frequency_hz, mode.decay_seconds, mode.gain)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_maps_without_losing_body_modes() {
        let profile = PhysicalProfile::default();
        let config = to_bowed_string_config(&profile);
        assert_eq!(config.body_modes[2].frequency_hz, 710.0);
        assert_eq!(config.max_frequency_hz, 2_000.0);
    }
}
