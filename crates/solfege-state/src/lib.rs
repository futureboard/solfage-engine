//! Stable, host-safe parameter and state identifiers.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ParameterId(pub u32);

impl ParameterId {
    /// Stable FNV-1a over the versioned canonical path.
    pub const fn from_path(path: &str) -> Self {
        let bytes = path.as_bytes();
        let mut hash = 2_166_136_261_u32;
        let mut index = 0;
        while index < bytes.len() {
            hash ^= bytes[index] as u32;
            hash = hash.wrapping_mul(16_777_619);
            index += 1;
        }
        Self(hash)
    }
}

pub const MASTER_GAIN: ParameterId = ParameterId::from_path("v1/engine/master_gain");
pub const BOW_PRESSURE: ParameterId = ParameterId::from_path("v1/gesture/bow_pressure");
pub const BOW_VELOCITY: ParameterId = ParameterId::from_path("v1/gesture/bow_velocity");
pub const BOW_POSITION: ParameterId = ParameterId::from_path("v1/gesture/bow_position");
pub const VIBRATO_DEPTH: ParameterId = ParameterId::from_path("v1/gesture/vibrato_depth");
pub const CONTINUOUS_PITCH: ParameterId = ParameterId::from_path("v1/gesture/continuous_pitch");

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ParameterValue {
    pub id: ParameterId,
    pub normalized: f32,
}
