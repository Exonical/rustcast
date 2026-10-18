// flux-audio-sink: stereo ring buffer feeding the DAC at 48 kHz.
// Priming gate: output starts only once ~40 ms is buffered; overflow drops
// oldest down to the 60 ms target; underruns emit silence and re-prime.
// Plain JS — loaded via audioWorklet.addModule, no bundler.
const RATE = 48000;
const CAPACITY = RATE; // 1 s per channel
const PRIME = Math.floor(RATE * 0.04);
const TARGET = Math.floor(RATE * 0.06);
const HIGH_WATER = Math.floor(RATE * 0.2);

class FluxAudioSink extends AudioWorkletProcessor {
  constructor() {
    super();
    this.left = new Float32Array(CAPACITY);
    this.right = new Float32Array(CAPACITY);
    this.read = 0;
    this.write = 0;
    this.priming = true;
    this.muted = false;
    this.underruns = 0;
    this.dropped = 0;
    this.quanta = 0;
    this.port.onmessage = (event) => {
      const message = event.data;
      if (message.type === "push") {
        this.push(message.planes[0], message.planes[1] ?? message.planes[0]);
      } else if (message.type === "reset") {
        this.read = this.write = 0;
        this.priming = true;
      } else if (message.type === "mute") {
        this.muted = !!message.muted;
      }
    };
  }

  buffered() {
    return (this.write - this.read + CAPACITY) % CAPACITY;
  }

  push(left, right) {
    const n = Math.min(left.length, right.length);
    for (let i = 0; i < n; i++) {
      this.left[this.write] = left[i];
      this.right[this.write] = right[i];
      this.write = (this.write + 1) % CAPACITY;
      if (this.write === this.read) this.read = (this.read + 1) % CAPACITY; // overrun: drop oldest
    }
    const buffered = this.buffered();
    if (buffered > HIGH_WATER) {
      const drop = buffered - TARGET;
      this.read = (this.read + drop) % CAPACITY;
      this.dropped += drop;
    }
  }

  process(_inputs, outputs) {
    const out = outputs[0];
    const frames = out[0] ? out[0].length : 128;
    if (this.priming) {
      if (this.buffered() < PRIME) {
        for (const channel of out) channel.fill(0);
      } else {
        this.priming = false;
      }
    }
    if (!this.priming) {
      for (let i = 0; i < frames; i++) {
        if (this.read === this.write) {
          this.underruns++;
          this.priming = true;
          for (const channel of out) channel.fill(0, i);
          break;
        }
        const l = this.left[this.read];
        const r = this.right[this.read];
        this.read = (this.read + 1) % CAPACITY;
        if (!this.muted) {
          out[0][i] = l;
          if (out.length > 1) out[1][i] = r;
        }
      }
    }
    if (++this.quanta >= Math.round(500 / (frames / RATE * 1000))) {
      this.quanta = 0;
      this.port.postMessage({
        type: "stats",
        buffered_ms: (this.buffered() / RATE) * 1000,
        underruns: this.underruns,
        dropped: this.dropped,
      });
    }
    return true;
  }
}

registerProcessor("flux-audio-sink", FluxAudioSink);
