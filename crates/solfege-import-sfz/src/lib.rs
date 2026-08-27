//! SFZ is imported into the Solfege model; it is never interpreted by runtime.

use solfege_zone::Instrument;

#[derive(Debug, Default)]
pub struct ImportReport {
    pub instrument: Instrument,
    pub unsupported_opcodes: Vec<String>,
    pub warnings: Vec<String>,
}

pub fn supported_initial_opcodes() -> &'static [&'static str] {
    &[
        "sample",
        "key",
        "lokey",
        "hikey",
        "lovel",
        "hivel",
        "pitch_keycenter",
        "tune",
        "volume",
        "pan",
        "loop_mode",
        "loop_start",
        "loop_end",
        "seq_length",
        "seq_position",
        "trigger",
    ]
}
