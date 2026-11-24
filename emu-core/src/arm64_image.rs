//! Decoding the Linux arm64 boot Image and its compressed forms.

/// An arm64 Image: as given, or unpacked from an EFI zboot image ("zimg" at offset 4, the
/// payload's offset and size at 8 and 12, its compression at 24).
pub fn kernel_image(kernel: &[u8]) -> Result<Vec<u8>, String> {
    if kernel.len() >= 64 && &kernel[56..60] == b"ARMd" {
        return Ok(kernel.to_vec());
    }
    if kernel.len() >= 32 && &kernel[4..8] == b"zimg" {
        let u32_at = |o: usize| u32::from_le_bytes(kernel[o..o + 4].try_into().unwrap()) as usize;
        let (offset, size) = (u32_at(8), u32_at(12));
        let payload = kernel.get(offset..offset.checked_add(size).ok_or("zboot: bad payload")?).ok_or("zboot: truncated image")?;
        let kind = &kernel[24..32];
        if !kind.starts_with(b"gzip") {
            return Err(format!("zboot: {} compression is not supported (only gzip)", String::from_utf8_lossy(kind).trim_end_matches('\0')));
        }
        let image = gunzip(payload)?;
        if image.len() < 64 || &image[56..60] != b"ARMd" {
            return Err("zboot: the payload is not an arm64 Image".into());
        }
        return Ok(image);
    }
    if kernel.starts_with(&[0x1f, 0x8b]) {
        return kernel_image(&gunzip(kernel)?);
    }
    Err("expected an arm64 Image, a gzip'd Image or an EFI zboot vmlinuz".into())
}

fn gunzip(data: &[u8]) -> Result<Vec<u8>, String> {
    if data.len() < 18 || data[0] != 0x1f || data[1] != 0x8b || data[2] != 8 {
        return Err("not gzip".into());
    }
    let flags = data[3];
    let bad = || "gzip: truncated or bad header".to_string();
    let mut p = 10;
    if flags & 4 != 0 {
        let xlen = data.get(p..p + 2).ok_or_else(bad)?;
        p += 2 + u16::from_le_bytes([xlen[0], xlen[1]]) as usize;
    }
    for bit in [8, 16] {
        if flags & bit != 0 {
            p += data.get(p..).ok_or_else(bad)?.iter().position(|&b| b == 0).ok_or_else(bad)? + 1;
        }
    }
    if flags & 2 != 0 {
        p += 2;
    }
    let (body, trailer) = data.split_at(data.len() - 8);
    let out = miniz_oxide::inflate::decompress_to_vec(body.get(p..).ok_or_else(bad)?).map_err(|e| format!("gunzip: {e:?}"))?;
    if u32::from_le_bytes(trailer[4..].try_into().unwrap()) != out.len() as u32 {
        return Err("gzip: the unpacked size does not match (truncated file?)".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broken_gzip_is_an_error() {
        let mut d = vec![0u8; 18];
        d[..4].copy_from_slice(&[0x1f, 0x8b, 8, 4]);
        d[10..12].copy_from_slice(&[0xff, 0xff]); // an extra field longer than the file
        assert!(kernel_image(&d).is_err());
        d[3] = 8; // a name that never ends
        d[10..].fill(b'x');
        assert!(kernel_image(&d).is_err());
        assert!(kernel_image(&d[..12]).is_err());
        // a whole stream whose length trailer is wrong
        let mut z = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0];
        z.extend(miniz_oxide::deflate::compress_to_vec(&[7; 100], 6));
        z.extend([0, 0, 0, 0, 99, 0, 0, 0]);
        assert!(gunzip(&z).unwrap_err().contains("size"));
        let n = z.len();
        z[n - 4] = 100;
        assert_eq!(gunzip(&z).unwrap(), vec![7; 100]);
    }
}
