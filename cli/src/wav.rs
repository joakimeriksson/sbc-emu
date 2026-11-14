//! `--audio-out FILE.wav`: what the guest plays on the I²S DAC, as a 16-bit stereo WAV file.
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
