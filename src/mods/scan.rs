use anyhow::Result;
use std::{
    fs::OpenOptions,
    io::Write,
    path::Path,
    sync::Arc,
    time::Duration,
};

use crate::{logger::Logger, prescan, wasapi_loopback};

/// tiny hex encoder so this file is standalone
fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// Loopback-only pre-scan of the currently playing audio (e.g., YouTube).
/// Captures at configurable SR, then extracts and writes best segments to `SongScan.csv`.
pub fn run_scan(cli: &crate::Config, meta: &crate::ScanMeta, logger: Arc<Logger>) -> Result<()> {
    logger.info(&format!(
        "sonar-prescan (loopback-only) starting…  frame_ms={:.0} window_s={:.1} stride_ms={:.0} top_n={} min_pct={:.0}",
        cli.frame_ms,
        cli.scan_window_s,
        cli.stride_ms,
        cli.top_n,
        cli.min_percentile
    ))?;
    if !meta.url.is_empty() {
        logger.info(&format!("Tagging CSV with url={}", meta.url))?;
    }

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

    // ctrl+c to stop capture of a song
    let quit = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let q = quit.clone();
        let _ = ctrlc::set_handler(move || {
            q.store(true, std::sync::atomic::Ordering::SeqCst);
        });
    }

    // Capture loopback only. Use configurable target SR (default 48k).
    let sr_target: u32 = cli.scan_sample_rate_hz;
    logger.info(&format!(
        "Tip: set Windows Output sample rate to {} Hz for accurate timestamps.",
        sr_target
    ))?;

    // Smaller chunking for capture; analysis will re-frame anyway.
    let tick_ms_for_capture = 50u64;
    let rx = wasapi_loopback::start(sr_target, logger.clone(), tick_ms_for_capture)?;

    logger.info("Playback your YouTube track now. Press Ctrl+C when the track ends to analyze.")?;

    let mut song: Vec<f32> = Vec::with_capacity((sr_target as usize) * 600); // ~10 min
    while !quit.load(std::sync::atomic::Ordering::SeqCst) {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(block) => song.extend_from_slice(&block),
            Err(_timeout) => { /* keep polling until Ctrl+C */ }
        }
    }

    logger.info(&format!(
        "Captured {:.1} seconds of loopback audio; analyzing…",
        (song.len() as f32) / (sr_target as f32)
    ))?;

    // Build scan params
    let params = prescan::ScanParams {
        sr: sr_target as f32,
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

    // One fingerprint for the track (first ~N seconds)
    let fp = prescan::make_fingerprint(&song, params.sr, cli.fp_win_s);

    let segs = prescan::analyze(&song, &params);
    if segs.is_empty() {
        logger.info("No candidate segments found (audio too short or too quiet).")?;
        return Ok(());
    }

    // Append rows; include same fingerprint per row.
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
            if meta.url.is_empty() { "" } else { &meta.url },
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
