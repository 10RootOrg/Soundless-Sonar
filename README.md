# Sonar Presence

Low-latency **acoustic presence detection** and **music segment pre-scanning** for Windows.  
Three modes:

- **Presence** — Estimates whether a person is in front of the device by correlating **render (loopback)** with **microphone** to detect near-field echoes. Writes state changes to CSV and a rolling log.
- **Scan** — Captures **system output only** (WASAPI loopback) while you play audio (e.g., YouTube), ranks “sonar-friendly” segments, and appends them to a CSV.
- **Offline** — Analyzes a local audio file (wav/mp3/mp4/m4a) directly without playback; writes top segments to CSV.

---

## Table of Contents

- [Quick Start](#quick-start)
- [Installation](#installation)
- [Platforms & Requirements](#platforms--requirements)
- [Offline Mode — Supported Audio Formats](#offline-mode--supported-audio-formats)
- [Modes & How They Work](#modes--how-they-work)
- [Command Line Usage](#command-line-usage)
- [Outputs (CSV & Logs)](#outputs-csv--logs)
- [Keep Your Output Mix Clean (Scan Mode)](#keep-your-output-mix-clean-scan-mode)
- [Tips & Best Practices](#tips--best-practices)
- [Troubleshooting](#troubleshooting)
- [FAQ](#faq)
- [Privacy](#privacy)
- [License](#license)

---
to run the already build zip you need c++ 2015 redistributable 

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

> **Tip (Windows)**: Set **Playback sample rate to 48,000 Hz** (Sound settings → your output device → Advanced) for accurate Scan timestamps.

---

## Installation

- Requires **Rust (edition 2021)** and a stable toolchain.
- Build in release for real-time audio:
  ```bash
  cargo build --release
  ```
  Release profile is tuned for small, LTO’d binaries:
  ```toml
  [profile.release]
  opt-level = "s"
  lto = true
  codegen-units = 1
  ```

---

## Platforms & Requirements

- **Windows 10/11**: Full functionality (Presence + Scan + Offline).
  - Scan and loopback capture rely on **WASAPI loopback** via the Windows SDK (`windows` crate).
- **Non-Windows**:
  - **Offline** mode works (file decoding via **symphonia**).
  - **Presence** and **Scan** require loopback capture; the WASAPI module isn’t available and returns a clear error.

Hardware:

- A **microphone** and **speakers/headphones** for Presence.
- For Scan, **only system output** is captured (mic should _not_ be mixed into output).

---

## Offline Mode — Supported Audio Formats

The **Offline** scanner decodes local files using the bundled `symphonia` decoders enabled in this project (`mkv`, `isomp4`, `wav`, `mp3`, `aac`, `vorbis`, `flac`).

| Container                | Common extensions | Supported codec(s) (enabled) | Notes                                                                                                    |
| ------------------------ | ----------------- | ---------------------------- | -------------------------------------------------------------------------------------------------------- |
| **WAVE**                 | `.wav`            | PCM / IEEE Float             | Uncompressed LPCM (16/24/32-bit) and Float in WAV.                                                       |
| **MP3**                  | `.mp3`            | MP3                          | Elementary MP3 streams.                                                                                  |
| **MP4 / M4A** (ISO-BMFF) | `.mp4`, `.m4a`    | AAC                          | Preferred container for AAC audio.                                                                       |
| **FLAC**                 | `.flac`           | FLAC                         | Lossless FLAC files.                                                                                     |
| **Matroska**             | `.mkv`            | Vorbis, FLAC, MP3†           | Decodes tracks for which a corresponding enabled decoder exists. †Support depends on the embedded codec. |

> **Not enabled** in this build: Ogg container (`.ogg`), Opus, ALAC, AIFF, WMA, DRM/protected media.  
> **Raw AAC (`.aac`, ADTS)** may not be recognized reliably—use **`.m4a`/`.mp4`** when possible.

**Decoding behavior (Offline):**

- Only the **first (default) audio track** is used.
- Audio is downmixed by taking the **first channel only** (mono analysis).
- The file’s **native sample rate** is used for analysis as-is.

---

## Modes & How They Work

### Presence (ref↔mic correlation + sliding aggregator)

- Captures:
  - **Reference**: Default render device via **WASAPI loopback** (output you’re playing).
  - **Mic**: Default input via **CPAL** (prefers 48 kHz if supported).
- Processing:
  - DC removal, simple pre-emphasis, L2 normalization.
  - Normalized cross-correlation over lags: detect **direct path** and search the **echo band** corresponding to ~**0.3–1.5 m**.
  - A robust **prominence score** (echo vs local percentiles) acts as “strength”.
  - Converts lag to **distance** using speed of sound and halves for round-trip.
- Decision:
  - Per-tick votes are fed to a **sliding window aggregator** with **hysteresis**:
    - Enter at **62%** agreement, exit at **38%**, **min dwell 1.5 s** to avoid flapping.
- Output:
  - On state flips, appends a row to `Detection.csv` with timestamp, presence, average distance, strength, and agreement %.
  - Verbose lines go to `Detection.log`.

### Scan (render-only)

- Records **loopback** while you play audio on the PC; press **Ctrl+C** to analyze.
- Feature pipeline (3 s windows by default, strided):
  - STFT magnitudes → features: **spectral flux**, **flatness**, **crest (dB)**, **95% rolloff bandwidth**, **HF ratio**, **dynamic range**, **tonality (1-flatness)**, **loudness (dBFS)**.
  - Robust **median/MAD z-scoring** and weighted sum → **score**.
  - Percentile threshold + **NMS** + **merge** + **duration clamp** → top segments.
- Output:
  - Appends rows to `SongScan.csv`; can tag with `--scan-url`.

### Offline (file-based scan)

- Decodes the first channel of a local **WAV/MP3/MP4/M4A/FLAC/MKV** (per table above).
- Runs the same **Scan** pipeline at the file’s native sample rate.
- Tags rows with `--scan-url` if provided, else a `file://...` tag.

---

## Command Line Usage

```text
--mode presence|scan|offline    # default: presence
# General paths
--log-path <PATH>               # Detection.log (default points to a Windows path)
--scansong-path <PATH>          # SongScan.csv (default: sibling next to log)
# Presence
-tm, --tick-ms <MS>             # analyzer tick (default: 250)
-af, --agg-frac <FRAC>          # window agreement threshold [0..1] (default: 0.50)
-ws, --window-sec <SEC>         # sliding window length (default: 3)
# Scan/Offline (feature/scoring)
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
--scan-url <URL>                # tag rows (e.g., the YouTube URL)
--input <PATH>                  # required in --mode offline
-h, --help
```

**Examples**

```bash
# Presence, slightly faster ticks and tighter window
sonar-presence --mode presence -tm 200 -af 0.60 -ws 3

# Scan with URL tag
sonar-presence --mode scan --scan-url https://youtu.be/k1-TrAvp_xs

# Offline with custom top-N
sonar-presence --mode offline --input "C:\music\track.mp3" --top-n 15
```

---

## Outputs (CSV & Logs)

### `Detection.csv` (written next to `--log-path`)

Header:

```
timestamp,present,avg_distance_m,avg_strength,agree_pct
```

- **timestamp**: Local time when state flips.
- **present**: `true`/`false` after hysteresis + dwell.
- **avg_distance_m**: Mean estimated person distance across the window (∞ when not present).
- **avg_strength**: Mean echo prominence (0–1).
- **agree_pct**: % of valid votes asserting presence in the window.

### `SongScan.csv`

Header:

```
url,start_s,end_s,score,frame_ms,window_s,stride_s,bandwidth_z,flatness_z,flux_z,crest_db,hf_ratio,dynrange_z,tonality_z,loudness_dbfs,notes
```

- **url**: Tag from `--scan-url` or `file://…` in Offline mode.
- **start_s / end_s**: Segment bounds (may be clamped to min/max).
- **score**: Weighted feature score (higher is better).
- **frame_ms / window_s / stride_s**: Analysis parameters used.
- **bandwidth_z, flatness_z, flux_z, dynrange_z, tonality_z**: Median/MAD z-scores.
- **crest_db**: 75th-percentile frame crest within window.
- **hf_ratio**: HF energy / total.
- **loudness_dbfs**: Median RMS (dBFS).
- **notes**: Reserved.

### Logs

- **Default log path**: `Detection.log` (overridable with `--log-path`).
- Contains device info, timing, and per-tick summaries during Presence.

---

## Keep Your Output Mix Clean (Scan Mode)

> Ensure your **output mix is clean (no microphone)** so Scan mode works as intended (WASAPI loopback captures _only_ system output).

### TL;DR — Turn These Off

- [ ] Windows **“Listen to this device”** on your microphone
- [ ] **Direct monitoring** of the mic via interface/DAW/OBS
- [ ] **Virtual devices** (Stereo Mix / “What U Hear” / virtual cables) that route mic → output

If any of these are enabled, room sound (talking, claps) will leak into the loopback capture.

### Why It Matters

- **Scan mode** records **render/output** only. If your system mixes the mic into output, scans will reflect the room.
- **Presence mode** _does_ use the mic; the checklist above is **about Scan**, not Presence.

### Disable Common Mic→Output Paths

**Windows: “Listen to this device”**

1. Speaker icon → **Sound settings**
2. **More sound settings** → **Recording** → **Microphone → Properties**
3. **Listen** tab → **Uncheck** _Listen to this device_ → **OK**
   - Ensure **Playback through this device** is **not** your speakers.

**Audio Interface**

- Turn **Direct Monitor** **Off**, or set the **mix** knob to **Playback/PC** only.
- In vendor mixers (Focusrite/MOTU/RME), **mute** mic channels feeding the monitor bus.

**DAW (Ableton/FL/Reaper/Logic)**

- On the mic track, set **Monitoring = Off**; ensure no sends/returns route mic → Master during scans.

**OBS**

- **Settings → Audio** or **Advanced Audio Properties**: set **Mic/Aux → Audio Monitoring = Monitor Off**.
- Don’t mix mic into **Desktop Audio** or **Audio Output Capture** that hits your OS output.

**Virtual Devices**

- Disable **Stereo Mix / What U Hear**; don’t set them as Default.
- For virtual cables, ensure the mic isn’t routed to your default Playback device; mute it if unsure.

### 2-Minute Self-Test

1. Play a song on the PC.
2. **Speak/clap** loudly.
3. Run **Scan** and analyze. If your voice/claps change results, mic leakage still exists.

---

## Tips & Best Practices

- **Sample rate**: Presence will prefer 48 kHz for the mic if supported; set your **Playback device to 48 kHz** for tighter Scan timestamps.
- **Quiet rooms**: Presence uses RMS gates to avoid false work when both streams are silent.
- **Front distance band**: Presence focuses on ~**0.3–1.5 m** echoes after the direct path.
- **Latency budget**: Accounts for up to **200 ms** render→mic device latency before searching for echoes.
- **Stopping**: Press **Ctrl+C** in Scan/Offline to finalize analysis and write CSV rows.

---

## Troubleshooting

- **“No default input device (microphone) found”**  
  Plug in a mic or set a default input (Windows Sound → Recording).

- **Presence never flips to true**  
  Ensure the render output is audible (or disable any output device muting). The presence estimator needs _both_ a reference signal and a mic pickup of its echo.

- **Scan shows room sounds**  
  Revisit the **Clean Output Mix** checklist; something is mixing mic → output.

- **“IAudioClient Initialize failed” or loopback device errors**  
  Close apps that may be holding exclusive control. In device **Properties → Advanced**, consider disabling **exclusive mode**.

- **WASAPI loopback on non-Windows**  
  Not supported. Use **Offline** mode to analyze files.

---

## FAQ

**Does the app emit sound?**  
By default, no. There’s a tiny optional **18 kHz probe tone** (disabled in code) meant to keep loopback “alive” if needed.

**What distances does it report?**  
Presence clamps reported distance to **≤ 1.5 m**; strength is a normalized echo prominence (0–1).

**Why do Presence CSV rows only appear occasionally?**  
Rows are written **on state changes** (after hysteresis + dwell), not every tick.

**Can I tag Scan results with a source?**  
Yes, use `--scan-url`, which becomes the `url` column; Offline mode tags with `file://...` if no URL is given.

---

## Privacy

- All analysis is **local**.
- Files written: `Detection.log`, `Detection.csv`, and `SongScan.csv` at paths you control.
- No network activity or telemetry is performed.

---

## License

(Apache-2.0).
