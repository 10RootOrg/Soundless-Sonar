//! src/main.rs

use anyhow::Result;
use cpal::traits::{ DeviceTrait, HostTrait, StreamTrait };
use crossbeam_channel::{ bounded, Receiver };
use std::{
    env,
    fs::{ File, OpenOptions },
    io::{ BufRead, BufReader, Write },
    path::Path,
    sync::{ atomic::{ AtomicBool, Ordering }, Arc, Mutex },
    thread,
    time::{ Duration, Instant },
};

mod logger;
use logger::Logger;

use crate::logger::LogLevel;

// expose the split mode files in src/mods/
mod mods;

// ───────────────────────────────────────────────────────────────────────────────
// sonar_presence: ref↔mic correlation + sliding aggregator
// ───────────────────────────────────────────────────────────────────────────────
pub mod sonar_presence {
    use std::collections::VecDeque;

    // Defaults (overridable via CLI) - now moved to Config::default()
    pub const TICK_MS: u64 = 250;
    pub const DEFAULT_WINDOW_SEC: u32 = 5;
    pub const MAX_PIPELINE_DELAY_MS: u32 = 200;
    pub const AGG_FRAC: f32 = 0.5;

    #[inline]
    pub fn window_cap(window_sec: u32, tick_ms: u64) -> usize {
        ((1000 / (tick_ms as usize)) * (window_sec as usize)).max(1)
    }

    #[inline]
    fn l2norm_in_place(x: &mut [f32]) {
        let e =
            x
                .iter()
                .map(|v| v * v)
                .sum::<f32>()
                .sqrt() + 1e-9;
        for v in x.iter_mut() {
            *v /= e;
        }
    }
    #[inline]
    fn dc_remove_in_place(x: &mut [f32]) {
        let mean = x.iter().copied().sum::<f32>() / (x.len() as f32);
        for v in x.iter_mut() {
            *v -= mean;
        }
    }
    #[inline]
    fn preemph_diff_in_place(x: &mut [f32]) {
        let mut prev = 0.0f32;
        for s in x.iter_mut() {
            let cur = *s;
            *s = cur - prev;
            prev = cur;
        }
    }

    /// Estimate (distance_m, strength) by correlating RENDER (ref) with MIC.
    pub fn estimate_from_ref(
        x_ref: &[f32],
        x_mic: &[f32],
        sr: f32,
        config: &crate::Config,
        logger: Option<&crate::logger::Logger> // Add logger parameter
    ) -> Option<(f32, f32)> {
        let n = x_ref.len().min(x_mic.len());
        if n < 1024 {
            return None;
        }

        let mut a = x_ref[..n].to_vec();
        let mut b = x_mic[..n].to_vec();

        // quick RMS gates
        let rms = |v: &Vec<f32>|
            (
                v
                    .iter()
                    .map(|x| x * x)
                    .sum::<f32>() / (v.len() as f32)
            ).sqrt();
        let rms_mic = rms(&b);
        let rms_ref = rms(&a);

        // Add debug logging for RMS levels
        if let Some(log) = logger {
            let _ = log.debug(
                &format!(
                    "RMS levels: mic={:.6} ref={:.6} (thresholds: min_rms={:.6} min_ref_rms={:.6})",
                    rms_mic,
                    rms_ref,
                    config.min_rms,
                    config.min_ref_rms
                )
            );
        }

        if rms_mic < config.min_rms && rms_ref < config.min_ref_rms {
            if let Some(log) = logger {
                let _ = log.debug("RMS gate failed: both mic and ref below thresholds");
            }
            return None;
        }
        // normalize, pre-emphasis
        dc_remove_in_place(&mut a);
        dc_remove_in_place(&mut b);
        preemph_diff_in_place(&mut a);
        preemph_diff_in_place(&mut b);
        l2norm_in_place(&mut a);
        l2norm_in_place(&mut b);

        let c = 343.0_f32;
        let min_echo = (((2.0 * config.front_min_m) / c) * sr).round() as usize;
        let max_echo = (((2.0 * config.front_max_m) / c) * sr).round() as usize;
        if max_echo <= min_echo || max_echo >= n {
            return None;
        }

        let base_max = (((MAX_PIPELINE_DELAY_MS as f32) / 1000.0) * sr).round() as usize;
        let kmax = (base_max + max_echo).min(n - 1);

        // normalized cross-correlation r_xy[k] for k≥0
        let mut rs = Vec::with_capacity(kmax + 1);
        let mut best0 = (0usize, -1.0f32);
        for k in 0..=kmax {
            let m = n - k;
            let (mut num, mut ex, mut ey) = (0.0f32, 0.0f32, 0.0f32);
            for i in 0..m {
                let xr = a[i];
                let yr = b[i + k];
                num += xr * yr;
                ex += xr * xr;
                ey += yr * yr;
            }
            let r = num / (ex.sqrt() * ey.sqrt() + 1e-9);
            rs.push(r);
            if r > best0.1 {
                best0 = (k, r);
            }
        }
        let k0 = best0.0;

        // search echo band AFTER the direct path
        let start = k0.saturating_add(min_echo);
        let end = (k0 + max_echo).min(kmax);
        if start >= end {
            return None;
        }

        let mut best1 = (start, -1.0f32);
        for k in start..=end {
            if rs[k] > best1.1 {
                best1 = (k, rs[k]);
            }
        }

        // second-best outside small neighborhood
        let neigh = 6usize;
        let mut second = -1.0f32;
        for (i, &r) in rs[start..=end].iter().enumerate() {
            let idx = start + i;
            if idx + neigh < best1.0 || idx.saturating_sub(neigh) > best1.0 {
                if r > second {
                    second = r;
                }
            }
        }
        if second < 0.0 {
            second = 0.0;
        }

        // robust normalization within echo band
        let mut band = rs[start..=end].to_vec();
        band.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let idx = |p: f32| -> usize {
            (((band.len() as f32) * p).floor() as usize).clamp(0, band.len() - 1)
        };
        let p75 = band[idx(0.75)];
        let p95 = band[idx(0.95)].max(p75 + 1e-6);
        let mut prominence = ((best1.1 - second).max(0.0) / (p95 - p75)).clamp(0.0, 1.0);
        if best1.1 < p75 {
            prominence *= 0.5;
        }

        let delta_k = (best1.0 - k0) as f32; // samples between direct path and person echo
        let dist_m = ((delta_k / sr) * 343.0_f32) / 2.0;

        Some((dist_m.min(config.dist_max_m), prominence))
    }

    pub struct Aggregator {
        window_sec: u32,
        cap: usize,
        history: VecDeque<Option<(f32, f32)>>,
        agg_frac: f32,
    }
    impl Aggregator {
        pub fn new(window_sec: u32, tick_ms: u64, agg_frac: f32) -> Self {
            let cap = window_cap(window_sec, tick_ms);
            Self {
                window_sec,
                cap,
                history: VecDeque::with_capacity(cap),
                agg_frac,
            }
        }
        /// Sliding window aggregator (updated every tick)
        pub fn push(&mut self, vote: Option<(f32, f32)>) -> Option<(bool, f64, f64, f32)> {
            self.history.push_back(vote);
            while self.history.len() > self.cap {
                self.history.pop_front();
            }
            if self.history.len() < self.cap {
                return None;
            }

            let mut cnt = 0usize;
            let (mut sum_d, mut sum_s) = (0.0f32, 0.0f32);
            for v in self.history.iter() {
                if let Some((d, s)) = v {
                    cnt += 1;
                    sum_d += *d;
                    sum_s += *s;
                }
            }

            let agree = (cnt as f32) / (self.cap as f32);
            let present = agree >= self.agg_frac;
            let avg_d = if cnt > 0 { (sum_d / (cnt as f32)) as f64 } else { f64::INFINITY };
            let avg_s = if cnt > 0 { (sum_s / (cnt as f32)) as f64 } else { 0.0 };
            Some((present, avg_d, avg_s, agree))
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────────
// CLI config + parsing
// ───────────────────────────────────────────────────────────────────────────────
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Presence,
    Scan,
    Offline,
    Gated,
    Enrich,
    Impulse,
}

#[derive(Clone, Debug)]
pub struct Config {
    // common / presence
    pub mode: Mode,
    pub tick_ms: u64,
    pub agg_frac: f32,
    pub window_sec: u32,

    // presence detection parameters (now configurable)
    pub min_dwell_ms: u64,
    pub exit_frac: f32,
    pub enter_frac: f32,
    pub front_min_m: f32,
    pub front_max_m: f32,
    pub strength_thr: f32,
    pub dist_max_m: f32,
    pub min_ref_rms: f32,
    pub min_rms: f32,

    // paths
    pub log_path: String,
    pub scansong_path: String,

    // scan/offline params
    pub frame_ms: f32,
    pub scan_window_s: f32,
    pub stride_ms: f32,
    pub hf_split_hz: f32,
    pub top_n: usize,
    pub min_percentile: f32,
    pub nms_radius_s: f32,
    pub merge_gap_s: f32,
    pub clamp_min_s: f32,
    pub clamp_max_s: f32,

    // scan capture rate flag
    pub scan_sample_rate_hz: u32,

    // gated/fingerprint params
    pub fp_win_s: f32,
    pub fp_thr: f32,
    pub fp_margin: f32,
    pub guard_s: f32,
    pub fp_arm_dbfs: f32,
    pub offline_sample_rate_hz: u32,

    pub enrich_song_path: String,
    pub enrich_interval_length_s: f32,
    pub enrich_ping_length_s: f32,
    pub ffmpeg_path: String,

    pub impulse_listen_ms: u64,
    pub impulse_length_ms: f32,
    pub impulse_amplitude: f32,

    pub log_level: LogLevel,
}
impl Default for Config {
    fn default() -> Self {
        // // Production
        let default_log = env
            ::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("."))
            .join("build")
            .join("Detection.log")
            .to_string_lossy()
            .into_owned();

        // ***************************
        // Development
        // let default_log = env
        //     ::current_dir()
        //     .unwrap_or_else(|_| std::path::PathBuf::from("."))
        //     .join("sonar-web-gui")
        //     .join("public")
        //     .join("Detection.log")
        //     .to_string_lossy()
        //     .into_owned();

        println!("log path {}", default_log);

        let default_scansong = {
            let p = Path::new(&default_log);
            match p.parent() {
                Some(dir) => dir.join("SongScan.csv").to_string_lossy().into_owned(),
                None => String::from("SongScan.csv"),
            }
        };
        Self {
            mode: Mode::Presence,
            tick_ms: sonar_presence::TICK_MS,
            agg_frac: sonar_presence::AGG_FRAC,
            window_sec: sonar_presence::DEFAULT_WINDOW_SEC,
            log_level: LogLevel::Info, // ADD THIS LINE

            // New presence detection defaults
            min_dwell_ms: 5000,
            exit_frac: 0.3,
            enter_frac: 0.6,
            front_min_m: 0.3,
            front_max_m: 1.5,
            strength_thr: 0.2,
            dist_max_m: 1.5,
            min_ref_rms: 0.0001,
            min_rms: 0.0002,

            log_path: default_log,
            scansong_path: default_scansong,

            frame_ms: 23.0,
            scan_window_s: 3.0,
            stride_ms: 200.0,
            hf_split_hz: 2500.0,
            top_n: 20,
            min_percentile: 85.0,
            nms_radius_s: 1.0,
            merge_gap_s: 3.0,
            clamp_min_s: 3.0,
            clamp_max_s: 60.0,

            scan_sample_rate_hz: 48000,

            fp_win_s: 5.0,
            fp_thr: 0.6,
            fp_margin: 0.07,
            guard_s: 0.5,
            fp_arm_dbfs: -40.0,

            offline_sample_rate_hz: 0,

            enrich_song_path: String::new(),
            enrich_interval_length_s: 1.0,
            enrich_ping_length_s: 0.1,
            ffmpeg_path: String::from(".\\ffmpeg\\bin\\ffmpeg.exe"),
            impulse_listen_ms: 400,
            impulse_length_ms: 50.0,
            impulse_amplitude: 0.6,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ScanMeta {
    pub url: String, // optional tag in CSV
    pub input_path: String, // offline input path (.wav/.mp3/.mp4/.m4a)
}

fn print_usage(cfg: &Config) {
    println!("Usage: sonar_presence [OPTIONS]\n");
    println!("General paths:");
    println!("  --log-path <PATH>             Path to Detection.log (default: {})", cfg.log_path);
    println!(
        "  --scansong-path <PATH>        Path to SongScan.csv (default: {})",
        cfg.scansong_path
    );
    println!();
    println!(
        "  --log-level <LEVEL>           Log level: debug, info, warning, error (default: info)"
    );
    println!("Modes:");
    println!("  --mode presence       (default) Run ref↔mic presence detector");
    println!("  --mode scan           Pre-scan loopback audio and export best segments");
    println!("  --mode offline        Scan a local audio file directly (no playback)");
    println!(
        "  --mode gated          Presence, but only inside SongScan windows after 5s fingerprint align\n"
    );
    println!("  --mode enrich         Add sonar pings to audio file using FFmpeg\n");
    println!("  --mode impulse        Run impulse-based presence detector");

    println!("Presence options:");
    println!("  -tm, --tick-ms <MS>           Analyser tick in ms (default: {})", cfg.tick_ms);
    println!(
        "  -af, --agg-frac <FRAC>        Fraction of votes in window for 'present' [0..1] (default: {:.2})",
        cfg.agg_frac
    );
    println!(
        "  -ws, --window-sec <SEC>       Sliding window length in seconds (default: {})",
        cfg.window_sec
    );

    println!("\nPresence detection thresholds:");
    println!(
        "  --min-dwell-ms <MS>           Minimum dwell time for state change (default: {})",
        cfg.min_dwell_ms
    );
    println!(
        "  --exit-frac <FRAC>            Fraction to exit presence [0..1] (default: {:.2})",
        cfg.exit_frac
    );
    println!(
        "  --enter-frac <FRAC>           Fraction to enter presence [0..1] (default: {:.2})",
        cfg.enter_frac
    );
    println!(
        "  --front-min-m <M>             Minimum detection distance in meters (default: {:.1})",
        cfg.front_min_m
    );
    println!(
        "  --front-max-m <M>             Maximum detection distance in meters (default: {:.1})",
        cfg.front_max_m
    );
    println!(
        "  --strength-thr <FRAC>         Minimum strength threshold [0..1] (default: {:.2})",
        cfg.strength_thr
    );
    println!(
        "  --dist-max-m <M>              Maximum distance to report (default: {:.1})",
        cfg.dist_max_m
    );
    println!(
        "  --min-ref-rms <VAL>           Minimum reference RMS level (default: {:.5})",
        cfg.min_ref_rms
    );
    println!("  --min-rms <VAL>               Minimum mic RMS level (default: {:.5})", cfg.min_rms);

    println!("\nScan/Offline options:");
    println!("  --frame-ms <MS>               Analysis frame size (default: {:.0})", cfg.frame_ms);
    println!(
        "  --scan-window-s <SEC>         Scoring window size (default: {:.1})",
        cfg.scan_window_s
    );
    println!("  --stride-ms <MS>              Window stride (default: {:.0})", cfg.stride_ms);
    println!("  --hf-split-hz <HZ>            HF ratio split (default: {:.0})", cfg.hf_split_hz);
    println!("  --top-n <N>                   Max segments to keep (default: {})", cfg.top_n);
    println!(
        "  --min-percentile <PCT>        Score percentile threshold (default: {:.0})",
        cfg.min_percentile
    );
    println!(
        "  --nms-radius-s <SEC>          Peak suppression radius (default: {:.1})",
        cfg.nms_radius_s
    );
    println!(
        "  --merge-gap-s <SEC>           Merge winners with gaps ≤ this (default: {:.1})",
        cfg.merge_gap_s
    );
    println!(
        "  --clamp-min-s <SEC>           Minimum segment length (default: {:.1})",
        cfg.clamp_min_s
    );
    println!(
        "  --clamp-max-s <SEC>           Maximum segment length (default: {:.1})",
        cfg.clamp_max_s
    );
    println!(
        "  --sample-rate, --sr <HZ>      (scan) Loopback capture sample rate (default: {})",
        cfg.scan_sample_rate_hz
    );
    println!("  --scan-url <URL>              Tag CSV rows with this URL");
    println!(
        "  --input <PATH>                (offline) Audio file to analyze (.wav/.mp3/.mp4/.m4a)\n"
    );

    println!("Gated options:");
    println!(
        "  --fp-win-s <SEC>              Fingerprint window length (default: {:.1})",
        cfg.fp_win_s
    );
    println!(
        "  --fp-thr <FRAC>               Min similarity to accept [0..1] (default: {:.2})",
        cfg.fp_thr
    );
    println!(
        "  --fp-margin <FRAC>            Min top1-top2 margin (default: {:.2})",
        cfg.fp_margin
    );
    println!(
        "  --guard-s <SEC>               Guard band around segments (default: {:.1})",
        cfg.guard_s
    );
    println!(
        "  --fp-arm-dbfs <DB>            Loopback level to arm matching (default: {:.0})",
        cfg.fp_arm_dbfs
    );
    println!(
        "  --offline-sr <HZ>             (offline) Resample input to this rate before analysis (default: {}). Use 0 to keep native.",
        cfg.offline_sample_rate_hz
    );
    println!("\nEnrich options:");
    println!("  --song-path <PATH>            Input audio file to enrich with sonar pings");
    println!(
        "  --interval-length <SEC>       Time between ping bursts in seconds (default: {:.1})",
        cfg.enrich_interval_length_s
    );
    println!(
        "  --ping-length <SEC>           Duration of each ping burst in seconds (default: {:.1})",
        cfg.enrich_ping_length_s
    );
    println!(
        "  --ffmpeg-path <PATH>          Path to ffmpeg executable (default: {})",
        cfg.ffmpeg_path
    );

    println!("\nImpulse mode options:");
    println!(
        "  --impulse-listen-ms <MS>      Recording duration after impulse (default: {})",
        cfg.impulse_listen_ms
    );
    println!(
        "  --impulse-length-ms <MS>      Impulse signal duration (default: {})",
        cfg.impulse_length_ms
    );
    println!(
        "  --impulse-amplitude <VAL>     Impulse signal amplitude 0.0-1.0 (default: {})",
        cfg.impulse_amplitude
    );
    println!("\nExamples:");
    println!("  sonar_presence --mode presence -tm 200 -af 0.60 -ws 3");
    println!("  sonar_presence --mode scan --scan-url https://youtu.be/dQw4w9WgXcQ");
    println!("  sonar_presence --mode offline --input C:\\\\music\\\\track.mp3 --top-n 15");
    println!(
        "  sonar_presence --mode gated --scansong-path D:\\\\SongScan.csv --fp-thr 0.7 --fp-margin 0.1"
    );
    println!("  sonar_presence --mode enrich --song-path C:\\\\music\\\\track.mp3 ");
}

fn parse_arguments() -> std::result::Result<(Config, ScanMeta), String> {
    let args: Vec<String> = env::args().collect();
    let mut config = Config::default();
    let mut meta = ScanMeta::default();

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--mode" => {
                if i + 1 >= args.len() {
                    return Err("Missing value for --mode".to_string());
                }
                match args[i + 1].to_lowercase().as_str() {
                    "presence" => {
                        config.mode = Mode::Presence;
                    }
                    "scan" => {
                        config.mode = Mode::Scan;
                    }
                    "offline" => {
                        config.mode = Mode::Offline;
                    }
                    "gated" | "presence-gated" => {
                        config.mode = Mode::Gated;
                    }
                    "enrich" => {
                        config.mode = Mode::Enrich;
                    }
                    "impulse" => {
                        config.mode = Mode::Impulse;
                    }
                    other => {
                        return Err(format!("Unknown mode: {}", other));
                    }
                }
                i += 2;
            }
            "--log-path" => {
                if i + 1 >= args.len() {
                    return Err("Missing value for --log-path".to_string());
                }
                config.log_path = args[i + 1].to_string();
                i += 2;
            }
            "--log-level" => {
                if i + 1 >= args.len() {
                    return Err("Missing value for --log-level".to_string());
                }
                match args[i + 1].to_lowercase().as_str() {
                    "debug" => {
                        config.log_level = LogLevel::Debug;
                    }
                    "info" => {
                        config.log_level = LogLevel::Info;
                    }
                    "warning" | "warn" => {
                        config.log_level = LogLevel::Warning;
                    }
                    "error" => {
                        config.log_level = LogLevel::Error;
                    }
                    other => {
                        return Err(
                            format!("Invalid log level: {}. Valid options: debug, info, warning, error", other)
                        );
                    }
                }
                i += 2;
            }
            "--scansong-path" => {
                if i + 1 >= args.len() {
                    return Err("Missing value for --scansong-path".to_string());
                }
                config.scansong_path = args[i + 1].to_string();
                i += 2;
            }
            "-tm" | "--tick-ms" => {
                if i + 1 >= args.len() {
                    return Err("Missing value for tick-ms".to_string());
                }
                let v: u64 = args[i + 1].parse().map_err(|_| "Invalid tick-ms value".to_string())?;
                config.tick_ms = v.max(1);
                i += 2;
            }
            "-af" | "--agg-frac" => {
                if i + 1 >= args.len() {
                    return Err("Missing value for agg-frac".to_string());
                }
                let v: f32 = args[i + 1].parse().map_err(|_| "Invalid agg-frac value".to_string())?;
                config.agg_frac = v.clamp(0.0, 1.0);
                i += 2;
            }
            "-ws" | "--window-sec" => {
                if i + 1 >= args.len() {
                    return Err("Missing value for window-sec".to_string());
                }
                let v: u32 = args[i + 1]
                    .parse()
                    .map_err(|_| "Invalid window-sec value".to_string())?;
                config.window_sec = v.max(1);
                i += 2;
            }
            // New presence detection flags
            "--min-dwell-ms" => {
                if i + 1 >= args.len() {
                    return Err("Missing value for --min-dwell-ms".to_string());
                }
                config.min_dwell_ms = args[i + 1]
                    .parse()
                    .map_err(|_| "Invalid min-dwell-ms value".to_string())?;
                i += 2;
            }
            "--exit-frac" => {
                if i + 1 >= args.len() {
                    return Err("Missing value for --exit-frac".to_string());
                }
                config.exit_frac = args[i + 1]
                    .parse::<f32>()
                    .map_err(|_| "Invalid exit-frac value".to_string())?
                    .clamp(0.0, 1.0);
                i += 2;
            }
            "--enter-frac" => {
                if i + 1 >= args.len() {
                    return Err("Missing value for --enter-frac".to_string());
                }
                config.enter_frac = args[i + 1]
                    .parse::<f32>()
                    .map_err(|_| "Invalid enter-frac value".to_string())?
                    .clamp(0.0, 1.0);
                i += 2;
            }
            "--front-min-m" => {
                if i + 1 >= args.len() {
                    return Err("Missing value for --front-min-m".to_string());
                }
                config.front_min_m = args[i + 1]
                    .parse()
                    .map_err(|_| "Invalid front-min-m value".to_string())?;
                i += 2;
            }
            "--front-max-m" => {
                if i + 1 >= args.len() {
                    return Err("Missing value for --front-max-m".to_string());
                }
                config.front_max_m = args[i + 1]
                    .parse()
                    .map_err(|_| "Invalid front-max-m value".to_string())?;
                i += 2;
            }
            "--strength-thr" => {
                if i + 1 >= args.len() {
                    return Err("Missing value for --strength-thr".to_string());
                }
                config.strength_thr = args[i + 1]
                    .parse::<f32>()
                    .map_err(|_| "Invalid strength-thr value".to_string())?
                    .clamp(0.0, 1.0);
                i += 2;
            }
            "--dist-max-m" => {
                if i + 1 >= args.len() {
                    return Err("Missing value for --dist-max-m".to_string());
                }
                config.dist_max_m = args[i + 1]
                    .parse()
                    .map_err(|_| "Invalid dist-max-m value".to_string())?;
                i += 2;
            }
            "--min-ref-rms" => {
                if i + 1 >= args.len() {
                    return Err("Missing value for --min-ref-rms".to_string());
                }
                config.min_ref_rms = args[i + 1]
                    .parse()
                    .map_err(|_| "Invalid min-ref-rms value".to_string())?;
                i += 2;
            }
            "--min-rms" => {
                if i + 1 >= args.len() {
                    return Err("Missing value for --min-rms".to_string());
                }
                config.min_rms = args[i + 1]
                    .parse()
                    .map_err(|_| "Invalid min-rms value".to_string())?;
                i += 2;
            }
            // scan/offline options
            "--frame-ms" => {
                if i + 1 >= args.len() {
                    return Err("Missing value for frame-ms".to_string());
                }
                config.frame_ms = args[i + 1].parse().map_err(|_| "Invalid frame-ms".to_string())?;
                i += 2;
            }
            "--scan-window-s" => {
                if i + 1 >= args.len() {
                    return Err("Missing value for scan-window-s".to_string());
                }
                config.scan_window_s = args[i + 1]
                    .parse()
                    .map_err(|_| "Invalid scan-window-s".to_string())?;
                i += 2;
            }
            "--stride-ms" => {
                if i + 1 >= args.len() {
                    return Err("Missing value for stride-ms".to_string());
                }
                config.stride_ms = args[i + 1]
                    .parse()
                    .map_err(|_| "Invalid stride-ms".to_string())?;
                i += 2;
            }
            "--hf-split-hz" => {
                if i + 1 >= args.len() {
                    return Err("Missing value for hf-split-hz".to_string());
                }
                config.hf_split_hz = args[i + 1]
                    .parse()
                    .map_err(|_| "Invalid hf-split-hz".to_string())?;
                i += 2;
            }
            "--top-n" => {
                if i + 1 >= args.len() {
                    return Err("Missing value for top-n".to_string());
                }
                config.top_n = args[i + 1].parse().map_err(|_| "Invalid top-n".to_string())?;
                i += 2;
            }
            "--min-percentile" => {
                if i + 1 >= args.len() {
                    return Err("Missing value for min-percentile".to_string());
                }
                config.min_percentile = args[i + 1]
                    .parse()
                    .map_err(|_| "Invalid min-percentile".to_string())?;
                i += 2;
            }
            "--nms-radius-s" => {
                if i + 1 >= args.len() {
                    return Err("Missing value for nms-radius-s".to_string());
                }
                config.nms_radius_s = args[i + 1]
                    .parse()
                    .map_err(|_| "Invalid nms-radius-s".to_string())?;
                i += 2;
            }
            "--merge-gap-s" => {
                if i + 1 >= args.len() {
                    return Err("Missing value for merge-gap-s".to_string());
                }
                config.merge_gap_s = args[i + 1]
                    .parse()
                    .map_err(|_| "Invalid merge-gap-s".to_string())?;
                i += 2;
            }
            "--clamp-min-s" => {
                if i + 1 >= args.len() {
                    return Err("Missing value for clamp-min-s".to_string());
                }
                config.clamp_min_s = args[i + 1]
                    .parse()
                    .map_err(|_| "Invalid clamp-min-s".to_string())?;
                i += 2;
            }
            "--clamp-max-s" => {
                if i + 1 >= args.len() {
                    return Err("Missing value for clamp-max-s".to_string());
                }
                config.clamp_max_s = args[i + 1]
                    .parse()
                    .map_err(|_| "Invalid clamp-max-s".to_string())?;
                i += 2;
            }
            "--sample-rate" | "--sr" => {
                if i + 1 >= args.len() {
                    return Err("Missing value for --sample-rate/--sr".to_string());
                }
                let v: u32 = args[i + 1].parse().map_err(|_| "Invalid sample rate".to_string())?;
                if v == 0 {
                    return Err("sample rate must be > 0".to_string());
                }
                config.scan_sample_rate_hz = v;
                i += 2;
            }
            "--scan-url" => {
                if i + 1 >= args.len() {
                    return Err("Missing value for scan-url".to_string());
                }
                meta.url = args[i + 1].to_string();
                i += 2;
            }
            "--input" => {
                if i + 1 >= args.len() {
                    return Err("Missing value for --input".to_string());
                }
                meta.input_path = args[i + 1].to_string();
                i += 2;
            }
            "--fp-win-s" => {
                if i + 1 >= args.len() {
                    return Err("Missing value for fp-win-s".to_string());
                }
                config.fp_win_s = args[i + 1].parse().map_err(|_| "Invalid fp-win-s".to_string())?;
                i += 2;
            }
            "--fp-thr" => {
                if i + 1 >= args.len() {
                    return Err("Missing value for fp-thr".to_string());
                }
                config.fp_thr = args[i + 1].parse().map_err(|_| "Invalid fp-thr".to_string())?;
                i += 2;
            }
            "--fp-margin" => {
                if i + 1 >= args.len() {
                    return Err("Missing value for fp-margin".to_string());
                }
                config.fp_margin = args[i + 1]
                    .parse()
                    .map_err(|_| "Invalid fp-margin".to_string())?;
                i += 2;
            }
            "--guard-s" => {
                if i + 1 >= args.len() {
                    return Err("Missing value for guard-s".to_string());
                }
                config.guard_s = args[i + 1].parse().map_err(|_| "Invalid guard-s".to_string())?;
                i += 2;
            }
            "--fp-arm-dbfs" => {
                if i + 1 >= args.len() {
                    return Err("Missing value for fp-arm-dbfs".to_string());
                }
                config.fp_arm_dbfs = args[i + 1]
                    .parse()
                    .map_err(|_| "Invalid fp-arm-dbfs".to_string())?;
                i += 2;
            }
            "--offline-sr" => {
                if i + 1 >= args.len() {
                    return Err("Missing value for --offline-sr".to_string());
                }
                let v: u32 = args[i + 1].parse().map_err(|_| "Invalid offline-sr".to_string())?;
                config.offline_sample_rate_hz = v; // 0 => keep native
                i += 2;
            }
            "--song-path" => {
                if i + 1 >= args.len() {
                    return Err("Missing value for --song-path".to_string());
                }
                config.enrich_song_path = args[i + 1].to_string();
                i += 2;
            }
            "--interval-length" => {
                if i + 1 >= args.len() {
                    return Err("Missing value for --interval-length".to_string());
                }
                config.enrich_interval_length_s = args[i + 1]
                    .parse()
                    .map_err(|_| "Invalid interval-length value".to_string())?;
                i += 2;
            }
            "--ping-length" => {
                if i + 1 >= args.len() {
                    return Err("Missing value for --ping-length".to_string());
                }
                config.enrich_ping_length_s = args[i + 1]
                    .parse()
                    .map_err(|_| "Invalid ping-length value".to_string())?;
                i += 2;
            }
            "--ffmpeg-path" => {
                if i + 1 >= args.len() {
                    return Err("Missing value for --ffmpeg-path".to_string());
                }
                config.ffmpeg_path = args[i + 1].to_string();
                i += 2;
            }

            "--impulse-listen-ms" => {
                if i + 1 >= args.len() {
                    return Err("Missing value for --impulse-listen-ms".to_string());
                }
                config.impulse_listen_ms = args[i + 1]
                    .parse()
                    .map_err(|_| "Invalid impulse-listen-ms value")?;
                i += 2;
            }
            "--impulse-length-ms" => {
                if i + 1 >= args.len() {
                    return Err("Missing value for --impulse-length-ms".to_string());
                }
                config.impulse_length_ms = args[i + 1]
                    .parse()
                    .map_err(|_| "Invalid impulse-length-ms value")?;
                i += 2;
            }
            "--impulse-amplitude" => {
                if i + 1 >= args.len() {
                    return Err("Missing value for --impulse-amplitude".to_string());
                }
                config.impulse_amplitude = args[i + 1]
                    .parse::<f32>()
                    .map_err(|_| "Invalid impulse-amplitude value")?
                    .clamp(0.0, 1.0);
                i += 2;
            }
            "-h" | "--help" => {
                print_usage(&Config::default());
                std::process::exit(0);
            }
            _ => {
                return Err(format!("Unknown option: {}", args[i]));
            }
        }
    }

    Ok((config, meta))
}

// ───────────────────────────────────────────────────────────────────────────────
// Windows WASAPI loopback (reference capture)
// ───────────────────────────────────────────────────────────────────────────────
#[cfg(target_os = "windows")]
pub mod wasapi_loopback {
    use super::Logger;
    use anyhow::Context;
    use crossbeam_channel::{ bounded, Receiver, Sender };
    use std::{ sync::Arc, thread, time::Duration };
    use windows::{
        core::GUID,
        Win32::{
            Media::Audio::{
                eConsole,
                eRender,
                IAudioCaptureClient,
                IAudioClient,
                IMMDevice,
                IMMDeviceEnumerator,
                AUDCLNT_BUFFERFLAGS_SILENT,
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK,
                WAVEFORMATEX,
                WAVEFORMATEXTENSIBLE,
                MMDeviceEnumerator,
            },
            System::Com::{
                CoCreateInstance,
                CoInitializeEx,
                CoTaskMemFree,
                CoUninitialize,
                CLSCTX_ALL,
                COINIT_MULTITHREADED,
            },
        },
    };

    const WAVE_FORMAT_PCM_TAG: u16 = 0x0001;
    const WAVE_FORMAT_IEEE_FLOAT_TAG: u16 = 0x0003;
    const WAVE_FORMAT_EXTENSIBLE_TAG: u16 = 0xfffe;

    const KSDATAFORMAT_SUBTYPE_PCM: GUID = GUID::from_u128(0x00000001_0000_0010_8000_00aa00389b71);
    const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: GUID =
        GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

    pub fn start(
        target_sr: u32,
        logger: Arc<Logger>,
        tick_ms: u64
    ) -> anyhow::Result<Receiver<Vec<f32>>> {
        let (tx, rx) = bounded::<Vec<f32>>(8);

        thread::spawn(move || {
            if let Err(e) = capture_thread(target_sr, tx, logger, tick_ms) {
                eprintln!("WASAPI loopback thread error: {:?}", e);
            }
        });

        Ok(rx)
    }

    fn capture_thread(
        target_sr: u32,
        tx: Sender<Vec<f32>>,
        logger: Arc<Logger>,
        tick_ms: u64
    ) -> anyhow::Result<()> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;

            let enumerator: IMMDeviceEnumerator = CoCreateInstance(
                &MMDeviceEnumerator,
                None,
                CLSCTX_ALL
            )?;
            let device: IMMDevice = enumerator
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .context("GetDefaultAudioEndpoint failed")?;
            let audio_client: IAudioClient = device
                .Activate::<IAudioClient>(CLSCTX_ALL, None)
                .context("Activate IAudioClient failed")?;

            let pwfx: *mut WAVEFORMATEX = audio_client.GetMixFormat()?;
            let mix = *pwfx;
            let (in_sr, channels, fmt_tag, subfmt) = {
                let tag = mix.wFormatTag;
                let ch = mix.nChannels;
                let sr = mix.nSamplesPerSec;
                let sub = if tag == WAVE_FORMAT_EXTENSIBLE_TAG {
                    let wfxe = &*(pwfx as *const WAVEFORMATEXTENSIBLE);
                    wfxe.SubFormat
                } else if tag == WAVE_FORMAT_IEEE_FLOAT_TAG {
                    KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
                } else {
                    KSDATAFORMAT_SUBTYPE_PCM
                };
                (sr, ch, tag, sub)
            };

            let fmt_str = if fmt_tag == WAVE_FORMAT_EXTENSIBLE_TAG {
                if subfmt == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
                    "Float32 (extensible)"
                } else {
                    "PCM (extensible)"
                }
            } else if fmt_tag == WAVE_FORMAT_IEEE_FLOAT_TAG {
                "Float32"
            } else {
                "PCM"
            };

            let _ = logger.info(
                &format!(
                    "WASAPI loopback mix format: {} Hz, channels {}, {}",
                    in_sr,
                    channels,
                    fmt_str
                )
            )?;

            let hns_buffer_duration: i64 = 10_000_000 / 10; // 100ms

            audio_client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK,
                hns_buffer_duration,
                0,
                pwfx,
                None
            )?;
            CoTaskMemFree(Some(pwfx as *const _ as _));

            let capture: IAudioCaptureClient = audio_client.GetService()?;
            audio_client.Start()?;

            let mut leftover: Vec<f32> = Vec::new();

            loop {
                let mut p_data: *mut u8 = std::ptr::null_mut();
                let mut num_frames: u32 = 0;
                let mut flags: u32 = 0;
                let hr = capture.GetBuffer(&mut p_data, &mut num_frames, &mut flags, None, None);

                if hr.is_ok() && num_frames > 0 {
                    let mut mono = Vec::with_capacity(num_frames as usize);

                    let is_float =
                        fmt_tag == WAVE_FORMAT_IEEE_FLOAT_TAG ||
                        (fmt_tag == WAVE_FORMAT_EXTENSIBLE_TAG &&
                            subfmt == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT);

                    if (flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)) != 0 {
                        mono.resize(num_frames as usize, 0.0);
                    } else if is_float {
                        let slice = std::slice::from_raw_parts(
                            p_data as *const f32,
                            (num_frames * (channels as u32)) as usize
                        );
                        for f in 0..num_frames as usize {
                            mono.push(slice[f * (channels as usize)]); // first channel
                        }
                    } else {
                        let slice = std::slice::from_raw_parts(
                            p_data as *const i16,
                            (num_frames * (channels as u32)) as usize
                        );
                        for f in 0..num_frames as usize {
                            mono.push((slice[f * (channels as usize)] as f32) / 32768.0);
                        }
                    }

                    capture.ReleaseBuffer(num_frames)?;

                    leftover.extend_from_slice(&mono);
                    let mut chunk = ((target_sr as usize) * (tick_ms as usize)) / 1000;
                    if chunk == 0 {
                        chunk = 1;
                    }
                    while leftover.len() >= chunk {
                        let out = leftover.drain(0..chunk).collect::<Vec<f32>>();
                        if tx.send(out).is_err() {
                            audio_client.Stop()?;
                            CoUninitialize();
                            return Ok(());
                        }
                    }
                } else {
                    thread::sleep(Duration::from_millis(2));
                }
            }
        }
    }
}

#[cfg(not(target_os = "windows"))]
pub mod wasapi_loopback {
    use anyhow::Result;
    use crossbeam_channel::Receiver;
    use std::sync::Arc;
    use super::Logger;

    pub fn start(
        _target_sr: u32,
        _logger: Arc<Logger>,
        _tick_ms: u64
    ) -> Result<Receiver<Vec<f32>>> {
        anyhow::bail!("WASAPI loopback is only available on Windows")
    }
}

// ───────────────────────────────────────────────────────────────────────────────
// Optional: tiny built-in probe tone so loopback always has content
// ───────────────────────────────────────────────────────────────────────────────
#[cfg(target_os = "windows")]
pub const ENABLE_PROBE_TONE: bool = false;

#[cfg(target_os = "windows")]
pub fn start_probe(sr: u32) -> anyhow::Result<cpal::Stream> {
    use cpal::traits::{ DeviceTrait, HostTrait, StreamTrait };
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow::anyhow!("No default output device"))?;
    let mut cfg = device.default_output_config()?.config();
    cfg.sample_rate.0 = sr;

    let mut phase: f32 = 0.0;
    const FREQ: f32 = 18_000.0;
    const AMP: f32 = 0.02;
    let err_fn = |e| eprintln!("output stream error: {e}");
    let channels = cfg.channels as usize;

    let stream = match device.default_output_config()?.sample_format() {
        cpal::SampleFormat::F32 =>
            device.build_output_stream(
                &cfg,
                move |out: &mut [f32], _| {
                    for frame in out.chunks_mut(channels) {
                        phase += (2.0 * std::f32::consts::PI * FREQ) / (sr as f32);
                        if phase > 2.0 * std::f32::consts::PI {
                            phase -= 2.0 * std::f32::consts::PI;
                        }
                        let s = phase.sin() * AMP;
                        for ch in frame.iter_mut() {
                            *ch = s;
                        }
                    }
                },
                err_fn,
                None
            )?,
        cpal::SampleFormat::I16 =>
            device.build_output_stream(
                &cfg,
                move |out: &mut [i16], _| {
                    for frame in out.chunks_mut(channels) {
                        phase += (2.0 * std::f32::consts::PI * FREQ) / (sr as f32);
                        if phase > 2.0 * std::f32::consts::PI {
                            phase -= 2.0 * std::f32::consts::PI;
                        }
                        let s = (phase.sin() * AMP * 32767.0) as i16;
                        for ch in frame.iter_mut() {
                            *ch = s;
                        }
                    }
                },
                err_fn,
                None
            )?,
        cpal::SampleFormat::U16 =>
            device.build_output_stream(
                &cfg,
                move |out: &mut [u16], _| {
                    for frame in out.chunks_mut(channels) {
                        phase += (2.0 * std::f32::consts::PI * FREQ) / (sr as f32);
                        if phase > 2.0 * std::f32::consts::PI {
                            phase -= 2.0 * std::f32::consts::PI;
                        }
                        let s = ((phase.sin() * AMP * 0.5 + 0.5) * 65535.0) as u16;
                        for ch in frame.iter_mut() {
                            *ch = s;
                        }
                    }
                },
                err_fn,
                None
            )?,
        _ => anyhow::bail!("Unsupported output format"),
    };

    stream.play()?;
    Ok(stream)
}

// ───────────────────────────────────────────────────────────────────────────────
// Shared ring buffer (used by presence/gated)
// ───────────────────────────────────────────────────────────────────────────────
#[derive(Clone)]
pub struct SharedBuf {
    pub buf: Arc<Mutex<Vec<f32>>>, // mono ring buffer
    pub sr: Arc<Mutex<f32>>,
}

// ───────────────────────────────────────────────────────────────────────────────
// NEW: Scan feature extraction + fingerprint (used by scan/offline/gated)
// ───────────────────────────────────────────────────────────────────────────────
pub mod prescan {
    use realfft::RealFftPlanner;

    #[inline]
    fn hann(n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| {
                let t = (std::f32::consts::PI * (i as f32)) / (n as f32);
                t.sin() * t.sin()
            })
            .collect()
    }

    #[inline]
    pub fn rms(x: &[f32]) -> f32 {
        let e =
            x
                .iter()
                .map(|v| v * v)
                .sum::<f32>() / (x.len().max(1) as f32);
        e.sqrt()
    }

    #[inline]
    fn percentile(mut v: Vec<f32>, p: f32) -> f32 {
        if v.is_empty() {
            return 0.0;
        }
        v.retain(|x| x.is_finite());
        if v.is_empty() {
            return 0.0;
        }
        let p = p.clamp(0.0, 100.0);
        let k = ((p / 100.0) * ((v.len() - 1) as f32)).round() as usize;
        use std::cmp::Ordering;
        let (_, val, _) = v.select_nth_unstable_by(k, |a, b|
            a.partial_cmp(b).unwrap_or(Ordering::Equal)
        );
        *val
    }

    #[inline]
    fn median(mut v: Vec<f32>) -> f32 {
        if v.is_empty() {
            return 0.0;
        }
        v.retain(|x| x.is_finite());
        if v.is_empty() {
            return 0.0;
        }
        let n = v.len();
        let k = n / 2;
        use std::cmp::Ordering;
        let (lo, mid, _hi) = v.select_nth_unstable_by(k, |a, b|
            a.partial_cmp(b).unwrap_or(Ordering::Equal)
        );
        let mid_val = *mid;
        if n % 2 == 1 {
            mid_val
        } else {
            let max_lo = lo.iter().copied().fold(f32::NEG_INFINITY, f32::max);
            (max_lo + mid_val) * 0.5
        }
    }

    #[inline]
    fn mad_zscore(xs: &[f32], x: f32) -> f32 {
        if xs.is_empty() {
            return 0.0;
        }
        let m = median(xs.to_vec());
        let devs: Vec<f32> = xs
            .iter()
            .map(|v| (v - m).abs())
            .collect();
        let mad = median(devs).max(1e-6);
        (x - m) / (mad * 1.4826)
    }

    pub struct ScanParams {
        pub sr: f32,
        pub frame_ms: f32,
        pub window_s: f32,
        pub stride_ms: f32,
        pub hf_split_hz: f32,
        pub top_n: usize,
        pub min_percentile: f32,
        pub nms_radius_s: f32,
        pub merge_gap_s: f32,
        pub clamp_min_s: f32,
        pub clamp_max_s: f32,
    }

    #[derive(Clone)]
    pub struct WindowFeat {
        pub start_s: f32,
        pub end_s: f32,
        pub flux: f32,
        pub flatness: f32,
        pub crest_db: f32,
        pub bandwidth_hz_95: f32,
        pub hf_ratio: f32,
        pub dyn_range: f32,
        pub tonality: f32,
        pub loudness_dbfs: f32,
        pub score: f32,
        pub z: FeatZ,
    }

    #[derive(Clone, Default)]
    pub struct FeatZ {
        pub flux_z: f32,
        pub flatness_z: f32,
        pub crest_z: f32,
        pub bandwidth_z: f32,
        pub hf_ratio_z: f32,
        pub dynrange_z: f32,
        pub tonality_z: f32,
    }

    #[derive(Clone)]
    pub struct Segment {
        pub start_s: f32,
        pub end_s: f32,
        pub peak: WindowFeat,
    }

    /// Simple fingerprint: sequence of coarse-band peak indices.
    #[derive(Clone, Debug)]
    pub struct Fingerprint {
        pub fp_type: String, // "bandpeak_v1"
        pub bands: usize, // number of coarse bands
        pub hop_s: f32, // time between frames (seconds)
        pub offset_s: f32, // window start (relative to track start)
        pub bins: Vec<u8>, // per frame: argmax band index (0..bands-1)
    }

    /// Build a fingerprint from the most energetic `win_s` inside the first ~7s.
    pub fn make_fingerprint(samples: &[f32], sr: f32, win_s: f32) -> Option<Fingerprint> {
        if samples.is_empty() || sr <= 0.0 {
            return None;
        }
        let seek_s = (7.0f32).max(win_s + 1.0);
        let total_s = (samples.len() as f32) / sr;
        let search_s = seek_s.min(total_s).max(win_s);

        let win_len = (win_s * sr) as usize;
        if win_len < 512 {
            return None;
        }
        let search_len = (search_s * sr) as usize;
        if search_len < win_len {
            return None;
        }

        // Sliding RMS to find most energetic window
        let mut cur_e = 0.0f64;
        for k in 0..win_len {
            let v = samples[k] as f64;
            cur_e += v * v;
        }
        let mut best_e = cur_e;
        let mut best_i = 0usize;
        let mut i = 1usize;
        while i + win_len <= search_len {
            let add = samples[i + win_len - 1] as f64;
            let sub = samples[i - 1] as f64;
            cur_e += add * add - sub * sub;
            if cur_e > best_e {
                best_e = cur_e;
                best_i = i;
            }
            i += 1;
        }

        // Spectrogram params
        let mut planner = RealFftPlanner::<f32>::new();
        let frame_len = ((sr * 0.023) as usize).max(256).next_power_of_two();
        let hop_len = (frame_len / 2).max(1);
        let hann_win = super::prescan::hann(frame_len);
        let r2c = planner.plan_fft_forward(frame_len);
        let mut inbuf = vec![0.0f32; frame_len];
        let mut outbuf = r2c.make_output_vec();

        let n_bands = 32usize;
        let bin_hz = sr / (frame_len as f32);
        let max_hz = (6000.0f32).min(sr * 0.5 - bin_hz);
        let k_max = ((max_hz / bin_hz).floor() as usize).max(8);
        let band_size = (k_max / n_bands).max(1);

        // Walk frames across the selected window.
        let start = best_i;
        let end = start + win_len;
        let mut bins = Vec::<u8>::new();

        let mut pos = start;
        while pos + frame_len <= end {
            for j in 0..frame_len {
                inbuf[j] = samples[pos + j] * hann_win[j];
            }
            r2c.process(&mut inbuf, &mut outbuf).ok();

            // magnitude-squared energy per coarse band
            let mut band_e = vec![0.0f32; n_bands];
            for (k, c) in outbuf.iter().enumerate().take(k_max) {
                let b = (k / band_size).min(n_bands - 1);
                let v = c.norm_sqr();
                band_e[b] += v;
            }

            // pick peak band (ties → lower index)
            let mut best_b = 0usize;
            let mut best_v = -1.0f32;
            for b in 0..n_bands {
                if band_e[b] > best_v {
                    best_v = band_e[b];
                    best_b = b;
                }
            }
            bins.push(best_b as u8);

            pos += hop_len;
        }

        if bins.is_empty() {
            return None;
        }

        Some(Fingerprint {
            fp_type: "bandpeak_v1".to_string(),
            bands: n_bands,
            hop_s: (hop_len as f32) / sr,
            offset_s: (start as f32) / sr,
            bins,
        })
    }

    /// Compare two fingerprints; return similarity ∈ [0,1].
    /// Sweeps a small lag window (±0.5 s) and returns best coincidence ratio.
    pub fn fp_similarity(a: &Fingerprint, b: &Fingerprint) -> f32 {
        if a.fp_type != b.fp_type || a.bands != b.bands {
            return 0.0;
        }
        if a.bins.is_empty() || b.bins.is_empty() {
            return 0.0;
        }

        let step = a.hop_s.min(b.hop_s);
        let dur_a = (a.bins.len().saturating_sub(1) as f32) * a.hop_s;
        let dur_b = (b.bins.len().saturating_sub(1) as f32) * b.hop_s;
        let t_common = dur_a.min(dur_b);
        if t_common <= 0.0 {
            return 0.0;
        }

        let lag_max = 0.5_f32;
        let mut best = 0.0_f32;

        let mut lag = -lag_max;
        while lag <= lag_max + 1e-6 {
            let mut hits = 0usize;
            let mut total = 0usize;

            let mut t = 0.0_f32;
            while t <= t_common + 1e-6 {
                let ia = (t / a.hop_s).round() as isize;
                let ib = ((t + lag) / b.hop_s).round() as isize;
                if ia >= 0 && ib >= 0 {
                    let iau = ia as usize;
                    let ibu = ib as usize;
                    if iau < a.bins.len() && ibu < b.bins.len() {
                        if a.bins[iau] == b.bins[ibu] {
                            hits += 1;
                        }
                        total += 1;
                    }
                }
                t += step;
            }

            if total > 0 {
                let s = (hits as f32) / (total as f32);
                if s > best {
                    best = s;
                }
            }

            lag += step;
        }

        best
    }

    /// Compute per-window features and ranked segments
    pub fn analyze(samples: &[f32], p: &ScanParams) -> Vec<Segment> {
        if samples.len() < (p.sr as usize) {
            return vec![];
        }

        // --- frame-level processing
        let frame_len = (((p.sr * p.frame_ms) / 1000.0).round() as usize)
            .max(256)
            .next_power_of_two();
        let hop_len = (frame_len / 2).max(1);
        let hann_win = hann(frame_len);

        let mut planner = RealFftPlanner::<f32>::new();
        let r2c = planner.plan_fft_forward(frame_len);
        let mut inbuf = vec![0.0f32; frame_len];
        let mut outbuf = r2c.make_output_vec();

        let mut frame_mags: Vec<Vec<f32>> = Vec::new();
        let mut frame_rms: Vec<f32> = Vec::new();
        let mut frame_crest: Vec<f32> = Vec::new();
        let mut frame_times: Vec<f32> = Vec::new();

        let nframes = samples.len().saturating_sub(frame_len) / hop_len + 1;
        for f in 0..nframes {
            let start = f * hop_len;
            let end = start + frame_len;
            if end > samples.len() {
                break;
            }

            for i in 0..frame_len {
                inbuf[i] = samples[start + i] * hann_win[i];
            }

            let r = super::prescan::rms(&inbuf);
            let peak = inbuf.iter().fold(0.0_f32, |m, &v| m.max(v.abs()));
            let crest_db = if r > 1e-9 { 20.0 * (peak / r).log10().max(0.0) } else { 0.0 };

            r2c.process(&mut inbuf, &mut outbuf).ok();
            let mag: Vec<f32> = outbuf
                .iter()
                .map(|c| c.norm())
                .collect();

            frame_mags.push(mag);
            frame_rms.push(r);
            frame_crest.push(crest_db);
            frame_times.push((start as f32) / p.sr);
        }

        if frame_mags.is_empty() {
            return vec![];
        }

        let frames_per_win = ((p.window_s * p.sr) / (hop_len as f32)).round().max(1.0) as usize;
        let stride_frames = (((p.stride_ms / 1000.0) * p.sr) / (hop_len as f32))
            .round()
            .max(1.0) as usize;

        let bin_hz = p.sr / (frame_len as f32);
        let hf_bin = (p.hf_split_hz / bin_hz).floor() as usize;

        // spectral flux per frame
        let mut flux_per_frame: Vec<f32> = vec![0.0; frame_mags.len()];
        let mut prev: Option<&[f32]> = None;
        for (i, m) in frame_mags.iter().enumerate() {
            if let Some(pm) = prev {
                let mut flux = 0.0f32;
                for k in 0..m.len() {
                    let d = m[k] - pm[k];
                    if d > 0.0 {
                        flux += d;
                    }
                }
                flux_per_frame[i] = flux / (m.len() as f32);
            }
            prev = Some(m);
        }

        let mut wins: Vec<WindowFeat> = Vec::new();
        let total_frames = frame_mags.len();
        let window_len_s = ((frames_per_win * hop_len) as f32) / p.sr;

        let mut idx = 0usize;
        while idx + frames_per_win <= total_frames {
            let s_idx = idx;
            let e_idx = idx + frames_per_win;

            let mid = (s_idx + e_idx) / 2;
            let mag = &frame_mags[mid];
            let power: Vec<f32> = mag
                .iter()
                .map(|v| v * v)
                .collect();
            let total_e = power.iter().sum::<f32>().max(1e-12);

            // rolloff 95%
            let mut cume = 0.0f32;
            let mut roll95_bin = 0usize;
            for (k, pwr) in power.iter().enumerate() {
                cume += *pwr;
                if cume >= 0.95 * total_e {
                    roll95_bin = k;
                    break;
                }
            }
            let bandwidth_hz_95 = (roll95_bin as f32) * bin_hz;

            // flatness (GM/AM)
            let gm = (
                mag
                    .iter()
                    .map(|v| (v * v + 1e-12).ln())
                    .sum::<f32>() / (mag.len() as f32)
            ).exp();
            let am = power.iter().sum::<f32>() / (power.len() as f32);
            let flatness = (gm / am.max(1e-12)).clamp(0.0, 1.0);

            // HF ratio
            let hf_e = power
                .iter()
                .enumerate()
                .filter(|(k, _)| *k >= hf_bin)
                .map(|(_, v)| *v)
                .sum::<f32>();
            let hf_ratio = (hf_e / total_e).clamp(0.0, 1.0);

            // crest / flux / loudness / dyn range in window
            let crest_db = percentile(frame_crest[s_idx..e_idx].to_vec(), 75.0);
            let flux = percentile(flux_per_frame[s_idx..e_idx].to_vec(), 90.0);
            let r_med = median(frame_rms[s_idx..e_idx].to_vec());
            let loudness_dbfs = if r_med > 1e-9 { 20.0 * r_med.log10() } else { -120.0 };
            let r95 = percentile(frame_rms[s_idx..e_idx].to_vec(), 95.0);
            let r50 = percentile(frame_rms[s_idx..e_idx].to_vec(), 50.0);
            let dyn_range = (20.0 * (r95.max(1e-9) / r50.max(1e-9)).log10()).max(0.0);

            let start_s = frame_times[s_idx];
            let end_s = start_s + window_len_s;
            wins.push(WindowFeat {
                start_s,
                end_s,
                flux,
                flatness,
                crest_db,
                bandwidth_hz_95,
                hf_ratio,
                dyn_range,
                tonality: (1.0 - flatness).clamp(0.0, 1.0),
                loudness_dbfs,
                score: 0.0,
                z: FeatZ::default(),
            });

            idx += stride_frames;
        }

        if wins.is_empty() {
            return vec![];
        }

        // z-scores + scoring
        let collect = |f: &dyn Fn(&WindowFeat) -> f32| -> Vec<f32> { wins.iter().map(f).collect() };
        let xs_flux = collect(&(|w| w.flux));
        let xs_flat = collect(&(|w| w.flatness));
        let xs_crest = collect(&(|w| w.crest_db));
        let xs_bw = collect(&(|w| w.bandwidth_hz_95));
        let xs_hf = collect(&(|w| w.hf_ratio));
        let xs_dr = collect(&(|w| w.dyn_range));
        let xs_tone = collect(&(|w| w.tonality));

        for w in wins.iter_mut() {
            let z = FeatZ {
                flux_z: mad_zscore(&xs_flux, w.flux),
                flatness_z: mad_zscore(&xs_flat, w.flatness),
                crest_z: mad_zscore(&xs_crest, w.crest_db),
                bandwidth_z: mad_zscore(&xs_bw, w.bandwidth_hz_95),
                hf_ratio_z: mad_zscore(&xs_hf, w.hf_ratio),
                dynrange_z: mad_zscore(&xs_dr, w.dyn_range),
                tonality_z: mad_zscore(&xs_tone, w.tonality),
            };

            let mut score =
                0.25 * z.flux_z +
                0.2 * z.flatness_z +
                0.2 * z.crest_z +
                0.15 * z.bandwidth_z +
                0.1 * z.hf_ratio_z +
                0.1 * z.dynrange_z -
                0.2 * z.tonality_z;

            if w.loudness_dbfs < -45.0 {
                score -= 0.5;
            }
            if w.loudness_dbfs < -60.0 {
                score -= 1.0;
            }

            w.z = z;
            w.score = score as f32;
        }

        // local peaks above percentile + NMS + merge + clamp
        let scores: Vec<f32> = wins
            .iter()
            .map(|w| w.score)
            .collect();
        let thr = percentile(scores, p.min_percentile);
        let radius = (p.nms_radius_s / (p.stride_ms / 1000.0)).round().max(1.0) as usize;

        let mut keep: Vec<usize> = Vec::new();
        for i in 0..wins.len() {
            if wins[i].score < thr {
                continue;
            }
            let i0 = i.saturating_sub(radius);
            let i1 = (i + radius).min(wins.len() - 1);
            let mut is_peak = true;
            for j in i0..=i1 {
                if j != i && wins[j].score >= wins[i].score {
                    is_peak = false;
                    break;
                }
            }
            if is_peak {
                keep.push(i);
            }
        }
        keep.sort_by(|&a, &b| wins[b].score.partial_cmp(&wins[a].score).unwrap());
        if keep.len() > p.top_n {
            keep.truncate(p.top_n);
        }

        let mut seg_windows: Vec<WindowFeat> = keep
            .iter()
            .map(|&i| wins[i].clone())
            .collect();
        seg_windows.sort_by(|a, b| a.start_s.partial_cmp(&b.start_s).unwrap());

        let mut segs: Vec<Segment> = Vec::new();
        for w in seg_windows {
            if let Some(last) = segs.last_mut() {
                if w.start_s <= last.end_s + p.merge_gap_s {
                    last.end_s = last.end_s.max(w.end_s);
                    if w.score > last.peak.score {
                        last.peak = w.clone();
                    }
                    continue;
                }
            }
            segs.push(Segment { start_s: w.start_s, end_s: w.end_s, peak: w.clone() });
        }

        for s in segs.iter_mut() {
            let dur = s.end_s - s.start_s;
            if dur < p.clamp_min_s {
                s.end_s = (s.start_s + p.clamp_min_s).min(s.start_s + p.clamp_max_s);
            } else if dur > p.clamp_max_s {
                s.end_s = s.start_s + p.clamp_max_s;
            }
        }

        segs
    }
}

// ───────────────────────────────────────────────────────────────────────────────
// Decoder for WAV/MP3/MP4 (AAC) using symphonia (used by offline mode)
// ───────────────────────────────────────────────────────────────────────────────
pub mod decode {
    use std::{ fs::File, path::Path };
    use symphonia::core::{
        audio::SampleBuffer,
        codecs::DecoderOptions,
        errors::Error,
        formats::FormatOptions,
        io::MediaSourceStream,
        meta::MetadataOptions,
        probe::Hint,
    };
    use symphonia::default::{ get_codecs, get_probe };

    #[derive(Debug)]
    pub struct AudioData {
        pub sr: u32,
        pub channels: u16,
        pub samples_mono: Vec<f32>, // first channel only
    }

    pub fn load_first_channel<P: AsRef<Path>>(path: P) -> anyhow::Result<AudioData> {
        let path_ref = path.as_ref();

        let file = File::open(path_ref)?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());

        let mut hint = Hint::new();
        if let Some(ext) = path_ref.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }

        let probed = get_probe().format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default()
        )?;
        let mut format = probed.format;

        let (track_id, codec_params) = {
            let track = format
                .default_track()
                .ok_or_else(|| anyhow::anyhow!("no default audio track found"))?;
            (track.id, track.codec_params.clone())
        };

        let mut decoder = get_codecs().make(&codec_params, &DecoderOptions::default())?;

        let sr = codec_params.sample_rate.ok_or_else(|| anyhow::anyhow!("unknown sample rate"))?;
        let channels = codec_params.channels.map(|c| c.count() as u16).unwrap_or(1u16);

        let mut sample_buf: Option<SampleBuffer<f32>> = None;
        let mut mono = Vec::<f32>::new();

        loop {
            let packet = match format.next_packet() {
                Ok(packet) => packet,
                Err(Error::ResetRequired) => {
                    decoder.reset();
                    continue;
                }
                Err(Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    break;
                }
                Err(err) => {
                    return Err(err.into());
                }
            };

            if packet.track_id() != track_id {
                continue;
            }

            let decoded = match decoder.decode(&packet) {
                Ok(decoded) => decoded,
                Err(Error::DecodeError(_)) => {
                    continue;
                }
                Err(err) => {
                    return Err(err.into());
                }
            };

            let spec = *decoded.spec();
            let chan_count = spec.channels.count();

            if
                sample_buf
                    .as_ref()
                    .map(|b| b.capacity() < decoded.capacity())
                    .unwrap_or(true)
            {
                sample_buf = Some(SampleBuffer::<f32>::new(decoded.capacity() as u64, spec));
            }
            let buf = sample_buf.as_mut().unwrap();

            buf.copy_interleaved_ref(decoded);
            let samples = buf.samples();

            for i in (0..samples.len()).step_by(chan_count) {
                mono.push(samples[i]);
            }
        }

        Ok(AudioData { sr, channels, samples_mono: mono })
    }
}

// ───────────────────────────────────────────────────────────────────────────────
// Shared helpers used by multiple modes
// ───────────────────────────────────────────────────────────────────────────────
pub fn audio_sink_thread(rx: Receiver<Vec<f32>>, shared: SharedBuf) {
    loop {
        match rx.recv() {
            Ok(block) => {
                let mut ring = shared.buf.lock().unwrap();
                ring.extend_from_slice(&block);
                let cap = (*shared.sr.lock().unwrap() as usize) * 10;
                if ring.len() > cap {
                    let drop = ring.len() - cap;
                    ring.drain(0..drop);
                }
            }
            Err(_) => {
                break;
            }
        }
    }
}

pub fn build_input_stream(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    channels: usize,
    tx: crossbeam_channel::Sender<Vec<f32>>,
    logger: Arc<Logger>
) -> Result<cpal::Stream> {
    let err_logger = logger.clone();
    let err_fn = move |e| {
        let _ = err_logger.error(&format!("audio stream error: {}", e));
    };

    match device.default_input_config()?.sample_format() {
        cpal::SampleFormat::F32 => {
            let tx = tx.clone();
            Ok(
                device.build_input_stream(
                    config,
                    move |data: &[f32], _| on_audio_input_first_channel(data, channels, &tx),
                    err_fn,
                    None
                )?
            )
        }
        cpal::SampleFormat::I16 => {
            let tx = tx.clone();
            Ok(
                device.build_input_stream(
                    config,
                    move |data: &[i16], _| {
                        let mut tmp = Vec::with_capacity(data.len());
                        for &s in data {
                            tmp.push((s as f32) / 32768.0);
                        }
                        on_audio_input_first_channel(&tmp, channels, &tx);
                    },
                    err_fn,
                    None
                )?
            )
        }
        cpal::SampleFormat::U16 => {
            let tx = tx.clone();
            Ok(
                device.build_input_stream(
                    config,
                    move |data: &[u16], _| {
                        let mut tmp = Vec::with_capacity(data.len());
                        for &s in data {
                            tmp.push(((s as f32) / 65535.0) * 2.0 - 1.0);
                        }
                        on_audio_input_first_channel(&tmp, channels, &tx);
                    },
                    err_fn,
                    None
                )?
            )
        }
        _ => anyhow::bail!("Unsupported sample format"),
    }
}

fn on_audio_input_first_channel<T: AsRef<[f32]>>(
    data: T,
    channels: usize,
    tx: &crossbeam_channel::Sender<Vec<f32>>
) {
    let data = data.as_ref();
    if channels == 1 {
        let _ = tx.send(data.to_vec());
    } else {
        let frames = data.len() / channels;
        let mut mono = Vec::with_capacity(frames);
        for f in 0..frames {
            mono.push(data[f * channels]); // first channel only
        }
        let _ = tx.send(mono);
    }
}

pub fn maybe_rate_supported(device: &cpal::Device, want: u32) -> Option<u32> {
    if let Ok(mut configs) = device.supported_input_configs() {
        for c in configs.by_ref() {
            let r = c.min_sample_rate().0..=c.max_sample_rate().0;
            if r.contains(&want) {
                return Some(want);
            }
        }
    }
    None
}

// ───────────────────────────────────────────────────────────────────────────────
// main
// ───────────────────────────────────────────────────────────────────────────────
fn main() -> Result<()> {
    let (cli, scan_meta) = match parse_arguments() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {}\n", e);
            print_usage(&Config::default());
            std::process::exit(1);
        }
    };

    let logger = Arc::new(Logger::new_with_level(&cli.log_path, true, cli.log_level)?);

    match cli.mode {
        Mode::Presence => mods::presence::run_presence(&cli, logger, &cli.log_path),
        Mode::Scan => mods::scan::run_scan(&cli, &scan_meta, logger),
        Mode::Offline => mods::offline::run_offline(&cli, &scan_meta, logger),
        Mode::Gated => mods::gated::run_gated(&cli, logger),
        Mode::Enrich => mods::enrich::run_enrich(&cli, logger),
        Mode::Impulse => mods::impulse::run_impulse(&cli, logger), // Add this
    }
}
