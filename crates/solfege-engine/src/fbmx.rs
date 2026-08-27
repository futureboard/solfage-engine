//! Thin optional bridge to the repository's pure-Rust FBMX runtime.
//!
//! Loading is deliberately separate from audio processing. A caller prepares
//! models on the control thread, installs the resulting hooks, and can then
//! bypass either performer or residual path with one boolean branch.

use std::path::Path;

use fbmx_runtime::{AudioModel, FbmxError, FbmxModel, LstmRuntime};

#[derive(Debug)]
pub struct FbmxHooks {
    performer: Option<LstmRuntime>,
    residual: Option<LstmRuntime>,
    performer_enabled: bool,
    residual_enabled: bool,
    residual_mix: f32,
}

impl Default for FbmxHooks {
    fn default() -> Self {
        Self {
            performer: None,
            residual: None,
            performer_enabled: false,
            residual_enabled: false,
            residual_mix: 1.0,
        }
    }
}

impl FbmxHooks {
    pub fn from_models(
        performer: Option<FbmxModel>,
        residual: Option<FbmxModel>,
    ) -> Result<Self, FbmxError> {
        let performer = performer.map(|model| model.instantiate()).transpose()?;
        let residual = residual.map(|model| model.instantiate()).transpose()?;
        Ok(Self {
            performer_enabled: performer.is_some(),
            residual_enabled: residual.is_some(),
            performer,
            residual,
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

    pub fn set_residual(&mut self, model: Option<LstmRuntime>) {
        self.residual = model;
        self.residual_enabled = self.residual.is_some();
    }

    pub fn set_performer_enabled(&mut self, enabled: bool) {
        self.performer_enabled = enabled && self.performer.is_some();
    }

    pub fn set_residual_enabled(&mut self, enabled: bool) {
        self.residual_enabled = enabled && self.residual.is_some();
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

    /// Add a learned acoustic residual to an already-rendered physical/sample
    /// buffer. With the hook disabled this is a cheap branch and no model work.
    #[inline]
    pub fn apply_residual(&mut self, output: &mut [f32]) {
        if !self.residual_enabled {
            return;
        }
        if let Some(model) = self.residual.as_mut() {
            for sample in output {
                let residual = model.process_sample(*sample);
                *sample = if residual.is_finite() {
                    *sample + residual * self.residual_mix
                } else {
                    *sample
                };
            }
        }
    }

    pub fn reset(&mut self) {
        if let Some(model) = self.performer.as_mut() {
            model.reset();
        }
        if let Some(model) = self.residual.as_mut() {
            model.reset();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_residual_is_a_noop() {
        let mut hooks = FbmxHooks::default();
        let mut buffer = [0.25_f32, -0.5, 0.75];
        hooks.apply_residual(&mut buffer);
        assert_eq!(buffer, [0.25, -0.5, 0.75]);
        assert!(hooks.process_performer_sample(0.5).is_none());
    }
}
