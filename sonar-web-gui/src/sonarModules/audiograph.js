// src/sonarModules/audiograph.js
import { generateChirp, hannWindow, rectWindow } from "./chirp.js";

export class SonarAudioGraph {
  constructor(sonarParameters) {
    this.initialized = false;
    this.onWorkletMessage = (ev) => {
      console.error("audio graph callback is not registered", ev);
    };
    this.sonarParameters = sonarParameters;
    this.audioContext = null;
    this.sampleRate = 44100;
  }

  async initialize() {
    const impulseLength = this.sonarParameters.impulseLength;
    const fc = this.sonarParameters.fc;
    const bandwidth = this.sonarParameters.bandwidth;

    this.audioContext = new AudioContext({
      latencyHint: "playback",
    });

    this.sampleRate = this.audioContext.sampleRate;
    console.log("audiocontext fs", this.sampleRate);
    const normalizedCarrier = fc / this.sampleRate;

    const chirp = generateChirp(this.sampleRate, impulseLength, fc, bandwidth);
    this.chirpSource = this.initAudioOutput(chirp);

    const n_slow = this.sonarParameters.n_slow;
    let slow_time_window = this.sonarParameters.apply_window 
      ? hannWindow(n_slow) 
      : rectWindow(n_slow);

    const tau = 0.1;
    const clutter_alpha = impulseLength / (this.sampleRate * tau);

    this.sonarProcessor = await this.initSonarWorklet({
      chirp,
      normalizedCarrier,
      clutter_alpha,
      slow_time_window,
      decimation: this.sonarParameters.decimation,
      clutterFilterOption: this.sonarParameters.clutterFilterOption,
      track_offset: this.sonarParameters.track_offset,
    });
    
    this.sonarProcessor.port.onmessage = this.onWorkletMessage;
    this.micSource = await this.initAudioInput();
    this.micSource.connect(this.sonarProcessor);

    this.initialized = true;
  }

  async initAudioInput() {
    const stream = await navigator.mediaDevices.getUserMedia({
      audio: {
        autoGainControl: false,
        echoCancellation: false,
        noiseSuppression: false,
        channelCount: 2,
      },
    });
    const audioTrack = stream.getAudioTracks()[0];
    console.log("audio input settings:", audioTrack.getSettings());
    const micSource = this.audioContext.createMediaStreamSource(stream);
    return micSource;
  }

  initAudioOutput(chirp) {
    const myArrayBuffer = this.audioContext.createBuffer(
      2,
      chirp.length,
      this.audioContext.sampleRate,
    );
    myArrayBuffer.copyToChannel(chirp, 0);
    const chirpSource = this.audioContext.createBufferSource();
    chirpSource.buffer = myArrayBuffer;
    chirpSource.loop = true;
    chirpSource.connect(this.audioContext.destination);
    chirpSource.start();
    return chirpSource;
  }

  async initSonarWorklet(params) {
    // Load the WASM file from public folder
    const response = await fetch("/pkg/sonar_bg.wasm");
    const wasm_blob = await response.arrayBuffer();

    // Add the worklet module
    await this.audioContext.audioWorklet.addModule("/javascript/sonar-processor.js");
    
    const sonarProcessor = new AudioWorkletNode(
      this.audioContext, 
      "sonar-processor",
      {
        numberOfInputs: 1,
        numberOfOutputs: 0,
      }
    );

    sonarProcessor.port.postMessage({ wasm_blob, ...params });
    return sonarProcessor;
  }

  async start() {
    if (this.initialized) {
      await this.audioContext.resume();
    } else {
      await this.initialize();
    }
  }

  async stop() {
    if (this.initialized) {
      await this.audioContext.suspend();
    } else {
      console.error("stop() called before audio graph was initialized");
    }
  }
}
