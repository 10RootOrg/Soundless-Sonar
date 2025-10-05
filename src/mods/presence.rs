use anyhow::Result;
use cpal::traits::{ DeviceTrait, HostTrait, StreamTrait };
use crossbeam_channel::bounded;
use std::{
    collections::VecDeque,
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

// ===== NEW: Structure to store timestamped cross-correlation results =====
#[derive(Clone)]
struct TimestampedMeasurement {
    timestamp: Instant,
    distance_m: f32,
    strength: f32,
    // Store raw correlation data for multi-measurement analysis
    correlation: Vec<f32>,
    sample_rate: f32,
}

// ===== NEW: Peak detection helper (from analysis.rs pattern) =====
fn find_correlation_peaks(
    signal: &[f32],
    threshold: f32,
    min_distance_samples: usize
) -> Vec<(usize, f32)> {
    let mut peaks = Vec::new();
    let abs_signal: Vec<f32> = signal
        .iter()
        .map(|&x| x.abs())
        .collect();

    let mean = abs_signal.iter().sum::<f32>() / (abs_signal.len() as f32);
    let adaptive_threshold = threshold.max(mean * 2.0);

    let mut i = min_distance_samples;
    while i < abs_signal.len() - min_distance_samples {
        if abs_signal[i] > adaptive_threshold {
            let is_peak =
                (i - min_distance_samples..i).all(|j| abs_signal[i] >= abs_signal[j]) &&
                (i + 1..i + min_distance_samples + 1).all(
                    |j| (j >= abs_signal.len() || abs_signal[i] >= abs_signal[j])
                );

            if is_peak {
                peaks.push((i, abs_signal[i]));
                i += min_distance_samples;
            } else {
                i += 1;
            }
        } else {
            i += 1;
        }
    }

    peaks.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    peaks
}

// ===== NEW: Multi-measurement analyzer (passive, like map_room_continuous) =====
struct MultiMeasurementAnalyzer {
    window_sec: u32,
    tick_ms: u64,
    cap: usize,
    history: VecDeque<TimestampedMeasurement>,
    config: Config,
}

#[derive(Debug)]
struct PresenceResult {
    present: bool,
    confidence: f32,
    avg_distance_m: f32,
    avg_strength: f32,
    detection_count: usize,
    total_measurements: usize,
}

impl MultiMeasurementAnalyzer {
    fn new(window_sec: u32, tick_ms: u64, config: Config) -> Self {
        let cap = ((1000 / (tick_ms as usize)) * (window_sec as usize)).max(1);
        Self {
            window_sec,
            tick_ms,
            cap,
            history: VecDeque::with_capacity(cap),
            config,
        }
    }

    fn push(&mut self, measurement: Option<TimestampedMeasurement>) -> Option<PresenceResult> {
        if let Some(m) = measurement {
            self.history.push_back(m);
        }

        // Remove old measurements
        while self.history.len() > self.cap {
            self.history.pop_front();
        }

        // Need sufficient measurements for combined analysis
        if self.history.len() < (self.cap / 2).max(3) {
            return None;
        }

        //    self.analyze_combined() // less complicated
        self.analyze_multi_peak() // more sophisticated
    }

    fn analyze_combined(&self) -> Option<PresenceResult> {
        if self.history.is_empty() {
            return None;
        }

        // Similar to detect_room_objects but for presence detection
        // Group detections by distance bins (10cm resolution)
        let mut distance_bins: std::collections::HashMap<
            i32,
            Vec<&TimestampedMeasurement>
        > = std::collections::HashMap::new();

        for measurement in &self.history {
            // Only consider measurements in the target range
            if
                measurement.distance_m >= self.config.front_min_m &&
                measurement.distance_m <= self.config.front_max_m &&
                measurement.strength >= self.config.strength_thr
            {
                // Bin by 10cm (0.1m)
                let bin = (measurement.distance_m * 10.0).round() as i32;
                distance_bins.entry(bin).or_insert_with(Vec::new).push(measurement);
            }
        }

        if distance_bins.is_empty() {
            return Some(PresenceResult {
                present: false,
                confidence: 0.0,
                avg_distance_m: f32::INFINITY,
                avg_strength: 0.0,
                detection_count: 0,
                total_measurements: self.history.len(),
            });
        }

        // Find the most consistent detection (largest cluster)
        let mut best_bin = None;
        let mut best_count = 0;

        for (&bin, measurements) in &distance_bins {
            if measurements.len() > best_count {
                best_count = measurements.len();
                best_bin = Some((bin, measurements));
            }
        }

        if let Some((bin, measurements)) = best_bin {
            // Calculate average distance and strength for the cluster
            let mut sum_dist = 0.0f32;
            let mut sum_strength = 0.0f32;
            let mut count = 0;

            for m in measurements {
                sum_dist += m.distance_m;
                sum_strength += m.strength;
                count += 1;
            }

            let avg_dist = sum_dist / (count as f32);
            let avg_strength = sum_strength / (count as f32);

            // Confidence based on:
            // 1. Cluster size (how many measurements agree)
            // 2. Average strength
            // 3. Consistency (how tightly clustered)
            let cluster_ratio = (count as f32) / (self.history.len() as f32);
            let strength_factor = avg_strength.min(1.0);

            // Calculate consistency (std dev of distances in cluster)
            let variance: f32 =
                measurements
                    .iter()
                    .map(|m| (m.distance_m - avg_dist).powi(2))
                    .sum::<f32>() / (count as f32);
            let std_dev = variance.sqrt();
            let consistency = 1.0 / (1.0 + std_dev * 10.0); // High consistency if std_dev is low

            let confidence = (
                cluster_ratio * 0.4 +
                strength_factor * 0.4 +
                consistency * 0.2
            ).clamp(0.0, 1.0);

            // Present if we have good agreement and meet thresholds
            let present =
                cluster_ratio >= self.config.agg_frac && avg_strength >= self.config.strength_thr;

            return Some(PresenceResult {
                present,
                confidence,
                avg_distance_m: avg_dist,
                avg_strength,
                detection_count: count,
                total_measurements: self.history.len(),
            });
        }

        None
    }

    // Advanced multi-peak analysis using already-computed distances
    // (Treats each measurement's distance as a "peak" and clusters them)
    fn analyze_multi_peak(&self) -> Option<PresenceResult> {
        if self.history.is_empty() {
            return None;
        }

        // Collect all valid detections (distance + strength pairs)
        let mut all_detections: Vec<(f32, f32)> = Vec::new(); // (distance_m, strength)

        for measurement in &self.history {
            // Only include measurements that meet basic thresholds
            if
                measurement.distance_m >= self.config.front_min_m &&
                measurement.distance_m <= self.config.front_max_m &&
                measurement.strength >= self.config.strength_thr * 0.5
            {
                // Lower threshold for collection

                all_detections.push((measurement.distance_m, measurement.strength));
            }
        }

        if all_detections.is_empty() {
            return Some(PresenceResult {
                present: false,
                confidence: 0.0,
                avg_distance_m: f32::INFINITY,
                avg_strength: 0.0,
                detection_count: 0,
                total_measurements: self.history.len(),
            });
        }

        // Cluster detections by distance (10cm bins)
        let mut distance_clusters: std::collections::HashMap<
            i32,
            Vec<(f32, f32)>
        > = std::collections::HashMap::new();

        for (dist, strength) in &all_detections {
            let bin = (dist * 10.0).round() as i32; // 10cm bins
            distance_clusters.entry(bin).or_insert_with(Vec::new).push((*dist, *strength));
        }

        // Find cluster with best score (strength × consistency)
        let mut best_cluster = None;
        let mut best_score = 0.0f32;

        for (_bin, detections) in &distance_clusters {
            // Average strength in this cluster
            let avg_strength: f32 =
                detections
                    .iter()
                    .map(|(_, s)| s)
                    .sum::<f32>() / (detections.len() as f32);

            // Cluster size (more detections = more consistent)
            let cluster_size = detections.len() as f32;

            // Combined score: favor strong AND consistent reflections
            // sqrt() prevents large clusters from dominating too much
            let score = avg_strength * cluster_size.sqrt();

            if score > best_score {
                best_score = score;
                best_cluster = Some(detections);
            }
        }

        if let Some(detections) = best_cluster {
            // Calculate cluster statistics
            let avg_dist =
                detections
                    .iter()
                    .map(|(d, _)| d)
                    .sum::<f32>() / (detections.len() as f32);

            let avg_strength =
                detections
                    .iter()
                    .map(|(_, s)| s)
                    .sum::<f32>() / (detections.len() as f32);

            // Calculate consistency (lower variance = higher consistency)
            let variance: f32 =
                detections
                    .iter()
                    .map(|(d, _)| (d - avg_dist).powi(2))
                    .sum::<f32>() / (detections.len() as f32);
            let std_dev = variance.sqrt();
            let consistency = 1.0 / (1.0 + std_dev * 10.0);

            // Detection ratio (what fraction of measurements agree)
            let detection_ratio = (detections.len() as f32) / (self.history.len() as f32);

            // Confidence from multiple factors
            let confidence = (
                detection_ratio * 0.4 + // Agreement across window
                avg_strength * 0.4 + // Reflection strength
                consistency * 0.2
            ) // Spatial consistency
                .clamp(0.0, 1.0);

            // Present if we have good agreement and meet strength threshold
            let present =
                detection_ratio >= self.config.agg_frac * 0.5 &&
                avg_strength >= self.config.strength_thr * 0.75;

            return Some(PresenceResult {
                present,
                confidence,
                avg_distance_m: avg_dist,
                avg_strength,
                detection_count: detections.len(),
                total_measurements: self.history.len(),
            });
        }

        None
    }
}

/// Enhanced presence mode with multi-measurement combined analysis
pub fn run_presence(cli: &Config, logger: Arc<Logger>, log_path: &str) -> Result<()> {
    logger.info(
        &format!(
            "Enhanced sonar-presence (multi-measurement) starting…  tick_ms={}  window_sec={}  range={:.1}-{:.1}m",
            cli.tick_ms,
            cli.window_sec,
            cli.front_min_m,
            cli.front_max_m
        )
    )?;

    // CSV path
    let csv_path = {
        let p = Path::new(log_path);
        let dir = p.parent().ok_or_else(|| anyhow::anyhow!("Log path has no parent"))?;
        dir.join("Detection.csv")
    };
    let mut csv_file = OpenOptions::new().create(true).append(true).open(&csv_path)?;
    if csv_file.metadata()?.len() == 0 {
        writeln!(
            csv_file,
            "timestamp,present,avg_distance_m,avg_strength,confidence,detection_count,total_measurements"
        )?;
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

    // === loopback (passive reference) ===
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

    // === analysis setup ===
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

    // NEW: Multi-measurement analyzer instead of simple aggregator
    let mut analyzer = MultiMeasurementAnalyzer::new(cli.window_sec, cli.tick_ms, cli.clone());

    // State tracking with hysteresis
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
            // Get single-measurement estimate WITH correlation data
            if
                let Some((d, s)) = sonar_presence::estimate_from_ref(
                    &ref_frame,
                    &mic_frame,
                    sr_used,
                    cli,
                    Some(&logger)
                )
            {
                // NEW: Store timestamped measurement with correlation
                // Note: We need to modify estimate_from_ref to return correlation
                // For now, create a simple cross-correlation here
                let correlation = compute_simple_correlation(&ref_frame, &mic_frame);

                let measurement = TimestampedMeasurement {
                    timestamp: Instant::now(),
                    distance_m: d,
                    strength: s,
                    correlation,
                    sample_rate: sr_used,
                };

                // NEW: Push to multi-measurement analyzer
                if let Some(result) = analyzer.push(Some(measurement)) {
                    let nowi = Instant::now();

                    // Hysteresis logic
                    let want_present = if smooth_present {
                        result.confidence >= cli.exit_frac
                    } else {
                        result.confidence >= cli.enter_frac
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
                            "{},{},{:.2},{:.2},{:.2},{},{}",
                            ts,
                            smooth_present,
                            if smooth_present {
                                result.avg_distance_m
                            } else {
                                f32::INFINITY
                            },
                            result.avg_strength,
                            result.confidence,
                            result.detection_count,
                            result.total_measurements
                        );
                        let _ = csv_file.flush();
                    }

                    let _ = logger.info(
                        &format!(
                            "present={} distance={:.2}m strength={:.2} confidence={:.0}% detections={}/{} window={}s",
                            smooth_present,
                            if smooth_present {
                                result.avg_distance_m
                            } else {
                                f32::INFINITY
                            },
                            result.avg_strength,
                            result.confidence * 100.0,
                            result.detection_count,
                            result.total_measurements,
                            cli.window_sec
                        )
                    );
                }
            } else {
                // No detection this tick
                let _ = analyzer.push(None);
            }
        } else {
            let _ = analyzer.push(None);
        }

        let now = Instant::now();
        if next > now {
            thread::sleep(next - now);
        } else {
            next = now;
        }
    }

    logger.info("Enhanced sonar-presence stopped.")?;
    Ok(())
}

// Helper: Simple normalized cross-correlation (lightweight version)
fn compute_simple_correlation(ref_signal: &[f32], mic_signal: &[f32]) -> Vec<f32> {
    let len = ref_signal.len().min(mic_signal.len());
    let mut correlation = vec![0.0f32; len];

    // Compute correlation for each lag
    for lag in 0..len.min(2048) {
        // Limit for performance
        let mut sum = 0.0f32;
        let mut count = 0;

        for i in 0..len - lag {
            sum += ref_signal[i] * mic_signal[i + lag];
            count += 1;
        }

        if count > 0 {
            correlation[lag] = sum / (count as f32);
        }
    }

    // Normalize
    let max_val = correlation
        .iter()
        .map(|&x| x.abs())
        .fold(0.0f32, f32::max);
    if max_val > 1e-9 {
        for val in correlation.iter_mut() {
            *val /= max_val;
        }
    }

    correlation
}
