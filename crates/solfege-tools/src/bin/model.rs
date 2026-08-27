//! Build and inspect a self-contained SFM Solfege Model.

use std::{env, fs, io::Write, path::PathBuf, process::ExitCode, time::Instant};

use solfege_audio::SampleRate;
use solfege_core::GestureControl;
use solfege_engine::{EngineConfig, SamplerEngine, SharedMetrics, sfm::SfmMode};
use solfege_event::{Event, TimedEvent};
use solfege_model::{
    ACOUSTIC_TAG, AUDIO_TAG, BODY_TAG, FBMX_RESIDUAL_TAG, METADATA_TAG, PHYSICAL_TAG,
    PhysicalProfile, SfmBuilder, SfmFile,
};

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let Some(command) = args.next() else {
        usage();
        return ExitCode::from(2);
    };
    let result = match command.as_str() {
        "build" => build(args.collect()),
        "inspect" => one_path(args.collect(), false),
        "verify" => one_path(args.collect(), true),
        "render" => render(args.collect()),
        "benchmark" => benchmark(args.collect()),
        "ablation" => ablation(args.collect()),
        _ => {
            usage();
            Err(format!("unknown command '{command}'"))
        }
    };
    match result {
        Ok(output) => {
            println!("{output}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("solfage-model: {error}");
            ExitCode::from(1)
        }
    }
}

fn benchmark(args: Vec<String>) -> Result<String, String> {
    let model_path = args.first().ok_or_else(|| "missing SFM path".to_owned())?;
    let mode = match args.get(1).map(String::as_str).unwrap_or("hybrid") {
        "physical" | "physical-only" => SfmMode::PhysicalOnly,
        "hybrid" => SfmMode::Hybrid,
        value => return Err(format!("unknown benchmark mode '{value}'")),
    };
    let blocks = args
        .get(2)
        .map(|value| value.parse::<usize>().unwrap_or(2000))
        .unwrap_or(2000)
        .max(1);
    let mut lines = vec![format!(
        "benchmark mode={} sample_rate=48000 block_frames=64 blocks={blocks}",
        match mode {
            SfmMode::PhysicalOnly => "physical",
            SfmMode::Hybrid => "hybrid",
        }
    )];
    for voice_count in [1_usize, 8, 16] {
        let sfm = SfmFile::open(model_path).map_err(|error| error.to_string())?;
        let metrics = std::sync::Arc::new(SharedMetrics::default());
        let rate = SampleRate::new(48_000.0).map_err(|error| error.to_string())?;
        let mut config = EngineConfig::realtime(rate);
        config.max_block_frames = 64;
        config.polyphony = voice_count;
        let mut engine = SamplerEngine::prepare_sfm(config, sfm, metrics, mode)
            .map_err(|error| error.to_string())?;
        for index in 0..voice_count {
            engine.handle_event(Event::NoteOn {
                note: 48 + (index as u8 % 36),
                velocity: 0.55 + (index as f32 % 4.0) * 0.1,
                note_id: index as i32 + 1,
            });
        }
        let mut block = [0.0_f32; 64];
        engine.process_interleaved(&mut block, 1, &[]);
        let start = Instant::now();
        for _ in 0..blocks {
            engine.process_interleaved(&mut block, 1, &[]);
        }
        let elapsed = start.elapsed().as_secs_f64();
        let audio_seconds = blocks as f64 * 64.0 / 48_000.0;
        let realtime_factor = audio_seconds / elapsed.max(f64::MIN_POSITIVE);
        lines.push(format!(
            "voices={voice_count} elapsed_ms={:.3} us_per_block={:.3} realtime_factor={:.3} working_memory_bytes={}",
            elapsed * 1000.0,
            elapsed * 1_000_000.0 / blocks as f64,
            realtime_factor,
            engine.working_memory_bytes(),
        ));
    }
    Ok(lines.join("\n"))
}

fn render(args: Vec<String>) -> Result<String, String> {
    let model_path = args.first().ok_or_else(|| "missing SFM path".to_owned())?;
    let output_path = args
        .get(1)
        .ok_or_else(|| "missing output WAV path".to_owned())?;
    let mode = match args.get(2).map(String::as_str).unwrap_or("hybrid") {
        "physical" | "physical-only" => SfmMode::PhysicalOnly,
        "hybrid" => SfmMode::Hybrid,
        value => return Err(format!("unknown render mode '{value}'")),
    };
    let seconds = args
        .get(3)
        .map(|value| value.parse::<f32>().unwrap_or(4.0))
        .unwrap_or(4.0)
        .max(0.25);
    let sfm = SfmFile::open(model_path).map_err(|error| error.to_string())?;
    let (sample_rate, output, report) = render_sfm(sfm, mode, seconds)?;
    write_mono_wav(output_path, sample_rate, &output)?;
    Ok(format!(
        "rendered {} mode to {} ({} frames, peak={:.6}, rms={:.6}, peak voices={})",
        match mode {
            SfmMode::PhysicalOnly => "physical",
            SfmMode::Hybrid => "hybrid",
        },
        output_path,
        output.len(),
        report.output_peak,
        report.output_rms,
        report.peak_voices,
    ))
}

fn render_sfm(
    sfm: SfmFile,
    mode: SfmMode,
    seconds: f32,
) -> Result<(u32, Vec<f32>, solfege_engine::MetricsSnapshot), String> {
    let profile = sfm.physical_profile().map_err(|error| error.to_string())?;
    let sample_rate = profile.sample_rate;
    let frames = (seconds * sample_rate as f32).round() as usize;
    let rate = SampleRate::new(sample_rate as f32).map_err(|error| error.to_string())?;
    let mut config = EngineConfig::realtime(rate);
    config.max_block_frames = 64;
    config.polyphony = 16;
    let metrics = std::sync::Arc::new(SharedMetrics::default());
    let mut engine = SamplerEngine::prepare_sfm(config, sfm, metrics.clone(), mode)
        .map_err(|error| error.to_string())?;
    let release_at = frames
        .saturating_sub(sample_rate as usize / 4)
        .max(frames * 7 / 8);
    let events = [
        (
            0,
            Event::NoteOn {
                note: 67,
                velocity: 0.8,
                note_id: 9184,
            },
        ),
        (
            frames / 8,
            Event::Expression {
                note_id: 9184,
                value: 0.7,
            },
        ),
        (
            frames / 4,
            Event::Gesture {
                note_id: 9184,
                control: GestureControl::BowPressure,
                value: 0.75,
            },
        ),
        (
            frames * 3 / 8,
            Event::Gesture {
                note_id: 9184,
                control: GestureControl::BowPosition,
                value: 0.7,
            },
        ),
        (
            frames / 2,
            Event::Pitch {
                note_id: 9184,
                hz: solfege_core::midi_note_to_hz(69),
            },
        ),
        (
            release_at,
            Event::NoteOff {
                note: 67,
                velocity: 0.0,
                note_id: 9184,
            },
        ),
    ];
    let mut output = vec![0.0_f32; frames];
    let mut frame = 0;
    let mut event_index = 0;
    while frame < frames {
        let block_frames = (frames - frame).min(config.max_block_frames);
        let mut block_events = Vec::with_capacity(events.len());
        while let Some((at, event)) = events.get(event_index).copied() {
            if at >= frame + block_frames {
                break;
            }
            if at >= frame {
                block_events.push(TimedEvent::at_sample((at - frame) as u32, event));
            }
            event_index += 1;
        }
        engine.process_interleaved(&mut output[frame..frame + block_frames], 1, &block_events);
        frame += block_frames;
    }
    let report = engine.metrics().snapshot();
    Ok((sample_rate, output, report))
}

fn ablation(args: Vec<String>) -> Result<String, String> {
    let model_path = args.first().ok_or_else(|| "missing SFM path".to_owned())?;
    let output_dir = PathBuf::from(
        args.get(1)
            .ok_or_else(|| "missing ablation output directory".to_owned())?,
    );
    let seconds = args
        .get(2)
        .map(|value| value.parse::<f32>().unwrap_or(1.0))
        .unwrap_or(1.0)
        .max(0.25);
    fs::create_dir_all(&output_dir).map_err(|error| error.to_string())?;
    let full = SfmFile::open(model_path).map_err(|error| error.to_string())?;
    if full.section(ACOUSTIC_TAG).is_none() || full.section(AUDIO_TAG).is_none() {
        return Err("ablation requires a complete SFM with ACOU and AUDO".to_owned());
    }
    let (sample_rate, reference, _) = render_sfm(full.clone(), SfmMode::Hybrid, seconds)?;
    write_mono_wav(
        &output_dir.join("full.wav").to_string_lossy(),
        sample_rate,
        &reference,
    )?;
    let variants = [
        ("remove_acou", Some(ACOUSTIC_TAG), None),
        ("remove_audo", Some(AUDIO_TAG), None),
        ("mutate_acou", None, Some(ACOUSTIC_TAG)),
        ("mutate_audo", None, Some(AUDIO_TAG)),
    ];
    let mut reports = Vec::new();
    for (name, removed, mutated) in variants {
        let variant = rebuild_variant(&full, removed, mutated)?;
        let (variant_rate, samples, _) = render_sfm(variant, SfmMode::Hybrid, seconds)?;
        if variant_rate != sample_rate || samples.len() != reference.len() {
            return Err(format!("ablation variant {name} changed render dimensions"));
        }
        let mut max_abs_diff = 0.0_f32;
        let mut sum_squared = 0.0_f64;
        for (left, right) in reference.iter().zip(&samples) {
            let diff = *left - *right;
            max_abs_diff = max_abs_diff.max(diff.abs());
            sum_squared += (diff as f64) * (diff as f64);
        }
        let rms_diff = (sum_squared / samples.len().max(1) as f64).sqrt() as f32;
        let output_path = output_dir.join(format!("{name}.wav"));
        write_mono_wav(&output_path.to_string_lossy(), sample_rate, &samples)?;
        reports.push(serde_json::json!({
            "variant": name,
            "removed_section": removed.map(|tag| String::from_utf8_lossy(&tag).to_string()),
            "mutated_section": mutated.map(|tag| String::from_utf8_lossy(&tag).to_string()),
            "max_abs_diff_vs_full": max_abs_diff,
            "rms_diff_vs_full": rms_diff,
            "measurably_changed": max_abs_diff > 1.0e-6 && rms_diff > 1.0e-7,
        }));
    }
    let report = serde_json::json!({
        "format": "solfege-sfm-ablation",
        "sample_rate": sample_rate,
        "seconds": seconds,
        "reference": "full hybrid render with ACOU and AUDO",
        "variants": reports,
    });
    let report_path = output_dir.join("report.json");
    fs::write(&report_path, serde_json::to_vec_pretty(&report).unwrap())
        .map_err(|error| error.to_string())?;
    Ok(serde_json::to_string_pretty(&report).unwrap())
}

fn rebuild_variant(
    source: &SfmFile,
    removed: Option<[u8; 4]>,
    mutated: Option<[u8; 4]>,
) -> Result<SfmFile, String> {
    let mut builder = SfmBuilder::new().flags(source.flags());
    for entry in source.sections() {
        if removed == Some(entry.tag) {
            continue;
        }
        let mut payload = source
            .section(entry.tag)
            .ok_or_else(|| format!("missing source section {}", entry.name()))?
            .to_vec();
        if mutated == Some(entry.tag) {
            mutate_section(entry.tag, &mut payload)?;
        }
        builder = builder
            .add_section(entry.tag, entry.flags, payload)
            .map_err(|error| error.to_string())?;
    }
    let bytes = builder.build().map_err(|error| error.to_string())?;
    SfmFile::from_bytes(&bytes).map_err(|error| error.to_string())
}

fn mutate_section(tag: [u8; 4], payload: &mut [u8]) -> Result<(), String> {
    if tag == ACOUSTIC_TAG {
        let profile_count = u32::from_le_bytes(
            payload
                .get(12..16)
                .ok_or_else(|| "ACOU profile count is missing".to_owned())?
                .try_into()
                .map_err(|_| "ACOU profile count is invalid".to_owned())?,
        ) as usize;
        for profile_index in 0..profile_count {
            let offset = solfege_model::acoustic::ACOUSTIC_HEADER_SIZE
                + profile_index * solfege_model::acoustic::ACOUSTIC_PROFILE_SIZE
                + 300
                + 4;
            let current = payload
                .get(offset..offset + 4)
                .ok_or_else(|| "ACOU body mode is missing".to_owned())?;
            let current = f32::from_le_bytes([current[0], current[1], current[2], current[3]]);
            let replacement = (current * 1.75 + 0.25).clamp(0.01, 4.0);
            payload[offset..offset + 4].copy_from_slice(&replacement.to_le_bytes());
        }
        Ok(())
    } else if tag == AUDIO_TAG {
        let payload_offset = u64::from_le_bytes(
            payload
                .get(36..44)
                .ok_or_else(|| "AUDO payload offset is missing".to_owned())?
                .try_into()
                .map_err(|_| "AUDO payload offset is invalid".to_owned())?,
        ) as usize;
        let samples = payload
            .get_mut(payload_offset..)
            .ok_or_else(|| "AUDO samples are missing".to_owned())?;
        for chunk in samples.chunks_exact_mut(2) {
            let value = i16::from_le_bytes([chunk[0], chunk[1]]);
            chunk.copy_from_slice(&value.saturating_neg().to_le_bytes());
        }
        Ok(())
    } else {
        Err(format!(
            "cannot mutate unsupported section {}",
            String::from_utf8_lossy(&tag)
        ))
    }
}

fn write_mono_wav(path: &str, sample_rate: u32, samples: &[f32]) -> Result<(), String> {
    let data_len = samples
        .len()
        .checked_mul(2)
        .ok_or_else(|| "WAV data length overflow".to_owned())?;
    let riff_len = 36_usize
        .checked_add(data_len)
        .ok_or_else(|| "WAV RIFF length overflow".to_owned())?;
    let mut bytes = Vec::with_capacity(44 + data_len);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(riff_len as u32).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&sample_rate.to_le_bytes());
    bytes.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    bytes.extend_from_slice(&2_u16.to_le_bytes());
    bytes.extend_from_slice(&16_u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&(data_len as u32).to_le_bytes());
    for &sample in samples {
        bytes.extend_from_slice(
            &((sample.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16).to_le_bytes(),
        );
    }
    let mut file = fs::File::create(path).map_err(|error| error.to_string())?;
    file.write_all(&bytes).map_err(|error| error.to_string())
}

fn build(args: Vec<String>) -> Result<String, String> {
    let output = required_option(&args, "--output")?;
    let physical_path = option(&args, "--physical");
    let residual_path = option(&args, "--fbmx");
    let metadata_path = option(&args, "--metadata");
    let audio_path = option(&args, "--audio");
    let acoustic_path = option(&args, "--acoustic");

    let profile = if let Some(path) = physical_path {
        let bytes = fs::read(&path).map_err(|error| format!("{path}: {error}"))?;
        serde_json::from_slice::<PhysicalProfile>(&bytes)
            .map_err(|error| format!("{path}: invalid physical profile JSON: {error}"))?
    } else {
        PhysicalProfile::default()
    };
    profile.validate().map_err(|error| error.to_string())?;

    let metadata = if let Some(path) = metadata_path {
        let bytes = fs::read(&path).map_err(|error| format!("{path}: {error}"))?;
        serde_json::from_slice::<serde_json::Value>(&bytes)
            .map_err(|error| format!("{path}: invalid metadata JSON: {error}"))?
    } else {
        serde_json::json!({
            "name": PathBuf::from(&output)
                .file_stem()
                .and_then(|name| name.to_str())
                .unwrap_or("SFM instrument"),
            "format": "sfm",
            "format_version": 1,
            "model_source_type": "hybrid",
            "validated": false,
            "notes": "Physical baseline plus optional embedded FBMX residual."
        })
    };
    let metadata_bytes = serde_json::to_vec_pretty(&metadata).map_err(|error| error.to_string())?;
    let body_bytes =
        serde_json::to_vec_pretty(&profile.body_modes).map_err(|error| error.to_string())?;
    let mut builder = SfmBuilder::new()
        .add_section(
            PHYSICAL_TAG,
            0,
            profile.to_json_bytes().map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?
        .add_section(BODY_TAG, 0, body_bytes)
        .map_err(|error| error.to_string())?
        .add_section(METADATA_TAG, 0, metadata_bytes)
        .map_err(|error| error.to_string())?;
    if let Some(path) = residual_path {
        builder = builder
            .add_section(
                FBMX_RESIDUAL_TAG,
                0,
                fs::read(&path).map_err(|error| format!("{path}: {error}"))?,
            )
            .map_err(|error| error.to_string())?;
    }
    if let Some(path) = acoustic_path {
        builder = builder
            .add_section(
                ACOUSTIC_TAG,
                0,
                fs::read(&path).map_err(|error| format!("{path}: {error}"))?,
            )
            .map_err(|error| error.to_string())?;
    }
    if let Some(path) = audio_path {
        builder = builder
            .add_section(
                AUDIO_TAG,
                0,
                fs::read(&path).map_err(|error| format!("{path}: {error}"))?,
            )
            .map_err(|error| error.to_string())?;
    }
    let bytes = builder.build().map_err(|error| error.to_string())?;
    let output_path = PathBuf::from(&output);
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    fs::write(&output_path, &bytes).map_err(|error| error.to_string())?;
    let file = SfmFile::from_bytes(&bytes).map_err(|error| error.to_string())?;
    Ok(format!(
        "built {} ({} bytes, {} sections)",
        output_path.display(),
        bytes.len(),
        file.sections().len()
    ))
}

fn one_path(args: Vec<String>, verify_only: bool) -> Result<String, String> {
    let path = args.first().ok_or_else(|| "missing SFM path".to_owned())?;
    let file = SfmFile::open(path).map_err(|error| error.to_string())?;
    let acoustic_present = file.section(ACOUSTIC_TAG).is_some();
    let audio_present = file.section(AUDIO_TAG).is_some();
    let acoustic = file.acoustic_model().map_err(|error| error.to_string())?;
    if verify_only {
        if acoustic_present != audio_present {
            return Err("ACOU and AUDO must be present together".to_owned());
        }
        let asset_line = acoustic.as_ref().map_or_else(
            || "acoustic_assets=none (physical-only fallback)".to_owned(),
            |assets| {
                format!(
                    "acoustic_assets=profiles:{} segments:{} pcm_samples:{} source_files:{} source_frames:{}",
                    assets.profiles().len(),
                    assets.segments().len(),
                    assets.samples().len(),
                    assets.source_file_count(),
                    assets.source_frame_count(),
                )
            },
        );
        return Ok(format!(
            "verified {} ({} sections)\n{}",
            path,
            file.sections().len(),
            asset_line
        ));
    }
    let metadata = file.metadata_json().map_err(|error| error.to_string())?;
    let profile = file.physical_profile().map_err(|error| error.to_string())?;
    let section_list = file
        .sections()
        .iter()
        .map(|entry| entry.name())
        .collect::<Vec<_>>()
        .join(", ");
    let asset_line = acoustic.as_ref().map_or_else(
        || "acoustic assets  none (physical-only fallback)".to_owned(),
        |assets| {
            format!(
                "acoustic assets  profiles={} segments={} pcm_samples={} source_files={} source_frames={}",
                assets.profiles().len(),
                assets.segments().len(),
                assets.samples().len(),
                assets.source_file_count(),
                assets.source_frame_count(),
            )
        },
    );
    Ok(format!(
        "SFM v1\nname           {}\nsample rate    {} Hz\nsections       {}\nphysical range {:.1}..{:.1} Hz\n{}\nmetadata       {}",
        metadata
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("(unnamed)"),
        profile.sample_rate,
        section_list,
        profile.min_frequency_hz,
        profile.max_frequency_hz,
        asset_line,
        serde_json::to_string(&metadata).unwrap_or_else(|_| "{}".to_owned()),
    ))
}

fn option(args: &[String], name: &str) -> Option<String> {
    args.windows(2)
        .find(|pair| pair[0] == name)
        .map(|pair| pair[1].clone())
}

fn required_option(args: &[String], name: &str) -> Result<String, String> {
    option(args, name).ok_or_else(|| format!("missing {name}"))
}

fn usage() {
    eprintln!("usage:");
    eprintln!(
        "  solfage-model build --output <model.sfm> [--physical <profile.json>] [--fbmx <residual.fbmx>] [--metadata <metadata.json>] [--audio <asset>] [--acoustic <asset>]"
    );
    eprintln!("  solfage-model inspect <model.sfm>");
    eprintln!("  solfage-model verify <model.sfm>");
    eprintln!("  solfage-model render <model.sfm> <output.wav> [physical|hybrid] [seconds]");
    eprintln!("  solfage-model benchmark <model.sfm> [physical|hybrid] [blocks]");
    eprintln!("  solfage-model ablation <model.sfm> <output-dir> [seconds]");
}
