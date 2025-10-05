use anyhow::Result;
use cpal::traits::{ DeviceTrait, HostTrait, StreamTrait };
use crossbeam_channel::bounded;
use std::{
    fs::OpenOptions,
    io::Write,
    path::Path,
    sync::{ atomic::{ AtomicBool, Ordering }, Arc, Mutex },
    thread,
    time::{ Duration, Instant },
};

use crate::{
    audio_sink_thread,
    build_input_stream,
    maybe_rate_supported,
    sonar_presence,
    wasapi_loopback,
    SharedBuf,
    Config,
};
use crate::logger::Logger;

#[cfg(target_os = "windows")]
use crate::{ start_probe, ENABLE_PROBE_TONE };

/// Presence mode: ref↔mic correlation with sliding aggregator.
/// Writes state changes to `Detection.csv` next to the configured log file.
pub fn run_presence(cli: &Config, logger: Arc<Logger>, log_path: &str) -> Result<()> {
    logger.info(
        &format!(
            "sonar-presence (ref↔mic, WASAPI loopback) starting…  tick_ms={}  agg_frac={:.2}  window_sec={}",
            cli.tick_ms,
            cli.agg_frac,
            cli.window_sec
        )
    )?;

    // CSV path sits beside the log file.
    let csv_path = {
        let p = Path::new(log_path);
        let dir = p.parent().ok_or_else(|| anyhow::anyhow!("Log path has no parent"))?;
        dir.join("Detection.csv")
    };
    let mut csv_file = OpenOptions::new().create(true).append(true).open(&csv_path)?;
    if csv_file.metadata()?.len() == 0 {
        writeln!(csv_file, "timestamp,present,avg_distance_m,avg_strength,agree_pct")?;
        csv_file.flush()?;
    }

    // ctrl+c to quit
    let quit = Arc::new(AtomicBool::new(false));
    {
        let q = quit.clone();
        let _ = ctrlc::set_handler(move || {
            q.store(true, Ordering::SeqCst);
        });
    }

    // === microphone (cpal) ===
    let host = cpal::default_host();
    let mic_device = host
        .default_input_device()
        .ok_or_else(|| anyhow::anyhow!("No default input device (microphone) found"))?;
    let mut mic_config = mic_device.default_input_config()?.config();

    // Prefer 48 kHz if available.
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

    // === loopback (render reference) ===
    let sr_target = sr_mic as u32;

    #[cfg(target_os = "windows")]
    let _probe_stream = if ENABLE_PROBE_TONE { start_probe(sr_target).ok() } else { None };

    let shared_ref = SharedBuf {
        buf: Arc::new(Mutex::new(Vec::with_capacity((sr_target as usize) * 10))),
        sr: Arc::new(Mutex::new(sr_mic)),
    };
    let rx_ref = wasapi_loopback::start(sr_target, logger.clone(), cli.tick_ms)?;
    {
        let shared_ref_clone = shared_ref.clone();
        thread::spawn(move || audio_sink_thread(rx_ref, shared_ref_clone));
    }

    // === analysis constants ===
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

    // smoothed presence state with hysteresis+dwell
    let mut smooth_present = false;
    let mut last_flip = Instant::now() - Duration::from_millis(cli.min_dwell_ms);

    let mut next = Instant::now();
    while !quit.load(Ordering::SeqCst) {
        next += Duration::from_millis(cli.tick_ms);

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
                        nowi.duration_since(last_flip) >= Duration::from_millis(cli.min_dwell_ms)
                    {
                        smooth_present = want_present;
                        last_flip = nowi;

                        // CSV on state change
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

                    let _ = logger.info(
                        &format!(
                            "present={} avg_distance_m={:.2} avg_strength={:.2} window={}s agree={:.0}%",
                            smooth_present,
                            if smooth_present {
                                avg_d
                            } else {
                                f64::INFINITY
                            },
                            avg_s,
                            cli.window_sec,
                            agree * 100.0
                        )
                    );
                }
            } else if let Some((_present_raw, avg_d, avg_s, agree)) = agg.push(None) {
                // dwell/hysteresis even on quiet ticks
                let nowi = Instant::now();
                let want_present = if smooth_present {
                    agree >= cli.exit_frac
                } else {
                    agree >= cli.enter_frac
                };

                if
                    want_present != smooth_present &&
                    nowi.duration_since(last_flip) >= Duration::from_millis(cli.min_dwell_ms)
                {
                    smooth_present = want_present;
                    last_flip = nowi;

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

                let _ = logger.info(
                    &format!(
                        "present={} avg_distance_m={:.2} avg_strength={:.2} window={}s agree={:.0}% (quiet/none)",
                        smooth_present,
                        if smooth_present {
                            avg_d
                        } else {
                            f64::INFINITY
                        },
                        avg_s,
                        cli.window_sec,
                        agree * 100.0
                    )
                );
            }
        } else {
            let _ = agg.push(None);
        }

        let now = Instant::now();
        if next > now {
            thread::sleep(next - now);
        } else {
            next = now;
        }
    }

    logger.info("sonar-presence stopped.")?;
    Ok(())
}
