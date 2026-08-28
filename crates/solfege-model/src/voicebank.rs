//! Checked, self-contained SFM neural voicebank sections.
//!
//! `INDX` is the semantic voicebank index and `AUDO` is the playable acoustic
//! payload.  The payload is canonical interleaved PCM16, not a feature summary:
//! every accepted source recording has an entry and its complete decoded audio
//! is retained.  Parsing owns the audio on the control thread so the realtime
//! renderer only performs bounded table lookups and sample reads.

use crate::{Result, SfmError};

pub const INDEX_MAGIC: [u8; 4] = *b"VBI1";
pub const AUDIO_MAGIC: [u8; 4] = *b"AUO2";
pub const ASSET_VERSION: u16 = 1;
pub const INDEX_HEADER_SIZE: usize = 64;
pub const INDEX_LOOKUP_STRIDE: usize = 8;
pub const VOICEBANK_ENTRY_SIZE: usize = 120;
pub const AUDIO_HEADER_SIZE: usize = 64;
pub const MAX_ARTICULATIONS: usize = 8;
pub const MAX_DYNAMICS: usize = 8;
pub const MAX_ENTRIES: usize = 65_535;
pub const MAX_AUDIO_SAMPLES: usize = 512 * 1024 * 1024;
pub const LOOKUP_SLOT_COUNT: usize = MAX_ARTICULATIONS * MAX_DYNAMICS * 128;

pub const ARTICULATION_SUSTAIN_VIBRATO: u8 = 0;
pub const ARTICULATION_PIZZICATO: u8 = 1;
pub const ARTICULATION_SPICCATO: u8 = 2;
pub const ARTICULATION_TREMOLO: u8 = 3;

pub const DYNAMIC_P: u8 = 0;
pub const DYNAMIC_V1: u8 = 1;
pub const DYNAMIC_V2: u8 = 2;
pub const DYNAMIC_F: u8 = 3;

const FLAG_LOOP: u16 = 1;
const NO_ROUND_ROBIN: u16 = u16::MAX;
const PCM16_TO_F32: f32 = 1.0 / 32_768.0;
const LOOP_CROSSFADE_FRAMES: u64 = 96;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameRange {
    pub start: u64,
    pub end: u64,
}

impl FrameRange {
    pub const fn new(start: u64, end: u64) -> Option<Self> {
        if end > start {
            Some(Self { start, end })
        } else {
            None
        }
    }

    pub const fn len(self) -> u64 {
        self.end.saturating_sub(self.start)
    }
}

/// One playable source recording in the embedded voicebank.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VoicebankEntry {
    pub id: u32,
    pub midi_note: u8,
    pub root_pitch_hz: f32,
    pub articulation: u8,
    pub dynamic: u8,
    pub dynamic_value: f32,
    pub round_robin: Option<u16>,
    /// Byte offset relative to the `AUDO` PCM payload.
    pub audio_offset: u64,
    pub audio_size: u64,
    pub frame_count: u64,
    pub sample_rate: u32,
    pub channels: u16,
    pub loop_region: Option<FrameRange>,
    pub attack_region: Option<FrameRange>,
    pub sustain_region: Option<FrameRange>,
    pub release_region: Option<FrameRange>,
}

impl VoicebankEntry {
    pub fn validate(&self, audio_bytes: usize, sample_rate: u32, channels: u16) -> Result<()> {
        if self.articulation as usize >= MAX_ARTICULATIONS
            || self.dynamic as usize >= MAX_DYNAMICS
            || self.midi_note > 127
            || !self.root_pitch_hz.is_finite()
            || self.root_pitch_hz <= 0.0
            || !self.dynamic_value.is_finite()
            || !(0.0..=1.0).contains(&self.dynamic_value)
            || self.frame_count == 0
            || self.sample_rate != sample_rate
            || self.channels != channels
            || self.audio_size != self.frame_count.saturating_mul(channels as u64 * 2)
        {
            return Err(invalid("voicebank entry contains invalid values"));
        }
        let end = self
            .audio_offset
            .checked_add(self.audio_size)
            .ok_or_else(|| invalid("voicebank entry audio range overflows"))?;
        if end > audio_bytes as u64 {
            return Err(invalid("voicebank entry audio range is outside AUDO"));
        }
        for (name, range) in [
            ("loop", self.loop_region),
            ("attack", self.attack_region),
            ("sustain", self.sustain_region),
            ("release", self.release_region),
        ] {
            if let Some(range) = range
                && (range.end > self.frame_count || range.end <= range.start)
            {
                return Err(invalid(format!("{name} region is outside entry frames")));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LookupRange {
    start: u32,
    count: u32,
}

impl LookupRange {
    const EMPTY: Self = Self { start: 0, count: 0 };
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VoicebankSelection {
    pub primary: usize,
    /// A second entry to blend into `primary`, and how much of it.
    ///
    /// **Only ever set for entries that are the same recording**, because
    /// summing two entries is only meaningful when they are phase-coherent.
    /// See [`VoicebankModel::resolve`] for why dynamic layers are not blended.
    pub secondary: Option<usize>,
    /// Equal-power blend amount for `secondary`.
    pub secondary_mix: f32,
}

/// Fully prepared voicebank data.  All allocations happen while the SFM is
/// loaded; `resolve` and `sample` are allocation-free realtime operations.
#[derive(Debug, Clone)]
pub struct VoicebankModel {
    sample_rate: u32,
    channels: u16,
    articulation_mask: u16,
    entries: Vec<VoicebankEntry>,
    lookup: Vec<LookupRange>,
    samples: Vec<i16>,
    source_file_count: u32,
    source_frame_count: u64,
    decoded_frame_count: u64,
}

impl VoicebankModel {
    pub fn from_sections(index: &[u8], audio: &[u8]) -> Result<Self> {
        let parsed_index = parse_index(index)?;
        let (audio_sample_rate, audio_channels, source_files, source_frames, samples) =
            parse_audio(audio, parsed_index.entry_count)?;
        if parsed_index.sample_rate != audio_sample_rate
            || parsed_index.channels != audio_channels
            || parsed_index.source_file_count != source_files
            || parsed_index.source_frame_count != source_frames
        {
            return Err(invalid("INDX and AUDO headers disagree"));
        }
        for entry in &parsed_index.entries {
            entry.validate(
                samples.len() * std::mem::size_of::<i16>(),
                parsed_index.sample_rate,
                parsed_index.channels,
            )?;
        }
        let articulation_mask = parsed_index.entries.iter().fold(0_u16, |mask, entry| {
            if entry.articulation < 16 {
                mask | (1_u16 << entry.articulation)
            } else {
                mask
            }
        });
        let decoded_frame_count = samples.len() as u64 / parsed_index.channels as u64;
        Ok(Self {
            sample_rate: parsed_index.sample_rate,
            channels: parsed_index.channels,
            articulation_mask,
            entries: parsed_index.entries,
            lookup: parsed_index.lookup,
            samples,
            source_file_count: source_files,
            source_frame_count: source_frames,
            decoded_frame_count,
        })
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }

    pub fn entries(&self) -> &[VoicebankEntry] {
        &self.entries
    }

    pub fn samples(&self) -> &[i16] {
        &self.samples
    }

    pub fn source_file_count(&self) -> u32 {
        self.source_file_count
    }

    pub fn source_frame_count(&self) -> u64 {
        self.source_frame_count
    }

    pub fn decoded_frame_count(&self) -> u64 {
        self.decoded_frame_count
    }

    pub fn audio_bytes(&self) -> usize {
        self.samples.len() * std::mem::size_of::<i16>()
    }

    pub fn decoded_duration_seconds(&self) -> f64 {
        self.decoded_frame_count as f64 / self.sample_rate.max(1) as f64
    }

    /// Resolve pitch, articulation, dynamic layer, and round-robin using the
    /// prebuilt fixed lookup table.  It never scans the audio or allocates; at
    /// most the eight dynamic lookup planes are consulted.
    ///
    /// **Dynamic layers are chosen, not crossfaded.** Each layer is an
    /// independent recording of the note: its own bow stroke, its own vibrato
    /// phase, its own noise. Summing two of them is summing two decorrelated
    /// signals of similar level, which is a comb filter whose notches move with
    /// the difference in their vibrato — heard as chorus, phasiness, and a
    /// "watery" sustain, and measurable as deep amplitude modulation of every
    /// harmonic. Crossfading velocity layers is a common sampler technique, but
    /// it is only defensible when the layers are the same take at different
    /// levels; these are not.
    ///
    /// Choosing the nearest layer instead makes a held note as steady as the
    /// recording it came from. Dynamics stay continuous because level and
    /// expression are applied on top of the chosen layer; only the *timbre*
    /// snaps between layers, and it snaps at note-on, where no one can hear a
    /// change that never happens mid-note.
    pub fn resolve(
        &self,
        midi_note: u8,
        articulation: u8,
        dynamic_value: f32,
        round_robin_seed: u16,
    ) -> Option<VoicebankSelection> {
        let articulation = if self.has_articulation(articulation) {
            articulation
        } else {
            ARTICULATION_SUSTAIN_VIBRATO
        };
        let dynamic_value = if dynamic_value.is_finite() {
            dynamic_value.clamp(0.0, 1.0)
        } else {
            0.5
        };
        let mut lower: Option<(usize, f32)> = None;
        let mut upper: Option<(usize, f32)> = None;
        for dynamic in 0..MAX_DYNAMICS as u8 {
            let Some(range) = self.nearest_range(articulation, dynamic, midi_note) else {
                continue;
            };
            let index = range.start as usize + round_robin_seed as usize % range.count as usize;
            let entry = self.entries[index];
            if entry.dynamic_value <= dynamic_value {
                if lower.is_none_or(|(_, value)| entry.dynamic_value > value) {
                    lower = Some((index, entry.dynamic_value));
                }
            } else if upper.is_none_or(|(_, value)| entry.dynamic_value < value) {
                upper = Some((index, entry.dynamic_value));
            }
        }
        // Nearest layer wins. On a tie the quieter one is kept, so a value
        // exactly on a boundary resolves the same way every time rather than
        // depending on lookup order.
        let primary = match (lower, upper) {
            (Some(low), Some(high)) => {
                if (dynamic_value - low.1).abs() <= (high.1 - dynamic_value).abs() {
                    low.0
                } else {
                    high.0
                }
            }
            (Some(low), None) => low.0,
            (None, Some(high)) => high.0,
            (None, None) => return None,
        };
        Some(VoicebankSelection {
            primary,
            secondary: None,
            secondary_mix: 0.0,
        })
    }

    /// Read one smoothly interpolated sample from an entry. The frame
    /// position is relative to that entry's source audio and the channel is
    /// folded by the caller when the output is mono.
    #[inline]
    pub fn sample(&self, entry_index: usize, position: f64, channel: usize) -> f32 {
        let Some(entry) = self.entries.get(entry_index) else {
            return 0.0;
        };
        sample_entry_looped(&self.samples, entry, position, channel)
    }

    /// Read a stereo frame using the already-copied entry metadata. The
    /// renderer uses this fast path so the audio callback does not repeatedly
    /// look up entries, recalculate byte offsets, or perform bounds checks.
    #[inline]
    pub fn sample_stereo_entry(&self, entry: VoicebankEntry, position: f64) -> [f32; 2] {
        [
            sample_entry_looped(&self.samples, &entry, position, 0),
            sample_entry_looped(&self.samples, &entry, position, 1),
        ]
    }

    fn has_articulation(&self, articulation: u8) -> bool {
        articulation < 16 && self.articulation_mask & (1_u16 << articulation) != 0
    }

    fn nearest_range(&self, articulation: u8, dynamic: u8, midi_note: u8) -> Option<LookupRange> {
        for distance in 0..=127_u16 {
            let down = midi_note as i16 - distance as i16;
            if down >= 0 {
                let range = self.lookup[lookup_slot(articulation, dynamic, down as u8)];
                if range.count != 0 {
                    return Some(range);
                }
            }
            let up = midi_note as u16 + distance;
            if distance != 0 && up <= 127 {
                let range = self.lookup[lookup_slot(articulation, dynamic, up as u8)];
                if range.count != 0 {
                    return Some(range);
                }
            }
        }
        None
    }
}

/// Read one interpolated sample.
///
/// `position` is `f64` on purpose. A recorded sustain here is ~15 s — about
/// 720_000 frames — and at that magnitude an `f32` position has a spacing of
/// 1/16 of a sample. The interpolation fraction would then take only 16
/// distinct values and the effective read step would quantise to the same grid,
/// which is broadband distortion on every sample of a held note and gets worse
/// the deeper into the recording the cursor travels. `f64` has 52 mantissa bits,
/// so the fraction stays exact for any recording length this format allows.
#[inline]
fn sample_entry(samples: &[i16], entry: &VoicebankEntry, position: f64, channel: usize) -> f32 {
    if !position.is_finite() || position < 0.0 || position >= entry.frame_count as f64 {
        return 0.0;
    }
    let frame = position as usize;
    let fraction = (position - frame as f64) as f32;
    let center = frame as isize;
    let frame0 = neighbor_frame(entry, center, -1);
    let frame1 = neighbor_frame(entry, center, 0);
    let frame2 = neighbor_frame(entry, center, 1);
    let frame3 = neighbor_frame(entry, center, 2);
    let channel = channel.min(entry.channels.saturating_sub(1) as usize);
    let base = entry.audio_offset as usize / std::mem::size_of::<i16>();
    let channels = entry.channels as usize;
    let read = |source_frame: usize| {
        samples[base + source_frame * channels + channel] as f32 * PCM16_TO_F32
    };
    let y0 = read(frame0);
    let y1 = read(frame1);
    let y2 = read(frame2);
    let y3 = read(frame3);
    // Four-point Hermite interpolation is noticeably smoother than linear
    // interpolation when a recorded note is repitched. The clamp prevents
    // a steep transient from overshooting the PCM source.
    let c0 = y1;
    let c1 = 0.5 * (y2 - y0);
    let c2 = y0 - 2.5 * y1 + 2.0 * y2 - 0.5 * y3;
    let c3 = 0.5 * (y3 - y0) + 1.5 * (y1 - y2);
    (((c3 * fraction + c2) * fraction + c1) * fraction + c0).clamp(-1.0, 1.0)
}

#[inline]
fn sample_entry_looped(
    samples: &[i16],
    entry: &VoicebankEntry,
    position: f64,
    channel: usize,
) -> f32 {
    let value = sample_entry(samples, entry, position, channel);
    let Some(loop_region) = entry.loop_region else {
        return value;
    };
    let loop_len = loop_region.len();
    let crossfade = LOOP_CROSSFADE_FRAMES.min(loop_len / 2);
    let crossfade_start = loop_region.end.saturating_sub(crossfade);
    // The crossfade belongs to the approach to the loop point and to nothing
    // else. Without the upper bound it also fires for every position at or past
    // `loop_end`, which is exactly where a released note lives: the recorded
    // release tail would be replaced by the loop head, so a note-off jumped
    // back into the sustain instead of playing the release it was given.
    if crossfade == 0 || position < crossfade_start as f64 || position >= loop_region.end as f64 {
        return value;
    }
    let amount = ((position - crossfade_start as f64) / crossfade as f64).clamp(0.0, 1.0) as f32;
    let head = sample_entry(
        samples,
        entry,
        loop_region.start as f64 + (position - crossfade_start as f64),
        channel,
    );
    value * (1.0 - amount) + head * amount
}

#[inline]
fn neighbor_frame(entry: &VoicebankEntry, center: isize, offset: isize) -> usize {
    let last = entry.frame_count.saturating_sub(1) as isize;
    let candidate = center + offset;
    if let Some(loop_region) = entry.loop_region
        && center >= loop_region.start as isize
        && center < loop_region.end as isize
    {
        let loop_start = loop_region.start as isize;
        let loop_len = loop_region.len() as isize;
        return (loop_start + (candidate - loop_start).rem_euclid(loop_len)) as usize;
    }
    candidate.clamp(0, last) as usize
}

#[derive(Debug)]
struct ParsedIndex {
    sample_rate: u32,
    channels: u16,
    entry_count: usize,
    entries: Vec<VoicebankEntry>,
    lookup: Vec<LookupRange>,
    source_file_count: u32,
    source_frame_count: u64,
}

pub fn encode_index(
    entries: &[VoicebankEntry],
    sample_rate: u32,
    channels: u16,
    source_file_count: u32,
    source_frame_count: u64,
) -> Result<Vec<u8>> {
    if entries.is_empty() || entries.len() > MAX_ENTRIES || sample_rate == 0 || channels == 0 {
        return Err(invalid("cannot encode empty or oversized voicebank"));
    }
    let mut entries = entries.to_vec();
    entries.sort_by_key(|entry| {
        (
            entry.articulation,
            entry.dynamic,
            entry.midi_note,
            entry.round_robin.unwrap_or(0),
            entry.id,
        )
    });
    for entry in &entries {
        entry.validate(usize::MAX, sample_rate, channels)?;
    }
    let lookup_offset = INDEX_HEADER_SIZE;
    let entry_offset = lookup_offset
        .checked_add(LOOKUP_SLOT_COUNT * INDEX_LOOKUP_STRIDE)
        .ok_or_else(|| invalid("voicebank lookup size overflows"))?;
    let file_size = entry_offset
        .checked_add(entries.len() * VOICEBANK_ENTRY_SIZE)
        .ok_or_else(|| invalid("voicebank index size overflows"))?;
    let mut bytes = vec![0_u8; file_size];
    bytes[0..4].copy_from_slice(&INDEX_MAGIC);
    bytes[4..6].copy_from_slice(&ASSET_VERSION.to_le_bytes());
    bytes[6..8].copy_from_slice(&(INDEX_HEADER_SIZE as u16).to_le_bytes());
    bytes[8..12].copy_from_slice(&sample_rate.to_le_bytes());
    bytes[12..14].copy_from_slice(&channels.to_le_bytes());
    bytes[16..20].copy_from_slice(&(entries.len() as u32).to_le_bytes());
    bytes[20..24].copy_from_slice(&(VOICEBANK_ENTRY_SIZE as u32).to_le_bytes());
    bytes[24..28].copy_from_slice(&(LOOKUP_SLOT_COUNT as u32).to_le_bytes());
    bytes[28..32].copy_from_slice(&(INDEX_LOOKUP_STRIDE as u32).to_le_bytes());
    bytes[32..40].copy_from_slice(&(lookup_offset as u64).to_le_bytes());
    bytes[40..48].copy_from_slice(&(entry_offset as u64).to_le_bytes());
    bytes[48..52].copy_from_slice(&source_file_count.to_le_bytes());
    bytes[56..64].copy_from_slice(&source_frame_count.to_le_bytes());

    let mut lookup = vec![LookupRange::EMPTY; LOOKUP_SLOT_COUNT];
    for (index, entry) in entries.iter().enumerate() {
        let slot = lookup_slot(entry.articulation, entry.dynamic, entry.midi_note);
        if lookup[slot].count == 0 {
            lookup[slot].start = index as u32;
        }
        lookup[slot].count += 1;
        encode_entry(
            &mut bytes[entry_offset + index * VOICEBANK_ENTRY_SIZE..],
            entry,
        );
    }
    for (slot, range) in lookup.iter().enumerate() {
        let offset = lookup_offset + slot * INDEX_LOOKUP_STRIDE;
        bytes[offset..offset + 4].copy_from_slice(&range.start.to_le_bytes());
        bytes[offset + 4..offset + 8].copy_from_slice(&range.count.to_le_bytes());
    }
    Ok(bytes)
}

pub fn encode_audio(
    sample_rate: u32,
    channels: u16,
    entry_count: usize,
    source_file_count: u32,
    source_frame_count: u64,
    samples: &[i16],
) -> Result<Vec<u8>> {
    if sample_rate == 0
        || channels == 0
        || entry_count == 0
        || entry_count > MAX_ENTRIES
        || samples.is_empty()
        || !samples.len().is_multiple_of(channels as usize)
        || samples.len() > MAX_AUDIO_SAMPLES
    {
        return Err(invalid("invalid AUDO dimensions"));
    }
    let payload_bytes = samples
        .len()
        .checked_mul(std::mem::size_of::<i16>())
        .ok_or_else(|| invalid("AUDO payload size overflows"))?;
    let payload_offset = AUDIO_HEADER_SIZE;
    let mut bytes = vec![0_u8; payload_offset + payload_bytes];
    bytes[0..4].copy_from_slice(&AUDIO_MAGIC);
    bytes[4..6].copy_from_slice(&ASSET_VERSION.to_le_bytes());
    bytes[6..8].copy_from_slice(&(AUDIO_HEADER_SIZE as u16).to_le_bytes());
    bytes[8..12].copy_from_slice(&sample_rate.to_le_bytes());
    bytes[12..14].copy_from_slice(&channels.to_le_bytes());
    bytes[14..16].copy_from_slice(&1_u16.to_le_bytes());
    bytes[16..20].copy_from_slice(&(entry_count as u32).to_le_bytes());
    bytes[20..24].copy_from_slice(&source_file_count.to_le_bytes());
    bytes[24..32].copy_from_slice(&source_frame_count.to_le_bytes());
    bytes[32..40].copy_from_slice(&(payload_offset as u64).to_le_bytes());
    bytes[40..48].copy_from_slice(&(payload_bytes as u64).to_le_bytes());
    bytes[48..56].copy_from_slice(&(samples.len() as u64 / channels as u64).to_le_bytes());
    for (index, sample) in samples.iter().enumerate() {
        let offset = payload_offset + index * 2;
        bytes[offset..offset + 2].copy_from_slice(&sample.to_le_bytes());
    }
    Ok(bytes)
}

fn parse_index(raw: &[u8]) -> Result<ParsedIndex> {
    if raw.len() < INDEX_HEADER_SIZE {
        return Err(invalid("INDX header is truncated"));
    }
    if raw[0..4] != INDEX_MAGIC
        || read_u16(raw, 4)? != ASSET_VERSION
        || read_u16(raw, 6)? as usize != INDEX_HEADER_SIZE
    {
        return Err(invalid("INDX magic, version, or header size is invalid"));
    }
    let sample_rate = read_u32(raw, 8)?;
    let channels = read_u16(raw, 12)?;
    let entry_count = read_u32(raw, 16)? as usize;
    let entry_stride = read_u32(raw, 20)? as usize;
    let lookup_count = read_u32(raw, 24)? as usize;
    let lookup_stride = read_u32(raw, 28)? as usize;
    let lookup_offset = read_u64(raw, 32)? as usize;
    let entry_offset = read_u64(raw, 40)? as usize;
    let source_file_count = read_u32(raw, 48)?;
    let source_frame_count = read_u64(raw, 56)?;
    if sample_rate == 0
        || channels == 0
        || entry_count == 0
        || entry_count > MAX_ENTRIES
        || entry_stride != VOICEBANK_ENTRY_SIZE
        || lookup_count != LOOKUP_SLOT_COUNT
        || lookup_stride != INDEX_LOOKUP_STRIDE
        || lookup_offset != INDEX_HEADER_SIZE
    {
        return Err(invalid("INDX dimensions or count are invalid"));
    }
    let lookup_end = lookup_offset
        .checked_add(lookup_count * lookup_stride)
        .ok_or_else(|| invalid("INDX lookup table overflows"))?;
    if entry_offset != lookup_end {
        return Err(invalid("INDX entry offset does not follow lookup table"));
    }
    let entries_end = entry_offset
        .checked_add(entry_count * entry_stride)
        .ok_or_else(|| invalid("INDX entry table overflows"))?;
    if entries_end != raw.len() {
        return Err(invalid("INDX length does not match entry table"));
    }
    let mut lookup = Vec::with_capacity(lookup_count);
    for index in 0..lookup_count {
        let offset = lookup_offset + index * lookup_stride;
        let range = LookupRange {
            start: read_u32(raw, offset)?,
            count: read_u32(raw, offset + 4)?,
        };
        let end = range.start as usize + range.count as usize;
        if end > entry_count {
            return Err(invalid("INDX lookup range exceeds entries"));
        }
        lookup.push(range);
    }
    let mut entries = Vec::with_capacity(entry_count);
    for index in 0..entry_count {
        let offset = entry_offset + index * entry_stride;
        let entry = decode_entry(raw, offset)?;
        entry.validate(usize::MAX, sample_rate, channels)?;
        entries.push(entry);
    }
    for (index, entry) in entries.iter().enumerate() {
        let range = lookup[lookup_slot(entry.articulation, entry.dynamic, entry.midi_note)];
        if index < range.start as usize || index >= (range.start + range.count) as usize {
            return Err(invalid("INDX entry is not covered by its lookup range"));
        }
    }
    Ok(ParsedIndex {
        sample_rate,
        channels,
        entry_count,
        entries,
        lookup,
        source_file_count,
        source_frame_count,
    })
}

fn parse_audio(raw: &[u8], expected_entries: usize) -> Result<(u32, u16, u32, u64, Vec<i16>)> {
    if raw.len() < AUDIO_HEADER_SIZE {
        return Err(invalid("AUDO header is truncated"));
    }
    if raw[0..4] != AUDIO_MAGIC
        || read_u16(raw, 4)? != ASSET_VERSION
        || read_u16(raw, 6)? as usize != AUDIO_HEADER_SIZE
    {
        return Err(invalid("AUDO magic, version, or header size is invalid"));
    }
    let sample_rate = read_u32(raw, 8)?;
    let channels = read_u16(raw, 12)?;
    let encoding = read_u16(raw, 14)?;
    let entry_count = read_u32(raw, 16)? as usize;
    let source_file_count = read_u32(raw, 20)?;
    let source_frame_count = read_u64(raw, 24)?;
    let payload_offset = read_u64(raw, 32)? as usize;
    let payload_bytes = read_u64(raw, 40)? as usize;
    let decoded_frames = read_u64(raw, 48)?;
    if sample_rate == 0
        || channels == 0
        || encoding != 1
        || entry_count != expected_entries
        || payload_offset != AUDIO_HEADER_SIZE
        || payload_bytes != raw.len().saturating_sub(payload_offset)
        || payload_bytes % 2 != 0
        || !payload_bytes.is_multiple_of(channels as usize * 2)
        || decoded_frames != payload_bytes as u64 / (channels as u64 * 2)
        || payload_bytes / 2 > MAX_AUDIO_SAMPLES
    {
        return Err(invalid("AUDO dimensions or payload bounds are invalid"));
    }
    let mut samples = Vec::with_capacity(payload_bytes / 2);
    for chunk in raw[payload_offset..].chunks_exact(2) {
        samples.push(i16::from_le_bytes([chunk[0], chunk[1]]));
    }
    Ok((
        sample_rate,
        channels,
        source_file_count,
        source_frame_count,
        samples,
    ))
}

fn encode_entry(bytes: &mut [u8], entry: &VoicebankEntry) {
    bytes[0..4].copy_from_slice(&entry.id.to_le_bytes());
    bytes[4..6].copy_from_slice(&(entry.midi_note as i16).to_le_bytes());
    bytes[6] = entry.articulation;
    bytes[7] = entry.dynamic;
    bytes[8..10].copy_from_slice(
        &entry
            .round_robin
            .map_or(NO_ROUND_ROBIN, |value| value)
            .to_le_bytes(),
    );
    let flags = if entry.loop_region.is_some() {
        FLAG_LOOP
    } else {
        0
    };
    bytes[10..12].copy_from_slice(&flags.to_le_bytes());
    bytes[12..16].copy_from_slice(&entry.root_pitch_hz.to_le_bytes());
    bytes[16..20].copy_from_slice(&entry.dynamic_value.to_le_bytes());
    bytes[20..28].copy_from_slice(&entry.audio_offset.to_le_bytes());
    bytes[28..36].copy_from_slice(&entry.audio_size.to_le_bytes());
    bytes[36..44].copy_from_slice(&entry.frame_count.to_le_bytes());
    bytes[44..48].copy_from_slice(&entry.sample_rate.to_le_bytes());
    bytes[48..50].copy_from_slice(&entry.channels.to_le_bytes());
    encode_range(bytes, 52, entry.loop_region);
    encode_range(bytes, 68, entry.attack_region);
    encode_range(bytes, 84, entry.sustain_region);
    encode_range(bytes, 100, entry.release_region);
}

fn decode_entry(raw: &[u8], offset: usize) -> Result<VoicebankEntry> {
    let round_robin = read_u16(raw, offset + 8)?;
    let flags = read_u16(raw, offset + 10)?;
    let loop_region = decode_range(raw, offset + 52)?;
    if flags & FLAG_LOOP != 0 && loop_region.is_none() {
        return Err(invalid("INDX loop flag has no loop range"));
    }
    Ok(VoicebankEntry {
        id: read_u32(raw, offset)?,
        midi_note: read_i16(raw, offset + 4)? as u8,
        articulation: raw[offset + 6],
        dynamic: raw[offset + 7],
        round_robin: (round_robin != NO_ROUND_ROBIN).then_some(round_robin),
        root_pitch_hz: read_f32(raw, offset + 12)?,
        dynamic_value: read_f32(raw, offset + 16)?,
        audio_offset: read_u64(raw, offset + 20)?,
        audio_size: read_u64(raw, offset + 28)?,
        frame_count: read_u64(raw, offset + 36)?,
        sample_rate: read_u32(raw, offset + 44)?,
        channels: read_u16(raw, offset + 48)?,
        loop_region,
        attack_region: decode_range(raw, offset + 68)?,
        sustain_region: decode_range(raw, offset + 84)?,
        release_region: decode_range(raw, offset + 100)?,
    })
}

fn encode_range(bytes: &mut [u8], offset: usize, range: Option<FrameRange>) {
    let (start, end) = range.map_or((0, 0), |range| (range.start, range.end));
    bytes[offset..offset + 8].copy_from_slice(&start.to_le_bytes());
    bytes[offset + 8..offset + 16].copy_from_slice(&end.to_le_bytes());
}

fn decode_range(raw: &[u8], offset: usize) -> Result<Option<FrameRange>> {
    FrameRange::new(read_u64(raw, offset)?, read_u64(raw, offset + 8)?)
        .map_or_else(|| Ok(None), |range| Ok(Some(range)))
}

const fn lookup_slot(articulation: u8, dynamic: u8, midi_note: u8) -> usize {
    (articulation as usize * MAX_DYNAMICS + dynamic as usize) * 128 + midi_note as usize
}

fn invalid(message: impl Into<String>) -> SfmError {
    SfmError::InvalidVoicebank(message.into())
}

fn read_u16(raw: &[u8], offset: usize) -> Result<u16> {
    let bytes = raw
        .get(offset..offset + 2)
        .ok_or_else(|| invalid("voicebank integer is truncated"))?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn read_i16(raw: &[u8], offset: usize) -> Result<i16> {
    let bytes = raw
        .get(offset..offset + 2)
        .ok_or_else(|| invalid("voicebank integer is truncated"))?;
    Ok(i16::from_le_bytes([bytes[0], bytes[1]]))
}

fn read_u32(raw: &[u8], offset: usize) -> Result<u32> {
    let bytes = raw
        .get(offset..offset + 4)
        .ok_or_else(|| invalid("voicebank integer is truncated"))?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_u64(raw: &[u8], offset: usize) -> Result<u64> {
    let bytes = raw
        .get(offset..offset + 8)
        .ok_or_else(|| invalid("voicebank integer is truncated"))?;
    Ok(u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]))
}

fn read_f32(raw: &[u8], offset: usize) -> Result<f32> {
    Ok(f32::from_bits(read_u32(raw, offset)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries() -> Vec<VoicebankEntry> {
        (0..3)
            .map(|id| VoicebankEntry {
                id,
                midi_note: 60 + id as u8,
                root_pitch_hz: 261.625_55 * 2.0_f32.powf(id as f32 / 12.0),
                articulation: ARTICULATION_SUSTAIN_VIBRATO,
                dynamic: id as u8,
                dynamic_value: [0.2, 0.5, 0.8][id as usize],
                round_robin: Some(1),
                audio_offset: id as u64 * 16,
                audio_size: 16,
                frame_count: 4,
                sample_rate: 48_000,
                channels: 2,
                loop_region: FrameRange::new(1, 3),
                attack_region: FrameRange::new(0, 1),
                sustain_region: FrameRange::new(1, 3),
                release_region: FrameRange::new(3, 4),
            })
            .collect()
    }

    #[test]
    fn index_and_audio_round_trip_with_lookup() {
        let entries = entries();
        let index = encode_index(&entries, 48_000, 2, 3, 12).unwrap();
        let samples = vec![1_000_i16; 24];
        let audio = encode_audio(48_000, 2, entries.len(), 3, 12, &samples).unwrap();
        let model = VoicebankModel::from_sections(&index, &audio).unwrap();
        assert_eq!(model.entries().len(), 3);
        let selection = model.resolve(61, 0, 0.5, 0).unwrap();
        assert_eq!(model.entries()[selection.primary].midi_note, 61);
        assert!(model.sample(selection.primary, 1.5, 0) > 0.0);
    }

    /// A dynamic value between two layers must pick one, not sum both.
    ///
    /// Each dynamic layer is an independent recording — its own bow stroke, its
    /// own vibrato phase, its own noise. Summing two of them is summing two
    /// decorrelated signals of similar level: a comb filter whose notches move
    /// with the difference in their vibrato, heard as chorus and a "watery"
    /// sustain. See [`VoicebankModel::resolve`].
    #[test]
    fn a_dynamic_between_layers_selects_one_recording_rather_than_blending_two() {
        let entries = entries();
        let index = encode_index(&entries, 48_000, 2, 3, 12).unwrap();
        let audio = encode_audio(48_000, 2, entries.len(), 3, 12, &[1_000_i16; 24]).unwrap();
        let model = VoicebankModel::from_sections(&index, &audio).unwrap();
        for dynamic in [0.0_f32, 0.25, 0.5, 0.75, 1.0] {
            let selection = model
                .resolve(61, 0, dynamic, 0)
                .expect("a layer always resolves");
            assert!(
                selection.secondary.is_none() && selection.secondary_mix == 0.0,
                "dynamic {dynamic} blended two recordings"
            );
        }
    }

    /// The chosen layer is the nearest one, so dynamics still track the
    /// performance rather than snapping to one extreme.
    #[test]
    fn the_nearest_dynamic_layer_wins() {
        let entries = entries();
        let index = encode_index(&entries, 48_000, 2, 3, 12).unwrap();
        let audio = encode_audio(48_000, 2, entries.len(), 3, 12, &[1_000_i16; 24]).unwrap();
        let model = VoicebankModel::from_sections(&index, &audio).unwrap();
        let quiet = model.resolve(61, 0, 0.0, 0).unwrap();
        let loud = model.resolve(61, 0, 1.0, 0).unwrap();
        let quiet_value = model.entries()[quiet.primary].dynamic_value;
        let loud_value = model.entries()[loud.primary].dynamic_value;
        assert!(
            quiet_value <= loud_value,
            "asking for a quieter dynamic must not select a louder layer \
             ({quiet_value} vs {loud_value})"
        );
    }

    #[test]
    fn corrupt_entry_range_is_rejected() {
        let entries = entries();
        let mut index = encode_index(&entries, 48_000, 2, 3, 12).unwrap();
        let entry_offset = INDEX_HEADER_SIZE + LOOKUP_SLOT_COUNT * INDEX_LOOKUP_STRIDE;
        index[entry_offset + 28..entry_offset + 36].copy_from_slice(&999_u64.to_le_bytes());
        let audio = encode_audio(48_000, 2, entries.len(), 3, 12, &[1_000_i16; 12]).unwrap();
        assert!(VoicebankModel::from_sections(&index, &audio).is_err());
    }
}
