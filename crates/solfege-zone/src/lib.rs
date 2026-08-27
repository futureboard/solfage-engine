//! Authoring model and bounded note-on candidate lookup.

use std::array;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopMode {
    None,
    Forward,
    Sustain,
    ReleaseAware,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerMode {
    Attack,
    Release,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MidiRange {
    pub low: u8,
    pub high: u8,
}

impl MidiRange {
    pub const FULL: Self = Self { low: 0, high: 127 };

    pub fn new(low: u8, high: u8) -> Result<Self, ZoneError> {
        if low <= high && high <= 127 {
            Ok(Self { low, high })
        } else {
            Err(ZoneError::InvalidRange { low, high })
        }
    }

    pub const fn contains(self, value: u8) -> bool {
        value >= self.low && value <= self.high
    }
}

#[derive(Debug, Clone)]
pub struct Zone {
    pub sample_index: usize,
    pub key_range: MidiRange,
    pub velocity_range: MidiRange,
    pub root_key: u8,
    pub coarse_tune: i16,
    pub fine_tune_cents: i16,
    pub gain: f32,
    pub pan: f32,
    pub start_frame: u64,
    pub end_frame: Option<u64>,
    pub loop_start: u64,
    pub loop_end: u64,
    pub loop_mode: LoopMode,
    pub trigger: TriggerMode,
    pub round_robin_group: u16,
    pub round_robin_index: u16,
    pub probability: f32,
    pub choke_group: Option<u16>,
    pub one_shot: bool,
}

impl Zone {
    pub fn mapped_sample(sample_index: usize, root_key: u8) -> Self {
        Self {
            sample_index,
            key_range: MidiRange::FULL,
            velocity_range: MidiRange::FULL,
            root_key,
            coarse_tune: 0,
            fine_tune_cents: 0,
            gain: 1.0,
            pan: 0.0,
            start_frame: 0,
            end_frame: None,
            loop_start: 0,
            loop_end: 0,
            loop_mode: LoopMode::None,
            trigger: TriggerMode::Attack,
            round_robin_group: 0,
            round_robin_index: 0,
            probability: 1.0,
            choke_group: None,
            one_shot: false,
        }
    }

    pub fn matches(&self, note: u8, velocity: u8, trigger: TriggerMode) -> bool {
        self.trigger == trigger
            && self.key_range.contains(note)
            && self.velocity_range.contains(velocity)
            && self.probability > 0.0
    }
}

#[derive(Debug, Clone, Default)]
pub struct Group {
    pub name: String,
    pub zone_indices: Vec<usize>,
}

#[derive(Debug, Clone, Default)]
pub struct Instrument {
    pub name: String,
    pub groups: Vec<Group>,
    pub zones: Vec<Zone>,
}

/// Immutable index built off the audio thread. A note examines only its key
/// bucket; velocity and future generic conditions are evaluated on candidates.
#[derive(Debug, Clone)]
pub struct ZoneIndex {
    key_buckets: [Vec<usize>; 128],
}

impl ZoneIndex {
    pub fn build(zones: &[Zone]) -> Result<Self, ZoneError> {
        let mut key_buckets = array::from_fn(|_| Vec::new());
        for (index, zone) in zones.iter().enumerate() {
            if zone.root_key > 127 || zone.key_range.high > 127 {
                return Err(ZoneError::InvalidRange {
                    low: zone.key_range.low,
                    high: zone.key_range.high,
                });
            }
            for note in zone.key_range.low..=zone.key_range.high {
                key_buckets[note as usize].push(index);
            }
        }
        Ok(Self { key_buckets })
    }

    pub fn candidates(&self, note: u8) -> &[usize] {
        &self.key_buckets[note.min(127) as usize]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZoneError {
    InvalidRange { low: u8, high: u8 },
}

impl std::fmt::Display for ZoneError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRange { low, high } => {
                write!(formatter, "invalid MIDI range {low}..={high}")
            }
        }
    }
}

impl std::error::Error for ZoneError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_index_restricts_candidates_before_velocity_matching() {
        let mut low = Zone::mapped_sample(0, 36);
        low.key_range = MidiRange::new(36, 36).unwrap();
        low.velocity_range = MidiRange::new(0, 63).unwrap();
        let mut high = low.clone();
        high.sample_index = 1;
        high.velocity_range = MidiRange::new(64, 127).unwrap();
        let zones = vec![low, high];
        let index = ZoneIndex::build(&zones).unwrap();

        assert_eq!(index.candidates(36), &[0, 1]);
        assert!(index.candidates(37).is_empty());
        assert!(zones[index.candidates(36)[1]].matches(36, 100, TriggerMode::Attack));
    }
}
