//! `.wlabank` IFF FORM `WLAB` writer and reader.
//!
//! Layout (decoded from `ANDD02.wlabank`):
//! ```
//! FORM <total_size> "WLAB"
//!   HEAD <4>       — u32 BE version = 1
//!   BIDX <N>       — per clip: 5×u32 BE (payload_offset, payload_size,
//!                    channels, sample_rate, frame_count) + ASCII clip name
//!                    (no null terminator; length = chunk_size − 20)
//!   …              — one BIDX per clip, in order
//!   BBIN <M>       — raw Ogg Vorbis payload for the clip
//!   …
//! ```
//! IFF even-byte padding between chunks. `payload_offset` in BIDX is the
//! *file* byte offset of the matching BBIN's payload (not its chunk header).

use byteorder::{BigEndian, WriteBytesExt};
use std::path::Path;

/// One clip to pack into the bank.
pub struct BankClip {
    /// Clip name as it appears in `.wlamodel` `clipName` (e.g.
    /// `"module_ANDD02.wlabank/i1"`).
    pub clip_name: String,
    pub channels: u32,
    pub sample_rate: u32,
    pub frame_count: u64,
    /// Raw Ogg Vorbis bytes (the BBIN payload).
    pub ogg: Vec<u8>,
}

/// Write a WLAB bank from a list of clips. Clips are emitted in order:
/// all BIDX entries first, then all BBIN payloads.
pub fn write(clips: &[BankClip], dest: &Path) -> Result<(), String> {
    let bytes = encode(clips);
    std::fs::write(dest, bytes).map_err(|e| format!("write {}: {e}", dest.display()))
}

pub fn encode(clips: &[BankClip]) -> Vec<u8> {
    // Pass 1: compute the sizes.
    let head_chunk_size: u32 = 4; // version u32
    let bidx_sizes: Vec<u32> = clips
        .iter()
        .map(|c| 20 + c.clip_name.len() as u32)
        .collect();

    // Compute where BBIN payloads will land in the file. Each BBIN chunk
    // header is 8 bytes; payload starts right after.
    // File layout: 12 (FORM header) + 8+4 (HEAD) [+ 1 pad if odd] + sum(8+bidx_size [+pad]) + sum(8+bbin_size [+pad])
    let padded_size = |sz: usize| sz + (sz & 1); // round up to even
    let head_total = 8 + padded_size(head_chunk_size as usize);
    let bidx_total: usize = bidx_sizes
        .iter()
        .map(|&s| 8 + padded_size(s as usize))
        .sum();

    // BBIN payload offsets (file-absolute, pointing at the payload byte, not the chunk header).
    let bbin_base = 12 + head_total + bidx_total;
    let mut payload_offsets: Vec<u32> = Vec::with_capacity(clips.len());
    let mut cursor = bbin_base;
    for c in clips {
        let header = 8; // chunk id (4) + size (4)
        payload_offsets.push(cursor as u32 + header as u32);
        cursor += header + padded_size(c.ogg.len());
    }

    // Pass 2: emit.
    let mut buf: Vec<u8> = Vec::new();
    let w = &mut buf;

    // FORM header — size filled in at the end.
    w.extend_from_slice(b"FORM");
    w.extend_from_slice(&[0u8; 4]); // placeholder
    w.extend_from_slice(b"WLAB");

    // HEAD chunk.
    write_chunk(w, b"HEAD", |b| b.write_u32::<BigEndian>(1).unwrap());

    // BIDX chunks.
    for (i, c) in clips.iter().enumerate() {
        write_chunk(w, b"BIDX", |b| {
            b.write_u32::<BigEndian>(payload_offsets[i]).unwrap();
            b.write_u32::<BigEndian>(c.ogg.len() as u32).unwrap();
            b.write_u32::<BigEndian>(c.channels).unwrap();
            b.write_u32::<BigEndian>(c.sample_rate).unwrap();
            b.write_u32::<BigEndian>(c.frame_count as u32).unwrap();
            b.extend_from_slice(c.clip_name.as_bytes());
        });
    }

    // BBIN chunks.
    for c in clips {
        write_chunk(w, b"BBIN", |b| b.extend_from_slice(&c.ogg));
    }

    // Patch FORM size = total bytes after the initial 8-byte header.
    let form_size = (buf.len() - 8) as u32;
    buf[4..8].copy_from_slice(&form_size.to_be_bytes());

    buf
}

fn write_chunk<F: FnOnce(&mut Vec<u8>)>(buf: &mut Vec<u8>, id: &[u8; 4], fill: F) {
    buf.extend_from_slice(id);
    let size_pos = buf.len();
    buf.extend_from_slice(&[0u8; 4]); // placeholder for size
    let payload_start = buf.len();
    fill(buf);
    let payload_size = buf.len() - payload_start;
    // Patch size.
    let size_bytes = (payload_size as u32).to_be_bytes();
    buf[size_pos..size_pos + 4].copy_from_slice(&size_bytes);
    // IFF even-pad.
    if payload_size & 1 != 0 {
        buf.push(0);
    }
}

// ── reader ────────────────────────────────────────────────────────────────────

/// One parsed BIDX + its Ogg payload.
pub struct ParsedClip {
    pub clip_name: String,
    pub channels: u32,
    pub sample_rate: u32,
    pub frame_count: u64,
    /// Raw Ogg bytes (BBIN payload). `None` if the payload offset was
    /// out of bounds in the file.
    pub ogg: Option<Vec<u8>>,
}

pub fn read(path: &Path) -> Result<Vec<ParsedClip>, String> {
    let data = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    parse(&data)
}

pub fn parse(data: &[u8]) -> Result<Vec<ParsedClip>, String> {
    if data.len() < 12 || &data[..4] != b"FORM" || &data[8..12] != b"WLAB" {
        return Err("not a WLAB bank".into());
    }
    let mut bidx_entries: Vec<(u32, u32, u32, u32, u32, String)> = Vec::new();
    let mut off = 12usize;
    while off + 8 <= data.len() {
        let id = &data[off..off + 4];
        let sz = u32::from_be_bytes(data[off + 4..off + 8].try_into().unwrap()) as usize;
        let payload = &data[off + 8..off + 8 + sz.min(data.len().saturating_sub(off + 8))];
        if id == b"BIDX" && payload.len() >= 20 {
            let po = u32::from_be_bytes(payload[0..4].try_into().unwrap());
            let ps = u32::from_be_bytes(payload[4..8].try_into().unwrap());
            let ch = u32::from_be_bytes(payload[8..12].try_into().unwrap());
            let sr = u32::from_be_bytes(payload[12..16].try_into().unwrap());
            let fc = u32::from_be_bytes(payload[16..20].try_into().unwrap());
            let name = String::from_utf8_lossy(&payload[20..]).into_owned();
            bidx_entries.push((po, ps, ch, sr, fc, name));
        }
        off += 8 + sz + (sz & 1);
    }
    let clips = bidx_entries
        .into_iter()
        .map(|(po, ps, ch, sr, fc, name)| {
            let start = po as usize;
            let end = start + ps as usize;
            let ogg = if end <= data.len() {
                Some(data[start..end].to_vec())
            } else {
                None
            };
            ParsedClip {
                clip_name: name,
                channels: ch,
                sample_rate: sr,
                frame_count: fc as u64,
                ogg,
            }
        })
        .collect();
    Ok(clips)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset::ogg;

    #[test]
    fn wlabank_write_read_smoke() {
        // Two synthetic clips: a 0.1 s sine + a 0.2 s zero signal.
        let sr = 48_000u32;
        let sine: Vec<f32> = (0..(sr as usize / 10) * 2)
            .map(|i| (i as f32 * 0.01).sin() * 0.3)
            .collect();
        let silence: Vec<f32> = vec![0.0f32; sr as usize / 5 * 2];

        let ogg1 = ogg::encode(&sine, 2, sr, 0.5).unwrap();
        let ogg2 = ogg::encode(&silence, 2, sr, 0.5).unwrap();

        let clips = vec![
            BankClip {
                clip_name: "module_TEST.wlabank/i1".into(),
                channels: 2,
                sample_rate: sr,
                frame_count: (sine.len() / 2) as u64,
                ogg: ogg1.clone(),
            },
            BankClip {
                clip_name: "module_TEST.wlabank/m1".into(),
                channels: 2,
                sample_rate: sr,
                frame_count: (silence.len() / 2) as u64,
                ogg: ogg2.clone(),
            },
        ];

        let bytes = encode(&clips);

        // Smoke check: must start with FORM...WLAB.
        assert_eq!(&bytes[..4], b"FORM");
        assert_eq!(&bytes[8..12], b"WLAB");

        let parsed = parse(&bytes).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].clip_name, "module_TEST.wlabank/i1");
        assert_eq!(parsed[0].channels, 2);
        assert_eq!(parsed[0].sample_rate, sr);
        assert_eq!(parsed[0].frame_count, (sine.len() / 2) as u64);
        // Payload should be byte-identical.
        assert_eq!(parsed[0].ogg.as_deref(), Some(ogg1.as_slice()));
        assert_eq!(parsed[1].ogg.as_deref(), Some(ogg2.as_slice()));
    }

    /// Verify that the Atlas bank parses cleanly.
    #[test]
    fn atlas_bank_parses() {
        let path = Path::new(
            "/Volumes/Plottn/GREENLOBSTER/COLLECTION/Integrated Assets/Music/ANDD02 - Atlas_TT/ANDD02.wlabank",
        );
        if !path.exists() { return; }
        let clips = read(path).unwrap();
        assert_eq!(clips.len(), 20, "Atlas has 20 BIDX entries");
        // First entry should be the intro mix.
        assert!(clips[0].clip_name.contains("i1"), "first clip is i1");
        // All payloads start with OggS.
        for c in &clips {
            if let Some(ogg) = &c.ogg {
                assert_eq!(&ogg[..4], b"OggS", "clip {} must be Ogg", c.clip_name);
            }
        }
    }
}
