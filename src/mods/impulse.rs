//! src/mods/impulse.rs
//! Independent impulse-based presence detection mode

use anyhow::Result;
use cpal::traits::{ DeviceTrait, HostTrait, StreamTrait };
use std::sync::{ Arc, Mutex };
use std::thread;
use std::time::{ Duration, Instant };
use crate::logger::Logger;
use crate::Config;

const CORRELATION_THRESHOLD: f32 = 0.15;
const MIN_DETECTIONS_FOR_PRESENCE: f32 = 0.5; // 50% detection ratio

#[derive(Debug, Clone)]
struct ImpulseDetection {
    timestamp: Instant,
    distance: Option<f32>,
    confidence: f32,
    detected: bool,
}

pub fn run_impulse(config: &Config, logger: Arc<Logger>) -> Result<()> {
    println!("\n===== Impulse-based Presence Detection Mode =====");
    println!("Configuration:");
    println!("  Detection range: {:.1}m - {:.1}m", config.front_min_m, config.front_max_m);
    println!("  Window duration: {} seconds", config.window_sec);
    println!("  Tick interval: {} ms", config.tick_ms);
    println!("  Impulse duration: {:.1} ms", config.impulse_length_ms);
    println!("  Listen duration: {} ms", config.impulse_listen_ms);
    println!("  Amplitude: {:.2}", config.impulse_amplitude);
    println!("\nStarting continuous presence detection...");

    logger.info("Starting impulse-based presence detection mode")?;

    // Setup audio
    let host = cpal::default_host();
    let output_device = host
        .default_output_device()
        .ok_or_else(|| anyhow::anyhow!("No output device available"))?;
    let input_device = host
        .default_input_device()
        .ok_or_else(|| anyhow::anyhow!("No input device available"))?;

    let output_config = output_device.default_output_config()?;
    let input_config = input_device.default_input_config()?;
    let sample_rate = output_config.sample_rate().0;

    println!("Using sample rate: {} Hz", sample_rate);
    logger.info(&format!("Sample rate: {} Hz", sample_rate))?;

    // Calculate window parameters
    let window_duration = Duration::from_secs(config.window_sec as u64);
    let tick_duration = Duration::from_millis(config.tick_ms);
    let measurements_per_window = (window_duration.as_millis() /
        tick_duration.as_millis()) as usize;

    println!("Measurements per window: {}", measurements_per_window);

    // Detection history buffer for sliding window
    let mut detection_buffer = Vec::with_capacity(measurements_per_window);
    let mut window_start = Instant::now();
    let mut presence_state = false;

    // Main detection loop
    loop {
        let measurement_start = Instant::now();

        // Perform single impulse measurement
        let detection = perform_impulse_measurement(
            &output_device,
            &input_device,
            &output_config.config(),
            &input_config.config(),
            sample_rate,
            config,
            &logger
        )?;

        // Add to buffer
        detection_buffer.push(detection);

        // Check if window is complete
        if measurement_start.duration_since(window_start) >= window_duration {
            // Analyze window for presence
            let presence = analyze_window(&detection_buffer, measurements_per_window);

            // State change detection
            if presence != presence_state {
                presence_state = presence;
                let state_str = if presence { "PRESENT" } else { "ABSENT" };

                println!("\n>>> Presence state changed: {}", state_str);
                logger.info(&format!("Presence state: {}", state_str))?;
            }

            // Reset window
            detection_buffer.clear();
            window_start = Instant::now();

            println!("Window complete. Presence: {}", if presence { "YES" } else { "NO" });
        }

        // Wait for next tick
        let elapsed = measurement_start.elapsed();
        if elapsed < tick_duration {
            thread::sleep(tick_duration - elapsed);
        }
    }
}

fn perform_impulse_measurement(
    output_device: &cpal::Device,
    input_device: &cpal::Device,
    output_config: &cpal::StreamConfig,
    input_config: &cpal::StreamConfig,
    sample_rate: u32,
    config: &Config,
    _logger: &Arc<Logger>
) -> Result<ImpulseDetection> {
    // Generate impulse signal using config values
    let impulse_samples = ((config.impulse_length_ms / 1000.0) * (sample_rate as f32)) as usize;
    let mut impulse = vec![0.0f32; impulse_samples];

    // Create sharp impulse with configured amplitude
    if impulse_samples > 0 {
        impulse[0] = config.impulse_amplitude;
    }
    if impulse_samples > 1 {
        impulse[1] = config.impulse_amplitude * 0.5;
    }
    if impulse_samples > 2 {
        impulse[2] = config.impulse_amplitude * 0.25;
    }

    // Recording buffer
    let recording_buffer = Arc::new(Mutex::new(Vec::new()));
    let recording_clone = recording_buffer.clone();

    // Setup input stream
    let channels = input_config.channels as usize;
    let input_stream = input_device.build_input_stream(
        input_config,
        move |data: &[f32], _: &cpal::InputCallbackInfo| {
            let mut buffer = recording_clone.lock().unwrap();
            // Extract first channel only
            for frame in data.chunks(channels) {
                if let Some(sample) = frame.first() {
                    buffer.push(*sample);
                }
            }
        },
        |err| eprintln!("Input stream error: {}", err),
        None
    )?;

    // Setup output stream
    let impulse_clone = impulse.clone();
    let mut sample_clock = Arc::new(Mutex::new(0usize));
    let sample_clock_clone = sample_clock.clone();
    let output_channels = output_config.channels as usize;

    let output_stream = output_device.build_output_stream(
        output_config,
        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
            let mut clock = sample_clock_clone.lock().unwrap();
            for frame in data.chunks_mut(output_channels) {
                let sample_value = if *clock < impulse_clone.len() {
                    let val = impulse_clone[*clock];
                    *clock += 1;
                    val
                } else {
                    0.0
                };
                for sample in frame.iter_mut() {
                    *sample = sample_value;
                }
            }
        },
        |err| eprintln!("Output stream error: {}", err),
        None
    )?;

    // Start recording
    input_stream.play()?;
    thread::sleep(Duration::from_millis(10)); // Small delay

    // Play impulse
    output_stream.play()?;

    // Record for configured duration
    thread::sleep(Duration::from_millis(config.impulse_listen_ms));

    // Stop streams
    drop(output_stream);
    drop(input_stream);

    // Analyze recording
    let recording = recording_buffer.lock().unwrap().clone();
    let detection = analyze_impulse_response(
        &impulse,
        &recording,
        sample_rate,
        config.front_min_m,
        config.front_max_m
    );

    Ok(detection)
}

fn analyze_impulse_response(
    impulse: &[f32],
    recording: &[f32],
    sample_rate: u32,
    min_distance: f32,
    max_distance: f32
) -> ImpulseDetection {
    if recording.len() < impulse.len() {
        return ImpulseDetection {
            timestamp: Instant::now(),
            distance: None,
            confidence: 0.0,
            detected: false,
        };
    }

    // Simple cross-correlation to find reflections
    let correlation = compute_correlation(impulse, recording);

    // Find peaks in correlation
    let peaks = find_correlation_peaks(&correlation, CORRELATION_THRESHOLD);

    // Convert peaks to distances and filter by range
    const SOUND_SPEED: f32 = 343.0; // m/s
    let mut valid_reflections = Vec::new();

    // Skip the first peak (direct sound)
    for &(idx, strength) in peaks.iter().skip(1) {
        let time_delay = (idx as f32) / (sample_rate as f32);
        let distance = (time_delay * SOUND_SPEED) / 2.0; // Round trip

        if distance >= min_distance && distance <= max_distance {
            valid_reflections.push((distance, strength));
        }
    }

    // Determine if detection is valid
    if let Some(&(dist, strength)) = valid_reflections.first() {
        ImpulseDetection {
            timestamp: Instant::now(),
            distance: Some(dist),
            confidence: strength.min(1.0),
            detected: true,
        }
    } else {
        ImpulseDetection {
            timestamp: Instant::now(),
            distance: None,
            confidence: 0.0,
            detected: false,
        }
    }
}

fn compute_correlation(signal: &[f32], recording: &[f32]) -> Vec<f32> {
    let mut correlation = Vec::with_capacity(recording.len());
    let signal_len = signal.len();

    // Normalize signal
    let signal_energy: f32 = signal
        .iter()
        .map(|x| x * x)
        .sum();
    if signal_energy == 0.0 {
        return vec![0.0; recording.len()];
    }

    // Compute correlation at each lag
    for lag in 0..recording.len().saturating_sub(signal_len) {
        let mut sum = 0.0f32;
        let mut rec_energy = 0.0f32;

        for i in 0..signal_len {
            sum += signal[i] * recording[lag + i];
            rec_energy += recording[lag + i] * recording[lag + i];
        }

        // Normalized correlation
        let norm_corr = if rec_energy > 0.0 {
            sum / (signal_energy * rec_energy).sqrt()
        } else {
            0.0
        };

        correlation.push(norm_corr.abs());
    }

    correlation
}

fn find_correlation_peaks(correlation: &[f32], threshold: f32) -> Vec<(usize, f32)> {
    let mut peaks: Vec<(usize, f32)> = Vec::new();
    let min_distance = 20; // Minimum samples between peaks

    for i in 1..correlation.len() - 1 {
        // Check if local maximum above threshold
        if
            correlation[i] > threshold &&
            correlation[i] > correlation[i - 1] &&
            correlation[i] > correlation[i + 1]
        {
            // Check minimum distance from last peak
            if peaks.is_empty() || i - peaks.last().unwrap().0 > min_distance {
                peaks.push((i, correlation[i]));
            }
        }
    }

    peaks
}

fn analyze_window(detections: &[ImpulseDetection], expected_count: usize) -> bool {
    // Count valid detections in window
    let valid_detections = detections
        .iter()
        .filter(|d| d.detected)
        .count();

    // Calculate detection ratio
    let detection_ratio = (valid_detections as f32) / (expected_count.max(1) as f32);

    // Presence if sufficient detections
    detection_ratio >= MIN_DETECTIONS_FOR_PRESENCE
}
