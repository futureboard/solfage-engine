//! Control-thread SFM preparation helpers shared by tools and hosts.

use solfege_core::{BodyMode, BowedStringConfig};
use solfege_model::{PhysicalProfile, SfmError};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SfmMode {
    /// Run only the deterministic physical instrument. This path is always
    /// available, including in builds without the optional FBMX feature.
    PhysicalOnly,
    /// Run the physical instrument and add the embedded causal FBMX residual
    /// when the feature and section are available; otherwise physical output
    /// remains a valid fallback.
    Hybrid,
}

#[derive(Debug, Error)]
pub enum SfmEngineError {
    #[error("SFM format error: {0}")]
    Format(#[from] SfmError),
    #[error("physical instrument preparation failed: {0}")]
    Physical(String),
    #[error("FBMX preparation failed: {0}")]
    Fbmx(String),
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
