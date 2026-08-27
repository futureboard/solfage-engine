//! Host-neutral preset state. Binary encoding is versioned separately from UI state.

use solfege_core::{GestureState, SynthesisType};
use solfege_state::ParameterValue;

#[derive(Debug, Clone)]
pub struct PresetState {
    pub schema_version: u32,
    pub name: String,
    pub instrument_id: [u8; 16],
    pub synthesis: SynthesisType,
    pub gesture: GestureState,
    pub parameters: Vec<ParameterValue>,
    pub macros: Vec<f32>,
}

impl Default for PresetState {
    fn default() -> Self {
        Self {
            schema_version: 1,
            name: String::new(),
            instrument_id: [0; 16],
            synthesis: SynthesisType::Sample,
            gesture: GestureState::default(),
            parameters: Vec::new(),
            macros: Vec::new(),
        }
    }
}
