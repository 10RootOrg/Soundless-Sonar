use anyhow::Result;
use std::{
    fs::OpenOptions,
    io::Write,
    path::Path,
    sync::Arc,
};

use crate::{logger::Logger, prescan, decode};

/// tiny hex encoder so this file is standalone
fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// simple linear resampler (mono)
fn resample_linear_mono(x: &[f32], sr_in: u32, sr_out: u32) -> Vec<f32> {
    if x.is_empty() || sr_in == 0 || sr_out == 0 || sr_in == sr_out {
        return x.to_vec();
    }
    let ratio = (sr_out as f64) / (sr_in as f64);
    let n_out = ((x.len() as f64) * ratio).floor().max(1.0) as usize;
    let mut y = Vec::with_capacity(n_out);

    for i in 0..n_out {
        let pos = (i as f64) / ratio; // position in input
        let i0 = pos.floor() as usize;
        if i0 + 1 >= x.len() {
            y.push(*x.last().unwrap());
        } else {
            let t = (pos - (i0 as f64)) as f32; // frac
            let a = x[i0];
            let b = x[i0 + 1];
            y.push(a + (b - a) * t); // lerp
        }
    }
    y
}

/// Offline mode — analyze a local audio file directly (WAV/MP3/MP4/M4A)
/// Writes rows to `SongScan.csv` (path from CLI).
pub fn run_offline(
    cli: &crate::Config,
    meta: &crate::ScanMeta,
    logger: Arc<Logger>
) -> Result<()> {
    logger.info(&format!(
        "sonar-prescan (offline file) starting…  frame_ms={:.0} window_s={:.1} stride_ms={:.0} top_n={} min_pct={:.0}",
        cli.frame_ms,
        cli.scan_window_s,
        cli.stride_ms,
        cli.top_n,
        cli.min_percentile
    ))?;

    if meta.input_path.is_empty() {
        anyhow::bail!("--input <PATH> is required in offline mode");
    }
    let path = Path::new(&meta.input_path);
    if !path.exists() {
        anyhow::bail!("Input file not found: {}", path.display());
    }

    logger.info(&format!("Decoding: {}", path.display()))?;
    let audio = decode::load_first_channel(path)?;
    logger.info(&format!(
        "Decoded: sr={} Hz, channels={}, samples(mono)={}",
        audio.sr, audio.channels, audio.samples_mono.len()
    ))?;

    // choose target SR (0 => keep native, else force e.g. 48000)
    let target_sr: u32 = if cli.offline_sample_rate_hz == 0 {
        audio.sr
    } else {
        cli.offline_sample_rate_hz
    };

    // resample if needed
    let samples_mono: Vec<f32> = if audio.sr != target_sr {
        logger.info(&format!("Resampling offline audio: {} Hz → {} Hz", audio.sr, target_sr))?;
        resample_linear_mono(&audio.samples_mono, audio.sr, target_sr)
    } else {
        audio.samples_mono.clone()
    };

    // CSV path for scan results
    let csv_path = Path::new(&cli.scansong_path);
    let mut csv_file = OpenOptions::new().create(true).append(true).open(csv_path)?;
    if csv_file.metadata()?.len() == 0 {
        writeln!(
            csv_file,
            "url,start_s,end_s,score,frame_ms,window_s,stride_s,bandwidth_z,flatness_z,flux_z,crest_db,hf_ratio,dynrange_z,tonality_z,loudness_dbfs,notes,fp_type,fp_bands,fp_hop_s,fp_offset_s,fp_bins_hex"
        )?;
        csv_file.flush()?;
    }

    // Build scan params (on target SR)
    let params = prescan::ScanParams {
        sr: target_sr as f32,
        frame_ms: cli.frame_ms,
        window_s: cli.scan_window_s,
        stride_ms: cli.stride_ms,
        hf_split_hz: cli.hf_split_hz,
        top_n: cli.top_n,
        min_percentile: cli.min_percentile,
        nms_radius_s: cli.nms_radius_s,
        merge_gap_s: cli.merge_gap_s,
        clamp_min_s: cli.clamp_min_s,
        clamp_max_s: cli.clamp_max_s,
    };

    logger.info(&format!(
        "Analyzing {:.1} seconds of audio…",
        (samples_mono.len() as f32) / (target_sr as f32)
    ))?;

    // Fingerprint first ~N seconds (on the resampled grid)
    let fp = prescan::make_fingerprint(&samples_mono, params.sr, cli.fp_win_s);

    let segs = prescan::analyze(&samples_mono, &params);
    if segs.is_empty() {
        logger.info("No candidate segments found (audio too short or too quiet).")?;
        return Ok(());
    }

    // Tag column: use --scan-url if provided, else file:// path
    let tag = if !meta.url.is_empty() {
        meta.url.clone()
    } else {
        format!("file://{}", path.display())
    };

    for s in &segs {
        let w = &s.peak;
        let (fp_type, fp_bands, fp_hop_s, fp_offset_s, fp_bins_hex) = if let Some(ref f) = fp {
            (f.fp_type.as_str(), f.bands as u32, f.hop_s, f.offset_s, to_hex(&f.bins))
        } else {
            ("", 0, 0.0, 0.0, String::new())
        };
        writeln!(
            csv_file,
            "{},{:.3},{:.3},{:.3},{:.0},{:.1},{:.1},{:.2},{:.2},{:.2},{:.1},{:.3},{:.2},{:.2},{:.1},{}\
            ,{},{},{:.5},{:.3},{}",
            &tag,
            s.start_s,
            s.end_s,
            w.score,
            params.frame_ms,
            params.window_s,
            params.stride_ms / 1000.0,
            w.z.bandwidth_z,
            w.z.flatness_z,
            w.z.flux_z,
            w.crest_db,
            w.hf_ratio,
            w.z.dynrange_z,
            w.z.tonality_z,
            w.loudness_dbfs,
            "\"\"",
            fp_type,
            fp_bands,
            fp_hop_s,
            fp_offset_s,
            fp_bins_hex
        )?;
    }
    csv_file.flush()?;

    logger.info(&format!("Wrote {} segment(s) to {}", segs.len(), csv_path.display()))?;
    Ok(())
}
