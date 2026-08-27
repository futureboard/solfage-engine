//! Library index data model. Database work is intentionally outside realtime code.

use std::path::PathBuf;

use solfege_core::SynthesisType;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Piano,
    Keys,
    Bass,
    Guitar,
    Strings,
    Brass,
    Woodwinds,
    Drums,
    Percussion,
    Synth,
    World,
    Fx,
    Vocal,
    Experimental,
}

#[derive(Debug, Clone)]
pub struct LibraryEntry {
    pub schema_version: u32,
    pub library_id: String,
    pub product: String,
    pub author: String,
    pub instrument_name: String,
    pub category: Category,
    pub tags: Vec<String>,
    pub moods: Vec<String>,
    pub osmp_path: PathBuf,
    pub preset_count: u32,
    pub required_engine_major: u16,
    pub synthesis: SynthesisType,
}
