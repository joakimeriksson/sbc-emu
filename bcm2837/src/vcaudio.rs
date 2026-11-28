//! The VideoCore's audio service (`AUDS` over VCHIQ), what `snd-bcm2835` plays through: the
//! "bcm2835 Headphones" card (the 3.5 mm jack, PWM-driven by the firmware). Each open PCM
//! is a connection: `CONFIG` (channels, rate, bits), `CONTROL` (volume, destination),
//! `START`/`STOP`, and `WRITE`s whose sample data follows in `DATA` messages (service version 2
//! and later, as reported here) or a bulk transfer. The firmware plays the data at the
//! configured rate and reports each write back with `COMPLETE` once played; that is what
//! moves ALSA's pointer, so the model paces it by guest time. Played samples, 16-bit stereo,
//! collect in `out` for `--audio-out`; the jack's PWM itself is not modelled.
use crate::vchiq::Outbox;
use std::collections::VecDeque;

const RESULT: u32 = 0;
const COMPLETE: u32 = 1;
const CONFIG: u32 = 2;
const CONTROL: u32 = 3;
const OPEN: u32 = 4;
const CLOSE: u32 = 5;
const START: u32 = 6;
const STOP: u32 = 7;
const WRITE: u32 = 8;
/// `sizeof(struct vc_audio_msg)`: the type and the largest member (`vc_audio_write`)
const MSG_SIZE: usize = 20;

fn get32(d: &[u8], o: usize) -> u32 { d.get(o..o + 4).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap())) }

fn msg(t: u32, w: &[u32]) -> Vec<u8> {
    let mut v: Vec<u8> = std::iter::once(t).chain(w.iter().copied()).flat_map(|x| x.to_le_bytes()).collect();
    v.resize(MSG_SIZE, 0);
    v
}

#[derive(Clone, Debug, Default)]
struct Stream {
    /// our VCHIQ port for this PCM
    port: u32,
    channels: u32,
    rate: u32,
    bits: u32,
    /// attenuation in 1/256 dB (`alsa2chip`); 26214 and more: muted
    volume: i32,
    playing: bool,
    draining: bool,
    /// bytes of the current WRITE still to come in DATA messages
    expect: u32,
    /// sample frames waiting to play, as 16-bit stereo
    fifo: VecDeque<[i16; 2]>,
    /// the writes behind them: (bytes, frames not yet played, cookies)
    writes: VecDeque<(u32, u64, u32, u32)>,
    /// a write whose data is still arriving
    current: Option<(u32, u64, u32, u32)>,
    /// guest time played up to (ns), and the fraction of a frame carried over
    clock: u64,
    frac: u64,
}

impl Stream {
    fn frame_bytes(&self) -> u32 { (self.channels.max(1) * (self.bits.max(8) / 8)).max(1) }

    /// Convert sample data to stereo frames.
    fn take(&mut self, d: &[u8]) {
        let fb = self.frame_bytes() as usize;
        let bps = (self.bits.max(8) / 8) as usize;
        let gain = if self.volume >= 26214 { 0.0 } else { 10f64.powf(-(self.volume as f64) / 256.0 / 20.0) };
        let sample = |c: &[u8]| -> f64 {
            match bps {
                1 => ((c[0] as i32 - 128) << 8) as f64,
                2 => i16::from_le_bytes([c[0], c[1]]) as f64,
                3 => (i32::from_le_bytes([0, c[0], c[1], c[2]]) >> 16) as f64,
                _ => (i32::from_le_bytes([c[0], c[1], c[2], c[3]]) >> 16) as f64,
            }
        };
        for f in d.chunks_exact(fb) {
            let l = sample(&f[..bps]);
            let r = if self.channels >= 2 { sample(&f[bps..2 * bps]) } else { l };
            let s = |x: f64| (x * gain).round().clamp(-32768.0, 32767.0) as i16;
            self.fifo.push_back([s(l), s(r)]);
        }
        let frames = (d.len() / fb) as u64;
        if let Some(w) = self.current.as_mut() { w.1 += frames; }
    }

    /// The current write has all its data: it plays now.
    fn finish_write(&mut self) {
        if let Some(w) = self.current.take() { self.writes.push_back(w); }
    }
}

#[derive(Default)]
pub struct Audio {
    streams: Vec<Stream>,
    /// what played, 16-bit stereo at `rate`, until the machine takes it
    pub out: Vec<i16>,
    pub rate: u32,
    /// writes completed (diagnostics)
    pub completed: u64,
}

impl Audio {
    pub fn new() -> Self { Self::default() }

    pub fn closed(&mut self, port: u32) { self.streams.retain(|s| s.port != port); }

    fn stream(&mut self, port: u32) -> &mut Stream {
        if let Some(k) = self.streams.iter().position(|s| s.port == port) { return &mut self.streams[k]; }
        self.streams.push(Stream { port, channels: 2, rate: 48_000, bits: 16, ..Default::default() });
        self.streams.last_mut().unwrap()
    }

    pub fn message(&mut self, port: u32, p: &[u8], now: u64, out: &mut Outbox) {
        let s = self.stream(port);
        if s.expect > 0 {
            // sample data of the current WRITE
            let n = (p.len() as u32).min(s.expect) as usize;
            s.take(&p[..n]);
            s.expect -= n as u32;
            if s.expect == 0 { s.finish_write(); }
            return;
        }
        let w = |k: usize| get32(p, 4 * k);
        match w(0) {
            CONFIG => {
                (s.channels, s.rate, s.bits) = (w(1).clamp(1, 8), w(2).clamp(8_000, 192_000), w(3));
                let ok = matches!(s.bits, 8 | 16 | 24 | 32);
                out.data(port, msg(RESULT, &[if ok { 0 } else { u32::MAX }]));
            }
            CONTROL => { s.volume = w(1) as i32; out.data(port, msg(RESULT, &[0])); }
            OPEN => {}
            CLOSE => { self.closed(port); out.data(port, msg(RESULT, &[0])); }
            START => { s.playing = true; s.draining = false; s.clock = now; s.frac = 0; }
            STOP => {
                if w(1) != 0 && !s.fifo.is_empty() { s.draining = true; } else {
                    s.playing = false;
                    s.fifo.clear();
                    s.writes.clear();
                    s.current = None;
                }
            }
            WRITE => {
                let (count, c1, c2, max_packet) = (w(1), w(2), w(3), w(4) >> 16);
                s.current = Some((count, 0, c1, c2));
                // with max_packet 0 the data comes as a bulk transfer (`bulk`)
                s.expect = if max_packet != 0 { count } else { 0 };
                if max_packet != 0 && count == 0 { s.finish_write(); }
            }
            _ => {}
        }
    }

    /// The data of a WRITE sent as a bulk transfer.
    pub fn bulk(&mut self, port: u32, d: &[u8], _now: u64, _out: &mut Outbox) {
        let s = self.stream(port);
        s.take(d);
        s.finish_write();
    }

    /// Play what is due by `now` and report the writes that have played.
    pub fn tick(&mut self, now: u64, out: &mut Outbox) {
        for s in self.streams.iter_mut().filter(|s| s.playing) {
            if s.fifo.is_empty() {
                // nothing to play: the clock waits for data
                s.clock = now;
                s.frac = 0;
                if s.draining { s.playing = false; s.draining = false; }
                continue;
            }
            if now <= s.clock { continue; }
            let t = (now - s.clock) as u128 * s.rate as u128 + s.frac as u128;
            let due = (t / 1_000_000_000) as u64;
            s.frac = (t % 1_000_000_000) as u64;
            s.clock = now;
            let n = (due as usize).min(s.fifo.len());
            if self.rate != s.rate && !self.out.is_empty() { self.out.clear(); }
            self.rate = s.rate;
            for f in s.fifo.drain(..n) { self.out.extend_from_slice(&f); }
            let mut left = n as u64;
            while left > 0 {
                let Some(w) = s.writes.front_mut() else { break };
                let k = w.1.min(left);
                w.1 -= k;
                left -= k;
                if w.1 == 0 {
                    let (bytes, _, c1, c2) = s.writes.pop_front().unwrap();
                    out.data(s.port, msg(COMPLETE, &[bytes, c1, c2]));
                    self.completed += 1;
                }
            }
            // a write of no whole frames is done at once
            while s.writes.front().is_some_and(|w| w.1 == 0) {
                let (bytes, _, c1, c2) = s.writes.pop_front().unwrap();
                out.data(s.port, msg(COMPLETE, &[bytes, c1, c2]));
                self.completed += 1;
            }
        }
        // keep the played samples bounded if nobody collects them
        if self.out.len() > 2 * 192_000 * 10 { self.out.drain(..self.out.len() / 2 & !1); }
    }

    /// The next time `tick` has something to play (1 ms steps while a stream has data).
    pub fn next_event(&self) -> Option<u64> {
        self.streams.iter().filter(|s| s.playing && !s.fifo.is_empty()).map(|s| s.clock + 1_000_000).min()
    }

    pub fn save_state(&self, w: &mut emu_core::snap::W) {
        w.tag("vcaudio-v1");
        w.u32(self.rate);
        w.u32(self.streams.len() as u32);
        for s in &self.streams {
            for v in [s.port, s.channels, s.rate, s.bits, s.volume as u32, s.expect] { w.u32(v); }
            w.bool(s.playing); w.bool(s.draining); w.u64(s.clock); w.u64(s.frac);
            w.u32(s.fifo.len() as u32);
            for f in &s.fifo { w.u16(f[0] as u16); w.u16(f[1] as u16); }
            let save = |w: &mut emu_core::snap::W, x: &(u32, u64, u32, u32)| { w.u32(x.0); w.u64(x.1); w.u32(x.2); w.u32(x.3); };
            w.u32(s.writes.len() as u32);
            for x in &s.writes { save(w, x); }
            w.bool(s.current.is_some());
            save(w, &s.current.unwrap_or_default());
        }
    }

    pub fn load_state(&mut self, r: &mut emu_core::snap::R) -> Result<(), emu_core::snap::SnapError> {
        r.tag("vcaudio-v1")?;
        self.rate = r.u32()?;
        self.streams.clear();
        for _ in 0..r.u32()? {
            let mut s = Stream { port: r.u32()?, channels: r.u32()?, rate: r.u32()?, bits: r.u32()?, volume: r.u32()? as i32, expect: r.u32()?, ..Default::default() };
            s.playing = r.bool()?; s.draining = r.bool()?; s.clock = r.u64()?; s.frac = r.u64()?;
            for _ in 0..r.u32()? { let l = r.u16()? as i16; s.fifo.push_back([l, r.u16()? as i16]); }
            let load = |r: &mut emu_core::snap::R| -> Result<(u32, u64, u32, u32), emu_core::snap::SnapError> { Ok((r.u32()?, r.u64()?, r.u32()?, r.u32()?)) };
            for _ in 0..r.u32()? { s.writes.push_back(load(r)?); }
            let has = r.bool()?;
            let c = load(r)?;
            s.current = has.then_some(c);
            self.streams.push(s);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vchiq::Out;

    fn send(a: &mut Audio, p: &[u8], now: u64) -> Vec<Vec<u8>> {
        let mut out = Outbox::default();
        a.message(3, p, now, &mut out);
        out.0.into_iter().map(|(_, o)| match o { Out::Data(d) => d, Out::Bulk(_) => panic!() }).collect()
    }

    #[test]
    fn plays_a_tone_at_the_configured_rate() {
        let mut a = Audio::new();
        send(&mut a, &msg(OPEN, &[]), 0);
        assert_eq!(send(&mut a, &msg(CONTROL, &[0, 1]), 0), vec![msg(RESULT, &[0])]);
        assert_eq!(send(&mut a, &msg(CONFIG, &[2, 48_000, 16]), 0), vec![msg(RESULT, &[0])]);
        // 4800 frames (0.1 s) of a 440 Hz tone in two writes, as 4000-byte messages
        let tone: Vec<u8> = (0..4800).flat_map(|k| { let v = ((k as f64 * 440.0 / 48_000.0 * std::f64::consts::TAU).sin() * 16_000.0) as i16; [v.to_le_bytes(), v.to_le_bytes()].concat() }).collect();
        for half in tone.chunks(tone.len() / 2) {
            send(&mut a, &msg(WRITE, &[half.len() as u32, 0x42434d41, 0x44415441, 4000 << 16]), 0);
            for c in half.chunks(4000) { assert!(send(&mut a, c, 0).is_empty()); }
        }
        send(&mut a, &msg(START, &[]), 0);
        let mut out = Outbox::default();
        a.tick(25_000_000, &mut out);
        assert!(out.0.is_empty(), "a quarter of the data has played: no write is complete");
        a.tick(50_000_000, &mut out);
        assert_eq!(out.0.len(), 1, "the first write has played");
        let Out::Data(d) = &out.0[0].1 else { panic!() };
        assert_eq!((get32(d, 0), get32(d, 4), get32(d, 8)), (COMPLETE, 9600, 0x42434d41));
        a.tick(200_000_000, &mut out);
        assert_eq!(out.0.len(), 2);
        assert_eq!(a.out.len(), 9600);
        assert_eq!(a.rate, 48_000);
        // the samples are the tone
        assert_eq!(&a.out[200..204], &[i16::from_le_bytes([tone[400], tone[401]]), i16::from_le_bytes([tone[402], tone[403]]), i16::from_le_bytes([tone[404], tone[405]]), i16::from_le_bytes([tone[406], tone[407]])]);
        assert!(a.next_event().is_none());
    }
}
