use anyhow::Result;
use cpal::traits::{ DeviceTrait, HostTrait, StreamTrait };
use crossbeam_channel::bounded;
use std::{
    fs::{ File, OpenOptions },
    io::{ BufRead, BufReader, Write },
    path::Path,
    sync::{ atomic::{ AtomicBool, Ordering }, Arc, Mutex },
    thread,
    time::{ Duration, Instant },
};

use crate::{
    audio_sink_thread,
    build_input_stream,
    maybe_rate_supported,
    prescan,
    sonar_presence,
    wasapi_loopback,
    SharedBuf,
    Config,
};
use crate::logger::Logger;

#[cfg(target_os = "windows")]
use crate::{ start_probe, ENABLE_PROBE_TONE };

/// Small local hex decoder (kept here so this file is self-contained).
fn from_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    for i in (0..s.len()).step_by(2) {
        let hi = (bytes[i] as char).to_digit(16)? as u8;
        let lo = (bytes[i + 1] as char).to_digit(16)? as u8;
        out.push((hi << 4) | lo);
    }
    Some(out)
}

#[derive(Clone, Debug)]
struct SongFingerprint {
    url: String,
    fp_type: String,
    bands: usize,
    hop_s: f32,
    offset_s: f32,
    bins: Vec<u8>,
}

#[derive(Clone, Debug)]
struct SongWindows {
    url: String,
    segs: Vec<(f32, f32)>, // [start_s, end_s]
    fp: SongFingerprint,
}

fn parse_scansong(csv_path: &Path, logger: &Logger) -> Result<Vec<SongWindows>> {
    let file = File::open(csv_path)?;
    let reader = BufReader::new(file);
    let mut lines = reader.lines();

    // header
    let header = lines.next().ok_or_else(|| anyhow::anyhow!("SongScan.csv is empty"))??;
    let cols: Vec<&str> = header.split(',').collect();
    let mut idx = |name: &str| -> Option<usize> { cols.iter().position(|c| c.trim() == name) };

    // required columns
    let i_url = idx("url").ok_or_else(|| anyhow::anyhow!("SongScan.csv missing 'url' column"))?;
    let i_start = idx("start_s").ok_or_else(|| anyhow::anyhow!("SongScan.csv missing 'start_s'"))?;
    let i_end = idx("end_s").ok_or_else(|| anyhow::anyhow!("SongScan.csv missing 'end_s'"))?;

    // fingerprint columns
    let i_fp_type = idx("fp_type").ok_or_else(||
        anyhow::anyhow!("SongScan.csv missing 'fp_type'")
    )?;
    let i_fp_bands = idx("fp_bands").ok_or_else(||
        anyhow::anyhow!("SongScan.csv missing 'fp_bands'")
    )?;
    let i_fp_hop = idx("fp_hop_s").ok_or_else(||
        anyhow::anyhow!("SongScan.csv missing 'fp_hop_s'")
    )?;
    let i_fp_off = idx("fp_offset_s").ok_or_else(||
        anyhow::anyhow!("SongScan.csv missing 'fp_offset_s'")
    )?;
    let i_fp_bins = idx("fp_bins_hex").ok_or_else(||
        anyhow::anyhow!("SongScan.csv missing 'fp_bins_hex'")
    )?;

    use std::collections::BTreeMap;
    let mut by_url: BTreeMap<String, (Option<SongFingerprint>, Vec<(f32, f32)>)> = BTreeMap::new();

    for line in lines {
        let line = match line {
            Ok(s) => s,
            Err(_) => {
                continue;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split(',').collect();
        if parts.len() <= i_end {
            continue;
        }

        let url = parts[i_url].trim().to_string();
        if url.is_empty() {
            continue;
        }

        let start_s: f32 = parts[i_start].trim().parse().unwrap_or(0.0);
        let end_s: f32 = parts[i_end].trim().parse().unwrap_or(0.0);

        let entry = by_url.entry(url.clone()).or_insert((None, Vec::new()));
        entry.1.push((start_s, end_s));

        if entry.0.is_none() {
            let fp_type = parts[i_fp_type].trim().to_string();
            let bands = parts[i_fp_bands].trim().parse::<usize>().unwrap_or(0);
            let hop_s = parts[i_fp_hop].trim().parse::<f32>().unwrap_or(0.0);
            let offset_s = parts[i_fp_off].trim().parse::<f32>().unwrap_or(0.0);
            let bins_hex = parts
                .get(i_fp_bins)
                .map(|s| s.trim())
                .unwrap_or("");
            if !fp_type.is_empty() && bands > 0 && hop_s > 0.0 && !bins_hex.is_empty() {
                if let Some(bins) = from_hex(bins_hex) {
                    entry.0 = Some(SongFingerprint {
                        url: url.clone(),
                        fp_type,
                        bands,
                        hop_s,
                        offset_s,
                        bins,
                    });
                }
            }
        }
    }

    let mut out = Vec::<SongWindows>::new();
    for (url, (maybe_fp, mut segs)) in by_url {
        if let Some(fp) = maybe_fp {
            segs.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
            out.push(SongWindows { url, segs, fp });
        } else {
            let _ = logger.warn(&format!("Skipping url with no usable fingerprint: {}", url));
        }
    }
    Ok(out)
}

fn rms_dbfs(x: &[f32]) -> f32 {
    if x.is_empty() {
        return -120.0;
    }
    let mut e = 0.0f64;
    for &v in x {
        e += (v as f64) * (v as f64);
    }
    let r = (e / (x.len() as f64)).sqrt() as f32;
    if r <= 1e-9 {
        -120.0
    } else {
        20.0 * r.log10()
    }
}

/// Gated mode:
/// 1) align playback to a song via 5s fingerprint,
/// 2) run presence only inside that song's exported windows (+/- guard).
pub fn run_gated(cli: &Config, logger: Arc<Logger>) -> Result<()> {
    logger.info(
        "sonar-presence-gated starting… will align via 5s fingerprint, then run presence only inside SongScan windows"
    )?;

    // load SongScan.csv (with fingerprint columns)
    let csv_scan_path = Path::new(&cli.scansong_path);
    if !csv_scan_path.exists() {
        anyhow::bail!("SongScan.csv not found at {}", csv_scan_path.display());
    }
    let songs = parse_scansong(csv_scan_path, &logger)?;
    if songs.is_empty() {
        anyhow::bail!("No songs with fingerprints found in {}", csv_scan_path.display());
    }
    logger.info(&format!("Loaded {} song(s) with fingerprints.", songs.len()))?;

    // ctrl+c to quit
    let quit = Arc::new(AtomicBool::new(false));
    {
        let q = quit.clone();
        let _ = ctrlc::set_handler(move || {
            q.store(true, Ordering::SeqCst);
        });
    }

    // === devices: mic + loopback ===
    let host = cpal::default_host();
    let mic_device = host
        .default_input_device()
        .ok_or_else(|| anyhow::anyhow!("No default input device (microphone) found"))?;
    let mut mic_config = mic_device.default_input_config()?.config();
    if let Some(sr) = maybe_rate_supported(&mic_device, 48_000) {
        mic_config.sample_rate.0 = sr;
    }
    let sr_mic = mic_config.sample_rate.0 as f32;

    logger.info(&format!("Mic device: {}", mic_device.name().unwrap_or_default()))?;
    logger.info(
        &format!(
            "Mic: sample rate {} Hz, channels {}",
            mic_config.sample_rate.0,
            mic_config.channels
        )
    )?;

    let shared_mic = SharedBuf {
        buf: Arc::new(Mutex::new(Vec::with_capacity((sr_mic as usize) * 10))),
        sr: Arc::new(Mutex::new(sr_mic)),
    };

    let (tx_mic, rx_mic) = bounded::<Vec<f32>>(8);
    let mic_channels = mic_config.channels.max(1) as usize;

    let mic_stream = build_input_stream(
        &mic_device,
        &mic_config,
        mic_channels,
        tx_mic,
        logger.clone()
    )?;
    mic_stream.play()?;

    {
        let shared_clone = shared_mic.clone();
        thread::spawn(move || audio_sink_thread(rx_mic, shared_clone));
    }

    // loopback at mic SR
    let sr_target = sr_mic as u32;
    #[cfg(target_os = "windows")]
    let _probe_stream = if ENABLE_PROBE_TONE { start_probe(sr_target).ok() } else { None };

    let shared_ref = SharedBuf {
        buf: Arc::new(Mutex::new(Vec::with_capacity((sr_target as usize) * 20))),
        sr: Arc::new(Mutex::new(sr_mic)),
    };
    let rx_ref = wasapi_loopback::start(sr_target, logger.clone(), cli.tick_ms.min(50))?;
    {
        let shared_ref_clone = shared_ref.clone();
        thread::spawn(move || audio_sink_thread(rx_ref, shared_ref_clone));
    }

    // prepare Detection.csv beside the normal log
    let csv_path_det = {
        let p = Path::new(&cli.log_path);
        let dir = p.parent().ok_or_else(|| anyhow::anyhow!("Log path has no parent"))?;
        dir.join("Detection.csv")
    };
    let mut csv_file = OpenOptions::new().create(true).append(true).open(&csv_path_det)?;
    if csv_file.metadata()?.len() == 0 {
        writeln!(csv_file, "timestamp,present,avg_distance_m,avg_strength,agree_pct")?;
        csv_file.flush()?;
    }

    // presence analysis constants (same as presence mode)
    let sr_used = *shared_mic.sr.lock().unwrap();
    let c = 343.0_f32;
    let echo_max = (((2.0 * cli.front_max_m) / c) * sr_used).ceil() as usize;
    let base_max = (
        ((sonar_presence::MAX_PIPELINE_DELAY_MS as f32) / 1000.0) *
        sr_used
    ).ceil() as usize;
    let analysis_len = (base_max + echo_max + 1024).next_power_of_two().max(4096);

    logger.info(
        &format!(
            "Analysis window: {} samples (~{:.0} ms)",
            analysis_len,
            ((analysis_len as f32) / sr_used) * 1000.0
        )
    )?;

    let mut agg = sonar_presence::Aggregator::new(cli.window_sec, cli.tick_ms, cli.agg_frac);
    let mut smooth_present = false;
    let mut last_flip = Instant::now() - Duration::from_millis(cli.min_dwell_ms);

    // current alignment: (url, t0 when song started, t0 offset_s)
    let mut aligned: Option<(String, Instant, f32)> = None;

    logger.info(
        &format!(
            "Waiting for playback… arming fingerprint when loopback > {:.0} dBFS",
            cli.fp_arm_dbfs
        )
    )?;

    // main loop
    let mut next = Instant::now();
    while !quit.load(Ordering::SeqCst) {
        next += Duration::from_millis(cli.tick_ms);

        // Step 1: if not aligned, try to match live 5s fingerprint.
        if aligned.is_none() {
            let (loop_recent, sr_loop) = {
                let b = shared_ref.buf.lock().unwrap();
                let sr = *shared_ref.sr.lock().unwrap();
                (b.clone(), sr)
            };

            let db = rms_dbfs(&loop_recent);
            if
                db > cli.fp_arm_dbfs &&
                (loop_recent.len() as f32) >= cli.fp_win_s * sr_loop + 1024.0
            {
                // take up to last ~7s
                let need_secs = (7.0f32)
                    .min((loop_recent.len() as f32) / sr_loop)
                    .max(cli.fp_win_s);
                let need = (need_secs * sr_loop) as usize;
                let start = loop_recent.len().saturating_sub(need);
                let live_chunk = &loop_recent[start..];

                if let Some(live_fp) = prescan::make_fingerprint(live_chunk, sr_loop, cli.fp_win_s) {
                    // compare against all stored songs
                    let mut best: (String, f32) = (String::new(), 0.0);
                    let mut second = 0.0f32;

                    for s in &songs {
                        let ref_fp = prescan::Fingerprint {
                            fp_type: s.fp.fp_type.clone(),
                            bands: s.fp.bands,
                            hop_s: s.fp.hop_s,
                            offset_s: s.fp.offset_s,
                            bins: s.fp.bins.clone(),
                        };
                        let sim = prescan::fp_similarity(&live_fp, &ref_fp);
                        if sim > best.1 {
                            second = best.1;
                            best = (s.url.clone(), sim);
                        } else if sim > second {
                            second = sim;
                        }
                    }

                    let top = best.1;
                    let margin = top - second;
                    logger.info(
                        &format!(
                            "Fingerprint match: top={:.2} margin={:.2} url={}",
                            top,
                            margin,
                            if best.0.is_empty() {
                                "<none>"
                            } else {
                                &best.0
                            }
                        )
                    )?;

                    if !best.0.is_empty() && top >= cli.fp_thr && margin >= cli.fp_margin {
                        let url = best.0.clone();
                        let song = songs
                            .iter()
                            .find(|s| s.url == url)
                            .unwrap();
                        let t0_offset = song.fp.offset_s;
                        let t0 = Instant::now() - Duration::from_secs_f32(t0_offset);
                        aligned = Some((url.clone(), t0, t0_offset));
                        logger.info(
                            &format!(
                                "Aligned to '{}' (similarity {:.2}). t0 offset {:.3}s.",
                                url,
                                top,
                                t0_offset
                            )
                        )?;
                    } else {
                        logger.warn("Low-confidence match; still waiting…")?;
                    }
                }
            }

            // pacing
            let now = Instant::now();
            if next > now {
                thread::sleep(next - now);
            } else {
                next = now;
            }
            continue;
        }

        // Step 2: aligned — gate presence to that song's windows.
        let (active_url, t0, _t0_off) = aligned.clone().unwrap();
        let song = songs
            .iter()
            .find(|s| s.url == active_url)
            .unwrap();

        let t_song = (Instant::now() - t0).as_secs_f32();

        let mut inside = false;
        for &(a, b) in &song.segs {
            if t_song >= a - cli.guard_s && t_song <= b + cli.guard_s {
                inside = true;
                break;
            }
        }

        if inside {
            let mic_frame = {
                let b = shared_mic.buf.lock().unwrap();
                if b.len() < analysis_len {
                    Vec::new()
                } else {
                    b[b.len() - analysis_len..].to_vec()
                }
            };
            let ref_frame = {
                let b = shared_ref.buf.lock().unwrap();
                if b.len() < analysis_len {
                    Vec::new()
                } else {
                    b[b.len() - analysis_len..].to_vec()
                }
            };

            if mic_frame.len() == analysis_len && ref_frame.len() == analysis_len {
                if
                    let Some((d, s)) = sonar_presence::estimate_from_ref(
                        &ref_frame,
                        &mic_frame,
                        sr_used,
                        cli,
                        Some(&logger)
                    )
                {
                    let present_instant = d <= cli.dist_max_m && s >= cli.strength_thr;
                    let vote = if present_instant { Some((d, s)) } else { None };

                    if let Some((_present_raw, avg_d, avg_s, agree)) = agg.push(vote) {
                        let nowi = Instant::now();
                        let want_present = if smooth_present {
                            agree >= cli.exit_frac
                        } else {
                            agree >= cli.enter_frac
                        };

                        if
                            want_present != smooth_present &&
                            nowi.duration_since(last_flip) >=
                                Duration::from_millis(cli.min_dwell_ms)
                        {
                            smooth_present = want_present;
                            last_flip = nowi;

                            logger.info(
                                &format!(
                                    "state_change(hysteresis,gated url={}) -> present={}",
                                    active_url,
                                    smooth_present
                                )
                            )?;

                            let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
                            let _ = writeln!(
                                csv_file,
                                "{},{},{:.2},{:.2},{:.0}",
                                ts,
                                smooth_present,
                                avg_d,
                                avg_s,
                                agree * 100.0
                            );
                            let _ = csv_file.flush();
                        }
                    }
                } else {
                    let _ = agg.push(None);
                }
            } else {
                let _ = agg.push(None);
            }
        } else {
            // outside windows: decay the aggregator; optionally drop alignment after far past end
            let _ = agg.push(None);
            if let Some(&(_, last_b)) = song.segs.last() {
                if t_song > last_b + 60.0 {
                    logger.info(
                        "End of windows passed; clearing alignment and waiting for next track…"
                    )?;
                    aligned = None;
                    smooth_present = false;
                    last_flip = Instant::now() - Duration::from_millis(cli.min_dwell_ms);
                }
            }
        }

        let now = Instant::now();
        if next > now {
            thread::sleep(next - now);
        } else {
            next = now;
        }
    }

    logger.info("sonar-presence-gated stopped.")?;
    Ok(())
}
