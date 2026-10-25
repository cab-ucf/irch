// The microphone, out in 20 ms frames; each speaker, in: played after a
// frame of slack, and never more than 200 ms behind (the oldest go).

registerProcessor("mic", class extends AudioWorkletProcessor {
  buf = new Float32Array(960);
  n = 0;
  process([input]) {
    for (const x of input[0] ?? []) {
      this.buf[this.n++] = x;
      if (this.n === 960) {
        this.port.postMessage(this.buf, [this.buf.buffer]);
        this.buf = new Float32Array(960);
        this.n = 0;
      }
    }
    return true;
  }
});

registerProcessor("play", class extends AudioWorkletProcessor {
  q = [];
  i = 0;
  on = false;
  constructor() {
    super();
    this.port.onmessage = ({ data }) => {
      this.q.push(data);
      if (this.q.length > 10) this.q.splice(0, this.q.length - 2);
    };
  }
  process(_, [[out]]) {
    this.on ||= this.q.length > 1;
    for (let k = 0; k < out.length && this.on; k++) {
      out[k] = this.q[0][this.i++];
      if (this.i < this.q[0].length) continue;
      this.q.shift();
      this.i = 0;
      this.on = this.q.length > 0;
    }
    return true;
  }
});
