# Soundless Sonar

[![License](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](LICENSE_APACHE)
[![Rust](https://img.shields.io/badge/Rust-1.70+-orange.svg)](https://www.rust-lang.org/)
[![Platform](https://img.shields.io/badge/Platform-Windows%2010%2F11-lightgrey.svg)](https://www.microsoft.com/windows)

Low-latency **acoustic presence detection** and **music segment pre-scanning** for Windows.

## Features

- **Presence Detection** — Estimates whether a person is in front of the device by correlating render (loopback) with microphone to detect near-field echoes. Writes state changes to CSV and a rolling log.
- **Scan Mode** — Captures system output only (WASAPI loopback) while you play audio (e.g., YouTube), ranks "sonar-friendly" segments, and appends them to a CSV.
- **Offline Mode** — Analyzes a local audio file (WAV/MP3/MP4/M4A/FLAC/MKV) directly without playback; writes top segments to CSV.

---

## Table of Contents

- [Quick Start](#quick-start)
- [Installation](#installation)
- [Platforms & Requirements](#platforms--requirements)
- [Supported Audio Formats](#supported-audio-formats)
- [Modes & How They Work](#modes--how-they-work)
- [Command Line Usage](#command-line-usage)
- [Output Files](#output-files)
- [Keep Your Output Mix Clean](#keep-your-output-mix-clean)
- [Tips & Best Practices](#tips--best-practices)
- [Troubleshooting](#troubleshooting)
- [FAQ](#faq)
- [Privacy](#privacy)
- [License](#license)

---

## Quick Start

```bash
# Build
cargo build --release

# Presence detection (default)
target/release/sonar-presence

# Scan mode (play a track on your PC; press Ctrl+C to analyze)
target/release/sonar-presence --mode scan --scan-url https://www.youtube.com/watch?v=XXXX

# Offline mode (analyze a file directly)
target/release/sonar-presence --mode offline --input "C:\music\track.mp3"
```

> **Tip (Windows)**: Set **Playback sample rate to 48,000 Hz** (Sound settings > your output device > Advanced) for accurate Scan timestamps.

> **Note**: To run the pre-built zip, you need C++ 2015 redistributable installed.

---

## Installation

### Prerequisites

- **Rust** (edition 2021) with a stable toolchain
- **Windows 10/11** for full functionality
- **C++ 2015 Redistributable** (for pre-built binaries)

### Building from Source

```bash
cargo build --release
```

The release profile is tuned for optimized binaries:

```toml
[profile.release]
opt-level = "s"
lto = true
codegen-units = 1
```

---

## Platforms & Requirements

| Platform | Presence | Scan | Offline |
|----------|----------|------|---------|
| Windows 10/11 | Full | Full | Full |
| Linux/macOS | N/A | N/A | Full |

- **Windows**: Full functionality using WASAPI loopback via the Windows SDK
- **Non-Windows**: Only Offline mode works (file decoding via symphonia)

### Hardware Requirements

- **Microphone** and **speakers/headphones** for Presence mode
- For Scan mode, only system output is captured (mic should *not* be mixed into output)

---

## Supported Audio Formats

| Container | Extensions | Supported Codecs | Notes |
|-----------|------------|------------------|-------|
| **WAVE** | `.wav` | PCM / IEEE Float | Uncompressed LPCM (16/24/32-bit) |
| **MP3** | `.mp3` | MP3 | Elementary MP3 streams |
| **MP4/M4A** | `.mp4`, `.m4a` | AAC | Preferred container for AAC |
| **FLAC** | `.flac` | FLAC | Lossless FLAC files |
| **Matroska** | `.mkv` | Vorbis, FLAC, MP3 | Depends on embedded codec |

> **Note**: Ogg container (`.ogg`), Opus, ALAC, AIFF, and WMA are not enabled. Raw AAC (`.aac`, ADTS) may not be recognized reliably—use `.m4a`/`.mp4` instead.

---

## Modes & How They Work

### Presence Mode

Detects human presence by analyzing acoustic echoes:

1. **Captures** reference audio via WASAPI loopback and microphone input via CPAL
2. **Processes** with DC removal, pre-emphasis, and L2 normalization
3. **Correlates** to detect echoes in the **0.3–1.5m** range
4. **Decides** using a sliding window aggregator with hysteresis (enter at 62%, exit at 38%, min dwell 1.5s)
5. **Outputs** state changes to `Detection.csv` with timestamp, presence, distance, strength, and agreement %

### Scan Mode

Analyzes audio for "sonar-friendly" segments:

1. Records loopback while you play audio; press **Ctrl+C** to analyze
2. Extracts features: spectral flux, flatness, crest, rolloff bandwidth, HF ratio, dynamic range, tonality, loudness
3. Applies robust median/MAD z-scoring and weighted sum scoring
4. Uses percentile threshold + NMS + merge + duration clamp to find top segments
5. Outputs results to `SongScan.csv`

### Offline Mode

Same as Scan but operates on local files:

- Decodes the first channel of local audio files
- Runs the same feature pipeline at the file's native sample rate
- Tags results with `--scan-url` or generates a `file://...` tag

---

## Command Line Usage

```
--mode presence|scan|offline    # default: presence

# General paths
--log-path <PATH>               # Detection.log location
--scansong-path <PATH>          # SongScan.csv location

# Presence options
-tm, --tick-ms <MS>             # analyzer tick (default: 250)
-af, --agg-frac <FRAC>          # window agreement threshold [0..1] (default: 0.50)
-ws, --window-sec <SEC>         # sliding window length (default: 3)

# Scan/Offline options
--frame-ms <MS>                 # STFT frame size (default: 23)
--scan-window-s <SEC>           # analysis window (default: 3.0)
--stride-ms <MS>                # window stride (default: 200)
--hf-split-hz <HZ>              # HF band split (default: 2500)
--top-n <N>                     # max segments to keep (default: 20)
--min-percentile <PCT>          # score percentile threshold (default: 85)
--nms-radius-s <SEC>            # suppression radius (default: 1.0)
--merge-gap-s <SEC>             # merge gap (default: 3.0)
--clamp-min-s <SEC>             # min segment length (default: 3.0)
--clamp-max-s <SEC>             # max segment length (default: 60.0)
--scan-url <URL>                # tag rows (e.g., YouTube URL)
--input <PATH>                  # required for offline mode

-h, --help
```

### Examples

```bash
# Presence with custom parameters
sonar-presence --mode presence -tm 200 -af 0.60 -ws 3

# Scan with URL tag
sonar-presence --mode scan --scan-url https://youtu.be/k1-TrAvp_xs

# Offline with custom top-N
sonar-presence --mode offline --input "C:\music\track.mp3" --top-n 15
```

---

## Output Files

### Detection.csv (Presence Mode)

```csv
timestamp,present,avg_distance_m,avg_strength,agree_pct
```

| Column | Description |
|--------|-------------|
| `timestamp` | Local time when state flips |
| `present` | `true`/`false` after hysteresis |
| `avg_distance_m` | Mean estimated distance (infinity when not present) |
| `avg_strength` | Mean echo prominence (0–1) |
| `agree_pct` | % of votes asserting presence |

### SongScan.csv (Scan/Offline Mode)

```csv
url,start_s,end_s,score,frame_ms,window_s,stride_s,bandwidth_z,flatness_z,flux_z,crest_db,hf_ratio,dynrange_z,tonality_z,loudness_dbfs,notes
```

### Detection.log

Contains device info, timing, and per-tick summaries during Presence mode.

---

## Keep Your Output Mix Clean

For Scan mode to work correctly, ensure your output mix is clean (no microphone):

### Disable These Settings

- Windows **"Listen to this device"** on your microphone
- **Direct monitoring** via audio interface/DAW/OBS
- **Virtual devices** (Stereo Mix / virtual cables) routing mic to output

### Quick Test

1. Play a song on your PC
2. Speak or clap loudly
3. Run Scan and analyze—if your voice/claps appear in results, mic leakage exists

---

## Tips & Best Practices

- **Sample Rate**: Set playback device to 48 kHz for accurate timestamps
- **Quiet Rooms**: Presence uses RMS gates to avoid false positives in silence
- **Detection Range**: Presence focuses on ~0.3–1.5m echoes
- **Latency Budget**: Accounts for up to 200ms render-to-mic device latency
- **Stopping**: Press Ctrl+C in Scan/Offline to finalize analysis

---

## Troubleshooting

| Issue | Solution |
|-------|----------|
| "No default input device found" | Plug in a mic or set a default input in Windows Sound settings |
| Presence never detects | Ensure render output is audible; both reference signal and mic pickup are required |
| Scan shows room sounds | Check the Clean Output Mix section above |
| "IAudioClient Initialize failed" | Close apps with exclusive audio control; disable exclusive mode in device properties |
| WASAPI on non-Windows | Not supported; use Offline mode instead |

---

## FAQ

**Does the app emit sound?**
By default, no. There's an optional 18 kHz probe tone (disabled) to keep loopback active if needed.

**What distances does it report?**
Presence clamps distance to ≤1.5m; strength is normalized echo prominence (0–1).

**Why do Presence CSV rows appear infrequently?**
Rows are written only on state changes (after hysteresis + dwell), not every tick.

**Can I tag Scan results with a source?**
Yes, use `--scan-url`. Offline mode uses `file://...` if no URL is provided.

---

## Privacy

- All analysis is performed **locally**
- Files written: `Detection.log`, `Detection.csv`, `SongScan.csv` at paths you control
- **No network activity or telemetry**

---

## License

This project is licensed under the **Apache License 2.0** - see [LICENSE_APACHE](LICENSE_APACHE) for details.

### Third-Party Licenses

This project uses the following third-party libraries:

#### Direct Dependencies

| Crate | License | Description |
|-------|---------|-------------|
| [cpal](https://github.com/RustAudio/cpal) (0.15) | Apache-2.0 | Cross-platform audio I/O library |
| [anyhow](https://github.com/dtolnay/anyhow) (1.0) | MIT OR Apache-2.0 | Flexible concrete Error type |
| [rustfft](https://github.com/ejmahler/RustFFT) (6.1) | MIT OR Apache-2.0 | High-performance FFT library |
| [ndarray](https://github.com/rust-ndarray/ndarray) (0.15) | MIT OR Apache-2.0 | N-dimensional array library |
| [ndarray-stats](https://github.com/rust-ndarray/ndarray-stats) (0.5) | MIT OR Apache-2.0 | Statistical routines for ndarray |
| [plotters](https://github.com/plotters-rs/plotters) (0.3) | MIT | Data visualization library |
| [ctrlc](https://github.com/Detegr/rust-ctrlc) (3.2) | MIT OR Apache-2.0 | Ctrl-C handler |
| [serde](https://github.com/serde-rs/serde) (1.0) | MIT OR Apache-2.0 | Serialization framework |
| [serde_json](https://github.com/serde-rs/json) (1.0) | MIT OR Apache-2.0 | JSON serialization |
| [hound](https://github.com/ruuda/hound) (3.5) | Apache-2.0 | WAV encoding/decoding library |
| [chrono](https://github.com/chronotope/chrono) (0.4) | MIT OR Apache-2.0 | Date and time library |

#### Notable Transitive Dependencies

| Crate | License | Description |
|-------|---------|-------------|
| num-complex | MIT OR Apache-2.0 | Complex numbers |
| num-traits | MIT OR Apache-2.0 | Numeric traits |
| num-integer | MIT OR Apache-2.0 | Integer traits and functions |
| image | MIT | Image processing library |
| png | MIT OR Apache-2.0 | PNG image format support |
| gif | MIT OR Apache-2.0 | GIF image format support |
| font-kit | MIT OR Apache-2.0 | Font loading library |
| libc | MIT OR Apache-2.0 | Raw FFI bindings to platform libraries |
| proc-macro2 | MIT OR Apache-2.0 | Procedural macro support |
| syn | MIT OR Apache-2.0 | Rust syntax parsing |
| quote | MIT OR Apache-2.0 | Quasi-quoting for proc macros |
| byteorder | MIT OR Unlicense | Reading/writing numbers in big/little endian |
| cfg-if | MIT OR Apache-2.0 | Compile-time conditional configuration |
| lazy_static | MIT OR Apache-2.0 | Lazy static initialization |
| once_cell | MIT OR Apache-2.0 | Single assignment cells |
| thiserror | MIT OR Apache-2.0 | Derive macro for std::error::Error |
| walkdir | MIT OR Unlicense | Recursive directory walking |
| regex | MIT OR Apache-2.0 | Regular expressions |
| memchr | MIT OR Unlicense | Optimized string searching |
| rand | MIT OR Apache-2.0 | Random number generation |
| flate2 | MIT OR Apache-2.0 | DEFLATE compression |
| crc32fast | MIT OR Apache-2.0 | Fast CRC32 checksums |
| itertools | MIT OR Apache-2.0 | Extra iterator adaptors |
| indexmap | MIT OR Apache-2.0 | Hash table with consistent ordering |
| log | MIT OR Apache-2.0 | Lightweight logging facade |

#### Platform-Specific Dependencies

**Windows**
| Crate | License |
|-------|---------|
| windows, windows-sys, winapi | MIT OR Apache-2.0 |
| dwrote | MPL-2.0 |

**macOS**
| Crate | License |
|-------|---------|
| core-foundation, core-graphics, core-text, coreaudio-rs | MIT OR Apache-2.0 |
| mach2 | BSD-2-Clause |

**Linux**
| Crate | License |
|-------|---------|
| alsa, alsa-sys | MIT OR Apache-2.0 |
| fontconfig-sys | MIT |
| freetype-sys | MIT |

**Android**
| Crate | License |
|-------|---------|
| ndk, ndk-sys, oboe | MIT OR Apache-2.0 |
| jni | MIT OR Apache-2.0 |

**WebAssembly**
| Crate | License |
|-------|---------|
| wasm-bindgen, web-sys, js-sys | MIT OR Apache-2.0 |
