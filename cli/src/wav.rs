//! `--audio-out FILE.wav`: what the guest plays on the I²S DAC, as a 16-bit stereo WAV file; and
//! `--audio-in FILE.wav`, read as mono for a microphone.
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};

pub struct Wav { f: File, rate: u32, bytes: u32 }

impl Wav {
    pub fn create(path: &str) -> std::io::Result<Wav> {
        let mut w = Wav { f: File::create(path)?, rate: 0, bytes: 0 };
        w.header()?;
        Ok(w)
    }

    fn header(&mut self) -> std::io::Result<()> {
        let rate = self.rate.max(1);
        let mut h = Vec::with_capacity(44);
        h.extend_from_slice(b"RIFF");
        h.extend_from_slice(&(36 + self.bytes).to_le_bytes());
        h.extend_from_slice(b"WAVEfmt ");
        h.extend_from_slice(&16u32.to_le_bytes());
        h.extend_from_slice(&1u16.to_le_bytes()); // PCM
        h.extend_from_slice(&2u16.to_le_bytes()); // stereo
        h.extend_from_slice(&rate.to_le_bytes());
        h.extend_from_slice(&(rate * 4).to_le_bytes());
        h.extend_from_slice(&4u16.to_le_bytes());
        h.extend_from_slice(&16u16.to_le_bytes());
        h.extend_from_slice(b"data");
        h.extend_from_slice(&self.bytes.to_le_bytes());
        self.f.seek(SeekFrom::Start(0))?;
        self.f.write_all(&h)?;
        self.f.seek(SeekFrom::End(0))?;
        Ok(())
    }

    /// Append interleaved stereo samples played at `rate` Hz (the first rate seen is the file's).
    pub fn push(&mut self, rate: u32, s: &[i16]) {
        if s.is_empty() { return; }
        if self.rate == 0 { self.rate = rate; }
        let b: Vec<u8> = s.iter().flat_map(|v| v.to_le_bytes()).collect();
        if self.f.write_all(&b).is_ok() { self.bytes = self.bytes.saturating_add(b.len() as u32); }
    }

    pub fn finish(&mut self) { let _ = self.header(); let _ = self.f.flush(); }
}

impl Drop for Wav { fn drop(&mut self) { self.finish(); } }

/// A 16-bit PCM WAV file as its rate and mono samples (the channels averaged).
pub fn read_mono(path: &str) -> Result<(u32, Vec<i16>), String> {
    let b = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    parse_mono(&b).ok_or_else(|| format!("{path}: not a 16-bit PCM WAV file"))
}

fn parse_mono(b: &[u8]) -> Option<(u32, Vec<i16>)> {
    if b.len() < 12 || &b[0..4] != b"RIFF" || &b[8..12] != b"WAVE" { return None; }
    let (mut at, mut fmt) = (12, None);
    while at + 8 <= b.len() {
        let len = u32::from_le_bytes(b[at + 4..at + 8].try_into().ok()?) as usize;
        let body = &b[at + 8..(at + 8 + len).min(b.len())];
        match &b[at..at + 4] {
            b"fmt " if body.len() >= 16 => {
                let tag = u16::from_le_bytes([body[0], body[1]]);
                let ch = u16::from_le_bytes([body[2], body[3]]) as usize;
                let rate = u32::from_le_bytes(body[4..8].try_into().ok()?);
                let bits = u16::from_le_bytes([body[14], body[15]]);
                // PCM, or WAVE_FORMAT_EXTENSIBLE holding PCM
                if !(tag == 1 || tag == 0xfffe) || bits != 16 || ch == 0 { return None; }
                fmt = Some((rate, ch));
            }
            b"data" => {
                let (rate, ch) = fmt?;
                let s = body.chunks_exact(2 * ch)
                    .map(|f| (f.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as i32).sum::<i32>() / ch as i32) as i16)
                    .collect();
                return Some((rate, s));
            }
            _ => {}
        }
        at += 8 + len + (len & 1);
    }
    None
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_stereo_file_reads_as_mono() {
        let mut b = b"RIFF\0\0\0\0WAVEfmt ".to_vec();
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&[1, 0, 2, 0]);
        b.extend_from_slice(&22_050u32.to_le_bytes());
        b.extend_from_slice(&[0; 6]);
        b.extend_from_slice(&[16, 0]);
        b.extend_from_slice(b"LIST\x02\0\0\0ab");
        b.extend_from_slice(b"data\x08\0\0\0");
        for v in [100i16, 300, -2, -4] { b.extend_from_slice(&v.to_le_bytes()); }
        assert_eq!(super::parse_mono(&b), Some((22_050, vec![200, -3])));
        assert_eq!(super::parse_mono(b"RIFF....WAVE"), None);
    }
}
