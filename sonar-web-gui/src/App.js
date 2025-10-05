import React, { useEffect, useMemo, useRef, useState, useCallback } from "react";
import './index.css';
import { SonarAudioGraph } from './sonarModules/audiograph';
import { RangeDopplerDisplay } from './sonarModules/rangedoppler';

// ============= SONAR DEMO COMPONENT =============
const SonarDemo = () => {
  const [settings, setSettings] = useState({
    fc: 18000,
    bandwidth: 0,
    nSlow: 20,
    chirpLength: 0,
    clutterFilter: 'slow',
    offsetCompensation: true,
    windowFFT: true
  });
  
  const [sonarInfo, setSonarInfo] = useState({});
  const [isRunning, setIsRunning] = useState(false);
  const [inputLevel, setInputLevel] = useState(0);
  
  const canvasRef = useRef(null);
  const audioGraphRef = useRef(null);
  const displayRef = useRef(null);
  const animationFrameRef = useRef(null);

  const fs = 44100;

  const calculateParams = useCallback(() => {
    const bandwidth = Math.round(40 * Math.pow(2, settings.bandwidth / 4)) * 100;
    const impulseLength = Math.round(512 * Math.pow(2, settings.chirpLength / 4));
    const decimation = Math.floor(fs / (bandwidth * 1.3));
    const n_fast = Math.ceil(impulseLength / decimation);
    const impulseDuration = impulseLength / fs;
    const PRF = 1 / impulseDuration;
    const wavelength = 343 / settings.fc;
    const CPI = (impulseLength * settings.nSlow) / fs;
    const rangeResolution = (343 / fs) * decimation;
    const velocityResolution = (1 / CPI) * wavelength;
    const rangeAmbiguity = impulseDuration * 343;
    const velocityAmbiguity = (0.5 / impulseDuration) * wavelength;
    const integrationGain = 10 * Math.log10(bandwidth * CPI);

    return {
      bandwidth,
      impulseLength,
      decimation,
      n_fast,
      impulseDuration,
      PRF,
      wavelength,
      CPI,
      rangeResolution,
      velocityResolution,
      rangeAmbiguity,
      velocityAmbiguity,
      integrationGain
    };
  }, [settings, fs]);

  useEffect(() => {
    const params = calculateParams();
    setSonarInfo(params);
  }, [calculateParams]);

  // Cleanup on unmount
  useEffect(() => {
    return () => {
      if (audioGraphRef.current) {
        audioGraphRef.current.stop();
      }
    };
  }, []);

  const handleStart = async () => {
    if (isRunning) {
      // Stop
      if (audioGraphRef.current) {
        await audioGraphRef.current.stop();
      }
      setIsRunning(false);
    } else {
      // Start
      try {
        const params = calculateParams();
        audioGraphRef.current = new SonarAudioGraph({
          impulseLength: params.impulseLength,
          fc: settings.fc,
          bandwidth: params.bandwidth,
          decimation: params.decimation,
          n_slow: settings.nSlow,
          clutterFilterOption: settings.clutterFilter,
          track_offset: settings.offsetCompensation,
          apply_window: settings.windowFFT,
        });

        // Set up display
        if (canvasRef.current) {
          displayRef.current = new RangeDopplerDisplay(canvasRef.current);
          displayRef.current.updateDimensions(params.n_fast, settings.nSlow);
        }

        // Set up message handler
        audioGraphRef.current.onWorkletMessage = (ev) => {
          if (ev.data.fast_slow && displayRef.current) {
            displayRef.current.draw(ev.data.fast_slow);
          }
          if (ev.data.peak !== undefined) {
            setInputLevel(ev.data.peak * 100);
          }
        };

        await audioGraphRef.current.start();
        setIsRunning(true);
      } catch (error) {
        console.error("Failed to start sonar:", error);
        alert("Failed to start sonar. Make sure you allow microphone access.");
        setIsRunning(false);
      }
    }
  };

  return (
    <div className="sonar-demo">
      <div className="sonar-header">
        <h2>Pulse-Doppler Sonar Demo</h2>
      </div>
      
      <div className="rd-plot-container">
        <div className="velocity-axis-label">bistatic velocity</div>
        <div id="velocity-axis-ticks"></div>
        <canvas
          ref={canvasRef}
          id="rangedoppler-canvas"
          width="128"
          height="20"
          style={{ width: '100%', height: '400px', imageRendering: 'pixelated', background: 'black' }}
        />
        <div id="range-axis-ticks"></div>
        <div className="range-axis-label">bistatic range</div>
      </div>

      <div className="sonar-controls">
        <div className="control-section">
          <button onClick={handleStart} className="sonar-button">
            {isRunning ? 'Stop' : 'Start'}
          </button>
          <div className="input-level">
            <span>Peak input level: </span>
            <meter value={inputLevel} min="0" max="100" low="40" high="90" />
            <span>{Math.round(inputLevel)}%</span>
          </div>
        </div>

        <div className="settings-grid">
          <div className="setting-item">
            <label>
              Center frequency: <strong>{settings.fc} Hz</strong>
            </label>
            <input 
              type="range" 
              min="1000" 
              max="22000" 
              step="500" 
              value={settings.fc}
              onChange={(e) => setSettings({...settings, fc: Number(e.target.value)})}
            />
          </div>

          <div className="setting-item">
            <label>
              Nominal bandwidth: <strong>{sonarInfo.bandwidth} Hz</strong>
            </label>
            <input 
              type="range" 
              min="-8" 
              max="8" 
              step="1" 
              value={settings.bandwidth}
              onChange={(e) => setSettings({...settings, bandwidth: Number(e.target.value)})}
            />
          </div>

          <div className="setting-item">
            <label>
              Chirp length: <strong>{sonarInfo.impulseLength} samples</strong>
              ({(sonarInfo.impulseDuration * 1000 || 0).toFixed(2)} ms)
            </label>
            <input 
              type="range" 
              min="-8" 
              max="10" 
              step="1" 
              value={settings.chirpLength}
              onChange={(e) => setSettings({...settings, chirpLength: Number(e.target.value)})}
            />
          </div>

          <div className="setting-item">
            <label>
              Chirps per CPI: <strong>{settings.nSlow}</strong>
            </label>
            <input 
              type="range" 
              min="1" 
              max="201" 
              step="2" 
              value={settings.nSlow}
              onChange={(e) => setSettings({...settings, nSlow: Number(e.target.value)})}
            />
          </div>
        </div>

        <div className="clutter-settings">
          <fieldset>
            <legend>Clutter Filtering</legend>
            <label>
              <input 
                type="radio" 
                name="clutter" 
                value="none"
                checked={settings.clutterFilter === 'none'}
                onChange={(e) => setSettings({...settings, clutterFilter: e.target.value})}
              />
              None
            </label>
            <label>
              <input 
                type="radio" 
                name="clutter" 
                value="two-pulse"
                checked={settings.clutterFilter === 'two-pulse'}
                onChange={(e) => setSettings({...settings, clutterFilter: e.target.value})}
              />
              Two-pulse canceller
            </label>
            <label>
              <input 
                type="radio" 
                name="clutter" 
                value="slow"
                checked={settings.clutterFilter === 'slow'}
                onChange={(e) => setSettings({...settings, clutterFilter: e.target.value})}
              />
              Subtract clutter map
            </label>
            <label>
              <input 
                type="radio" 
                name="clutter" 
                value="remove-zero"
                checked={settings.clutterFilter === 'remove-zero'}
                onChange={(e) => setSettings({...settings, clutterFilter: e.target.value})}
              />
              Remove v=0 slice
            </label>
          </fieldset>
        </div>

        <div className="checkbox-settings">
          <label>
            <input 
              type="checkbox"
              checked={settings.offsetCompensation}
              onChange={(e) => setSettings({...settings, offsetCompensation: e.target.checked})}
            />
            Compensate audio delay
          </label>
          <label>
            <input 
              type="checkbox"
              checked={settings.windowFFT}
              onChange={(e) => setSettings({...settings, windowFFT: e.target.checked})}
            />
            Apply window before FFT
          </label>
        </div>

        <div className="info-panels">
          <div className="info-panel">
            <h3>Sonar Parameters</h3>
            <div className="info-item">
              <span>Range resolution:</span>
              <strong>{(sonarInfo.rangeResolution * 100 || 0).toFixed(2)} cm</strong>
            </div>
            <div className="info-item">
              <span>Velocity resolution:</span>
              <strong>{(sonarInfo.velocityResolution * 100 || 0).toFixed(2)} cm/s</strong>
            </div>
            <div className="info-item">
              <span>Range ambiguity:</span>
              <strong>{(sonarInfo.rangeAmbiguity || 0).toFixed(2)} m</strong>
            </div>
            <div className="info-item">
              <span>Velocity ambiguity:</span>
              <strong>{(sonarInfo.velocityAmbiguity || 0).toFixed(2)} m/s</strong>
            </div>
            <div className="info-item">
              <span>PRF:</span>
              <strong>{(sonarInfo.PRF || 0).toFixed(0)} Hz</strong>
            </div>
            <div className="info-item">
              <span>CPI:</span>
              <strong>{(sonarInfo.CPI * 1000 || 0).toFixed(0)} ms</strong>
            </div>
          </div>
        </div>
      </div>
    </div>
  );
};

// ============= MONITOR COMPONENT (Your existing code) =============
const LOG_URL = "/Detection.log";
const CSV_URL = "/Detection.csv";

const PRESENT_LINE_RE = /present=(true|false)\s+avg_distance_m=([0-9.]+|inf)\s+avg_strength=([0-9.]+)\s+window=(\d+)s\s+agree=(\d+)%/i;

function parseLog(text) {
  const lines = text.split(/\r?\n/).filter(Boolean);
  const parsed = [];

  for (const line of lines) {
    const m = line.match(/^\[(.+?) UTC\]\s+\[(\w+)\]\s+(.*)$/);
    const ts = m ? `${m[1]} UTC` : null;
    const body = m ? m[3] : line;

    const p = body.match(PRESENT_LINE_RE);
    if (p) {
      parsed.push({
        ts,
        present: p[1].toLowerCase() === "true",
        distance: p[2] === "inf" ? Infinity : parseFloat(p[2]),
        strength: parseFloat(p[3]),
        windowSec: parseInt(p[4], 10),
        agreePct: parseInt(p[5], 10),
        raw: line,
      });
    }
  }

  return { parsed, rawLines: lines };
}

function parseCsv(text) {
  const lines = text.split(/\r?\n/).filter(Boolean);
  if (!lines.length) return [];

  const hasHeader = /^timestamp\s*,\s*present\s*,\s*avg_distance_m\s*,\s*avg_strength\s*,\s*agree_pct\s*$/i.test(lines[0]);
  const startIdx = hasHeader ? 1 : 0;

  const events = [];
  for (let i = startIdx; i < lines.length; i++) {
    const line = lines[i];
    const parts = line.split(",");
    if (parts.length < 2) continue;

    const ts = (parts[0] || "").trim();
    const presentStr = (parts[1] || "").trim().toLowerCase();
    const present = presentStr === "true" || presentStr === "1";
    const distance = parts[2] && parts[2].trim().length ? Number(parts[2].trim()) : undefined;
    const strength = parts[3] && parts[3].trim().length ? Number(parts[3].trim()) : undefined;
    const agreePct = parts[4] && parts[4].trim().length ? Number(parts[4].trim()) : undefined;

    events.push({ ts, present, distance, strength, agreePct, raw: line });
  }

  return events;
}

function StatusPill({ present }) {
  return (
    <span
      style={{
        display: "inline-flex",
        alignItems: "center",
        gap: "0.5rem",
        padding: "0.35rem 0.7rem",
        borderRadius: "999px",
        background: present ? "rgba(0,160,60,0.12)" : "rgba(200,0,0,0.12)",
        color: present ? "#0d7a38" : "#b00020",
        fontWeight: 600,
      }}
    >
      <span
        style={{
          width: 10,
          height: 10,
          borderRadius: "50%",
          background: present ? "#14ae5c" : "#ff3b30",
          boxShadow: present
            ? "0 0 0 6px rgba(20,174,92,0.15)"
            : "0 0 0 6px rgba(255,59,48,0.15)",
        }}
      />
      {present ? "PRESENT" : "ABSENT"}
    </span>
  );
}

function Progress({ value }) {
  return (
    <div
      style={{
        width: 140,
        height: 8,
        background: "rgba(0,0,0,0.08)",
        borderRadius: 999,
        overflow: "hidden",
      }}
      title={`strength ${value.toFixed(2)}`}
    >
      <div
        style={{
          width: `${Math.max(0, Math.min(1, value)) * 100}%`,
          height: "100%",
          background: "linear-gradient(90deg, #8fd3f4, #84fab0)",
        }}
      />
    </div>
  );
}

const Monitor = () => {
  const [pollMs, setPollMs] = useState(1000);
  const [live, setLive] = useState(true);
  const [data, setData] = useState({ parsed: [], events: [], rawLines: [] });
  const [errors, setErrors] = useState({ log: null, csv: null });
  const scrollRef = useRef(null);

  const last = useMemo(
    () => (data.parsed.length ? data.parsed[data.parsed.length - 1] : null),
    [data.parsed]
  );

  useEffect(() => {
    if (!live) return;
    let mounted = true;
    let timer = null;

    const tick = async () => {
      try {
        const [logRes, csvRes] = await Promise.all([
          fetch(`${LOG_URL}?t=${Date.now()}`, { cache: "no-store" }),
          fetch(`${CSV_URL}?t=${Date.now()}`, { cache: "no-store" }),
        ]);

        let logText = "";
        let csvText = "";

        if (!logRes.ok) {
          throw new Error(`LOG HTTP ${logRes.status}`);
        } else {
          logText = await logRes.text();
        }

        if (!csvRes.ok) {
          setErrors((e) => ({
            ...e,
            csv: "Cannot read Detection.csv. State changes list will be empty until CSV is available.",
          }));
          csvText = "";
        } else {
          csvText = await csvRes.text();
          setErrors((e) => ({ ...e, csv: null }));
        }

        if (!mounted) return;
        const { parsed, rawLines } = parseLog(logText);
        const events = csvText ? parseCsv(csvText) : [];
        setData({ parsed, events, rawLines });
        setErrors((e) => ({ ...e, log: null }));
      } catch (e) {
        if (!mounted) return;
        setErrors((prev) => ({
          ...prev,
          log: "Cannot read Detection.log. Is the EXE running and writing to /public/Detection.log?",
        }));
      } finally {
        if (mounted) {
          timer = setTimeout(tick, pollMs);
        }
      }
    };

    tick();
    return () => {
      mounted = false;
      if (timer) clearTimeout(timer);
    };
  }, [pollMs, live]);

  useEffect(() => {
    if (scrollRef.current) {
      scrollRef.current.scrollTop = scrollRef.current.scrollHeight;
    }
  }, [data.rawLines]);

  return (
    <>
      <div className="toolbar-actions">
        <label className="toggle">
          <input type="checkbox" checked={live} onChange={(e) => setLive(e.target.checked)} />
          <span>{live ? "Live" : "Paused"}</span>
        </label>
        <label>
          Poll:
          <select value={pollMs} onChange={(e) => setPollMs(parseInt(e.target.value, 10))}>
            <option value={500}>0.5s</option>
            <option value={1000}>1s</option>
            <option value={2000}>2s</option>
            <option value={3000}>3s</option>
          </select>
        </label>
        <button onClick={() => setData({ parsed: [], events: [], rawLines: [] })}>
          Clear View
        </button>
      </div>

      <section className="cards">
        <div className="card status">
          <div className="row">
            <StatusPill present={last?.present ?? false} />
            <div className="last-ts">
              {last?.ts ? `Last window: ${last.ts}` : "Waiting for log…"}
            </div>
          </div>

          <div className="metrics">
            <div className="metric">
              <div className="label">Avg distance</div>
              <div className="value">
                {last
                  ? last.distance === Infinity
                    ? "—"
                    : `${last.distance.toFixed(2)} m`
                  : "—"}
              </div>
            </div>
            <div className="metric">
              <div className="label">Strength</div>
              <div className="value">
                <Progress value={last?.strength ?? 0} />
              </div>
            </div>
            <div className="metric">
              <div className="label">Agree</div>
              <div className="value">{last ? `${last.agreePct}%` : "—"}</div>
            </div>
            <div className="metric">
              <div className="label">Window</div>
              <div className="value">{last ? `${last.windowSec}s` : "—"}</div>
            </div>
          </div>
        </div>

        <div className="card events">
          <h3>State changes</h3>
          <ul>
            {data.events.slice(-10).map((e, i) => (
              <li key={i}>
                <span className={`chip ${e.present ? "ok" : "bad"}`}>
                  {e.present ? "present" : "absent"}
                </span>
                <span className="ts">{e.ts || ""}</span>
              </li>
            ))}
            {!data.events.length && <li className="muted">No changes yet</li>}
          </ul>
        </div>
      </section>

      <section className="card log">
        <div className="log-header">
          <h3>Detection.log</h3>
          <span className="muted">showing last {Math.min(500, data.rawLines.length)} lines</span>
        </div>
        <pre className="log-view" ref={scrollRef}>
          {data.rawLines.slice(-500).join("\n")}
        </pre>
        {(errors.log || errors.csv) && (
          <div className="error">
            {errors.log && <div>{errors.log}</div>}
            {errors.csv && <div>{errors.csv}</div>}
          </div>
        )}
      </section>
    </>
  );
};

// ============= MAIN APP WITH TABS =============
export default function App() {
  const [activeTab, setActiveTab] = useState('monitor');
console.log(activeTab,"activeTabactiveTabactiveTabactiveTab");

  return (
    <div className="wrap">
      <header className="toolbar">
        <h1>Sonar System</h1>
        <div className="tab-buttons">
          <button 
            className={`tab-button ${activeTab === 'monitor' ? 'active' : ''}`}
            onClick={() => setActiveTab('monitor')}
          >
            Presence Monitor
          </button>
          <button 
            className={`tab-button ${activeTab === 'demo' ? 'active' : ''}`}
            onClick={() => setActiveTab('demo')}
          >
            Sonar Demo
          </button>
        </div>
      </header>

      {activeTab === 'monitor' ? (
        <>
          <Monitor />
          <footer className="muted">
            Reads <code>public/Detection.log</code> for live metrics and{" "}
            <code>public/Detection.csv</code> for state changes (no backend). Keep
            the EXE running so both files update.
          </footer>
        </>
      ) : (
        <SonarDemo />
      )}
    </div>
  );
}