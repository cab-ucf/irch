//! A call's voice, native. A web view's echo canceller never hears what
//! WebAudio plays, so with a speaker it suppresses instead, and one side is
//! heard at a time. Here, as in iroh-live, the canceller (WebRTC's AEC3, by
//! sonora) is fed the very mix played. Opus at 20 ms, in the frames the page
//! sends too, so a native peer and a web view's hear each other.

use crate::Id;
use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use n0_future::task::AbortOnDropHandle;
use sonora::{AudioProcessing, StreamConfig};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Instant;
use tokio::sync::broadcast;

pub const RATE: u32 = 48_000;
const TEN: usize = 480;
const PACKET: usize = 960;
/// Queued before a voice plays: two packets of jitter.
const START: usize = 2 * PACKET;
/// Past this, a voice's oldest samples go: 200 ms behind at most.
const MOST: usize = 10 * PACKET;

fn rms(s: &[f32]) -> f32 {
    (s.iter().map(|x| x * x).sum::<f32>() / s.len().max(1) as f32).sqrt()
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// One 20 ms Opus frame as the page sends it: audio, Opus, f64 µs, data.
pub fn frame(us: f64, opus: &[u8]) -> Vec<u8> {
    [&[0u8, 0][..], &us.to_le_bytes(), opus].concat()
}

/// Opus, as libopus does it, in Rust.
mod opus {
    use super::RATE;
    use anyhow::{Result, bail};
    use unsafe_libopus::*;

    pub struct Enc(*mut OpusEncoder);
    // Used from one thread at a time.
    unsafe impl Send for Enc {}

    impl Enc {
        pub fn new() -> Result<Self> {
            let mut err = 0;
            let e = unsafe {
                opus_encoder_create(
                    RATE as i32,
                    1,
                    OPUS_APPLICATION_VOIP,
                    &mut err,
                )
            };
            if err != 0 || e.is_null() {
                bail!("opus encoder: {err}");
            }
            // As the page encodes: 32 kb/s, in-band FEC, voice.
            unsafe {
                opus_encoder_ctl!(e, OPUS_SET_BITRATE_REQUEST, 32_000);
                opus_encoder_ctl!(e, OPUS_SET_INBAND_FEC_REQUEST, 1);
                opus_encoder_ctl!(e, OPUS_SET_PACKET_LOSS_PERC_REQUEST, 10);
                opus_encoder_ctl!(
                    e,
                    OPUS_SET_SIGNAL_REQUEST,
                    OPUS_SIGNAL_VOICE
                );
            }
            Ok(Self(e))
        }

        pub fn encode(&mut self, pcm: &[f32]) -> Option<Vec<u8>> {
            let mut out = [0u8; 1500];
            let n = unsafe {
                opus_encode_float(
                    self.0,
                    pcm.as_ptr(),
                    pcm.len() as i32,
                    out.as_mut_ptr(),
                    1500,
                )
            };
            (n > 0).then(|| out[..n as usize].to_vec())
        }
    }

    impl Drop for Enc {
        fn drop(&mut self) {
            unsafe { opus_encoder_destroy(self.0) }
        }
    }

    pub struct Dec(*mut OpusDecoder);
    unsafe impl Send for Dec {}

    impl Dec {
        pub fn new() -> Option<Self> {
            let mut err = 0;
            let d = unsafe { opus_decoder_create(RATE as i32, 1, &mut err) };
            (err == 0 && !d.is_null()).then_some(Self(d))
        }

        pub fn decode(&mut self, data: &[u8]) -> Option<Vec<f32>> {
            let mut pcm = vec![0f32; 5760];
            let n = unsafe {
                let len = data.len() as i32;
                opus_decode_float(
                    self.0,
                    data.as_ptr(),
                    len,
                    pcm.as_mut_ptr(),
                    5760,
                    0,
                )
            };
            (n > 0).then(|| {
                pcm.truncate(n as usize);
                pcm
            })
        }
    }

    impl Drop for Dec {
        fn drop(&mut self) {
            unsafe { opus_decoder_destroy(self.0) }
        }
    }
}
use opus::{Dec, Enc};

#[derive(Default)]
struct Queue {
    pcm: VecDeque<f32>,
    on: bool,
    level: f32,
    spoke: Option<Instant>,
}

/// What plays: each peer's voice, queued and mixed; and what played, for
/// the canceller to take away from the microphone.
#[derive(Default)]
pub struct Mix {
    peers: HashMap<Id, Queue>,
    played: VecDeque<f32>,
}

impl Mix {
    pub fn push(&mut self, who: Id, pcm: &[f32]) {
        let q = self.peers.entry(who).or_default();
        (q.level, q.spoke) = (rms(pcm), Some(Instant::now()));
        q.pcm.extend(pcm);
        if q.pcm.len() > MOST {
            let n = q.pcm.len() - START;
            q.pcm.drain(..n);
        }
    }

    /// The next samples of the mix, mono.
    pub fn play(&mut self, out: &mut [f32]) {
        for s in out.iter_mut() {
            let mut v = 0.0;
            for q in self.peers.values_mut() {
                q.on |= q.pcm.len() >= START;
                if q.on {
                    match q.pcm.pop_front() {
                        Some(x) => v += x,
                        None => q.on = false,
                    }
                }
            }
            *s = v.clamp(-1.0, 1.0);
        }
        self.played.extend(out.iter());
        if self.played.len() > MOST {
            let n = self.played.len() - MOST;
            self.played.drain(..n);
        }
    }

    /// Who spoke in the last 300 ms, and how loud.
    pub fn levels(&self) -> Vec<(Id, f32)> {
        let lately =
            |q: &Queue| q.spoke.is_some_and(|t| t.elapsed().as_millis() < 300);
        self.peers
            .iter()
            .filter(|(_, q)| lately(q))
            .map(|(i, q)| (*i, q.level))
            .collect()
    }
}

/// Each peer's packets, decoded into the mix.
#[derive(Default)]
pub struct Ears(HashMap<Id, Dec>);

impl Ears {
    pub fn hear(&mut self, mix: &Mutex<Mix>, who: Id, f: &[u8]) {
        if f.len() < 11 || f[0] != 0 || f[1] != 0 {
            return;
        }
        let Some(d) = self.0.get_mut(&who) else {
            if let Some(d) = Dec::new() {
                self.0.insert(who, d);
                self.hear(mix, who, f);
            }
            return;
        };
        if let Some(pcm) = d.decode(&f[10..]) {
            lock(mix).push(who, &pcm);
        }
    }
}

/// The microphone's way out: what played taken away, then Opus.
pub struct Mic {
    apm: AudioProcessing,
    enc: Enc,
    played: VecDeque<f32>,
    pending: Vec<f32>,
    clean: Vec<f32>,
    us: f64,
    pub level: f32,
}

impl Mic {
    pub fn new() -> Result<Self> {
        use sonora::config::{
            EchoCanceller, GainController2, NoiseSuppression,
        };
        let mono = StreamConfig::new(RATE, 1);
        let config = sonora::Config {
            echo_canceller: Some(EchoCanceller::default()),
            noise_suppression: Some(NoiseSuppression::default()),
            gain_controller2: Some(GainController2::default()),
            ..Default::default()
        };
        let apm = AudioProcessing::builder()
            .capture_config(mono)
            .render_config(mono)
            .config(config)
            .build();
        let (played, pending, clean) = (VecDeque::new(), vec![], vec![]);
        let enc = Enc::new()?;
        Ok(Self { apm, enc, played, pending, clean, us: 0.0, level: 0.0 })
    }

    /// Mono microphone samples, and what played meanwhile; out, packets
    /// (none while muted, though the canceller keeps learning).
    pub fn take(
        &mut self,
        mic: &[f32],
        played: impl IntoIterator<Item = f32>,
        send: bool,
        mut out: impl FnMut(Vec<u8>),
    ) {
        // What played first, 10 ms at a time: the echo model's order.
        self.played.extend(played);
        let (mut r, mut o) = ([0f32; TEN], [0f32; TEN]);
        while self.played.len() >= TEN {
            r.iter_mut()
                .zip(self.played.drain(..TEN))
                .for_each(|(a, b)| *a = b);
            let _ = self.apm.process_render_f32(&[&r], &mut [&mut o]);
        }
        self.pending.extend_from_slice(mic);
        while self.pending.len() >= TEN {
            let c: Vec<f32> = self.pending.drain(..TEN).collect();
            let mut d = [0f32; TEN];
            if self.apm.process_capture_f32(&[&c], &mut [&mut d]).is_err() {
                d.copy_from_slice(&c);
            }
            self.clean.extend_from_slice(&d);
        }
        while self.clean.len() >= PACKET {
            let p: Vec<f32> = self.clean.drain(..PACKET).collect();
            self.level = rms(&p);
            if send && let Some(b) = self.enc.encode(&p) {
                out(frame(self.us, &b));
            }
            self.us += 20_000.0;
        }
    }
}

/// The devices: the microphone and the speaker, on a thread of their own
/// (some systems' streams may not move between threads), until dropped.
pub struct Voice {
    stop: mpsc::Sender<()>,
    mix: Arc<Mutex<Mix>>,
    muted: Arc<AtomicBool>,
    level: Arc<AtomicU32>,
    _ears: AbortOnDropHandle<()>,
}

impl Voice {
    /// Packets out by `send`, and in from the call's `media`.
    pub fn start(
        send: impl Fn(Vec<u8>) + Send + 'static,
        mut media: broadcast::Receiver<(Id, Vec<u8>)>,
    ) -> Result<Self> {
        let mix = Arc::new(Mutex::new(Mix::default()));
        let muted = Arc::new(AtomicBool::new(false));
        let level = Arc::new(AtomicU32::new(0));
        let (stop, wait) = mpsc::channel::<()>();
        let (ready, started) = mpsc::channel();
        let (m, mu, l) = (mix.clone(), muted.clone(), level.clone());
        std::thread::spawn(move || match open(m, mu, l, send) {
            Ok(streams) => {
                let _ = ready.send(Ok(()));
                let _ = wait.recv();
                drop(streams);
            }
            Err(e) => drop(ready.send(Err(e))),
        });
        started.recv().context("audio thread")??;
        let m = mix.clone();
        let ears = tokio::spawn(async move {
            let mut ears = Ears::default();
            loop {
                match media.recv().await {
                    Ok((who, f)) => ears.hear(&m, who, &f),
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => return,
                }
            }
        });
        let _ears = AbortOnDropHandle::new(ears);
        Ok(Self { stop, mix, muted, level, _ears })
    }

    pub fn mute(&self, on: bool) {
        self.muted.store(on, Relaxed);
    }

    /// Our own level, and each peer's who spoke lately.
    pub fn levels(&self) -> (f32, Vec<(Id, f32)>) {
        let me = f32::from_bits(self.level.load(Relaxed));
        (me, lock(&self.mix).levels())
    }
}

impl Drop for Voice {
    fn drop(&mut self) {
        let _ = self.stop.send(());
    }
}

/// 48 kHz in f32, where a device offers it.
fn config(
    configs: impl Iterator<Item = cpal::SupportedStreamConfigRange>,
    want: u16,
) -> Option<cpal::StreamConfig> {
    let ok = |c: &cpal::SupportedStreamConfigRange| {
        c.sample_format() == cpal::SampleFormat::F32
            && (c.min_sample_rate()..=c.max_sample_rate()).contains(&RATE)
    };
    let mut all: Vec<_> = configs.filter(ok).collect();
    all.sort_by_key(|c| c.channels() != want);
    let c = all.first()?;
    let channels = c.channels();
    Some(cpal::StreamConfig {
        channels,
        sample_rate: RATE,
        buffer_size: cpal::BufferSize::Default,
    })
}

fn open(
    mix: Arc<Mutex<Mix>>,
    muted: Arc<AtomicBool>,
    level: Arc<AtomicU32>,
    send: impl Fn(Vec<u8>) + Send + 'static,
) -> Result<(cpal::Stream, cpal::Stream)> {
    let host = cpal::default_host();
    let speaker = host.default_output_device().context("no speaker")?;
    let micro = host.default_input_device().context("no microphone")?;
    let oc = config(speaker.supported_output_configs()?, 2)
        .context("speaker: no 48 kHz")?;
    let ic = config(micro.supported_input_configs()?, 1)
        .context("microphone: no 48 kHz")?;
    let (och, ich) = (oc.channels as usize, ic.channels as usize);
    let (mx, warn) = (mix.clone(), |e| tracing::warn!("audio: {e}"));
    let mut buf = vec![];
    let out = speaker.build_output_stream::<f32, _, _>(
        oc,
        move |data, _| {
            buf.resize(data.len() / och, 0.0);
            lock(&mx).play(&mut buf);
            for (frame, s) in data.chunks_mut(och).zip(&buf) {
                frame.fill(*s);
            }
        },
        warn,
        None,
    )?;
    // Packets leave on a thread of their own: never a lock in the callback
    // but the mix's.
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || rx.iter().for_each(send));
    let mut mic = Mic::new()?;
    let mut mono = vec![];
    let inp = micro.build_input_stream::<f32, _, _>(
        ic,
        move |data, _| {
            mono.clear();
            mono.extend(
                data.chunks(ich).map(|f| f.iter().sum::<f32>() / ich as f32),
            );
            let played: Vec<f32> = match mix.try_lock() {
                Ok(mut m) => m.played.drain(..).collect(),
                Err(_) => vec![],
            };
            let on = !muted.load(Relaxed);
            mic.take(&mono, played, on, |p| drop(tx.send(p)));
            level.store(if on { mic.level } else { 0.0 }.to_bits(), Relaxed);
        },
        warn,
        None,
    )?;
    out.play()?;
    inp.play()?;
    Ok((out, inp))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Speech-like: a pitch, its harmonics, and syllables four a second.
    fn voice(f: f32, n: usize, from: usize) -> Vec<f32> {
        let w = 2.0 * std::f32::consts::PI / RATE as f32;
        let at = |t: f32| {
            let syl = (w * 4.0 * t).sin().max(0.0);
            let h = (1..6).map(|k| (w * f * k as f32 * t).sin() / k as f32);
            0.2 * syl * h.sum::<f32>()
        };
        (from..from + n).map(|i| at(i as f32)).collect()
    }

    /// The far voice, played and heard back by the microphone, is taken
    /// out; ours, spoken over it, goes through: both at once.
    #[test]
    fn both_speak_at_once_and_the_echo_goes() {
        let mut mic = Mic::new().unwrap();
        let mut dec = Dec::new().unwrap();
        let (mut sent, mut echo_only) = (vec![], vec![]);
        let delay = 10 * 48; // 10 ms from speaker to microphone
        let mut played: VecDeque<f32> = VecDeque::from(vec![0.0; delay]);
        for i in 0..400 {
            // 8 s of 20 ms: far voice 300 Hz always; ours 1 kHz after 4 s.
            let far = voice(140.0, PACKET, i * PACKET);
            played.extend(&far);
            let heard: Vec<f32> =
                played.drain(..PACKET).map(|x| 0.5 * x).collect();
            let near = if i >= 200 {
                voice(220.0, PACKET, i * PACKET)
            } else {
                vec![0.0; PACKET]
            };
            let input: Vec<f32> =
                heard.iter().zip(&near).map(|(a, b)| a + b).collect();
            let mut out = vec![];
            mic.take(&input, far, true, |p| out.push(p));
            for p in out {
                let pcm = dec.decode(&p[10..]).unwrap();
                if (150..200).contains(&i) {
                    echo_only.extend(pcm);
                } else if i >= 350 {
                    sent.extend(pcm);
                }
            }
        }
        // Echo alone, converged: well under the 0.5 * 0.3 it came in at.
        assert!(rms(&echo_only) < 0.005, "echo left: {}", rms(&echo_only));
        // Ours over it still goes out.
        assert!(rms(&sent) > 0.05, "our voice: {}", rms(&sent));
    }

    #[test]
    fn voices_mix_after_a_little_jitter() {
        let mut mix = Mix::default();
        let (a, b) = (Id([1; 32]), Id([2; 32]));
        mix.push(a, &vec![0.25; PACKET]);
        let mut out = vec![0.0; 10];
        mix.play(&mut out);
        assert_eq!(out[0], 0.0, "one packet: still waiting");
        mix.push(a, &vec![0.25; PACKET]);
        mix.push(b, &vec![0.5; START]);
        mix.play(&mut out);
        assert_eq!(out[0], 0.75, "both, together");
        assert_eq!(mix.levels().len(), 2);
        mix.push(a, &vec![0.1; 20 * PACKET]);
        assert!(mix.peers[&a].pcm.len() <= MOST, "never far behind");
    }
}
