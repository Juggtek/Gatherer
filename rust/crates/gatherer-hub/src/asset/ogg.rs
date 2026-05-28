//! Ogg Vorbis encode (via `vorbis_rs`) and decode (via `lewton`).
//! Both operate on interleaved stereo f32 PCM at the session sample rate.

use vorbis_rs::{VorbisBitrateManagementStrategy, VorbisEncoderBuilder};

/// Encode interleaved stereo f32 PCM → Ogg Vorbis bytes.
/// `quality` 0..1 (0.6 ≈ 192 kbps VBR at 48 kHz, matches Atlas quality).
pub fn encode(interleaved: &[f32], channels: u32, sample_rate: u32, quality: f32) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut encoder = VorbisEncoderBuilder::new(
        std::num::NonZeroU32::new(sample_rate).unwrap(),
        std::num::NonZeroU8::new(channels as u8).unwrap(),
        &mut out,
    )
    .map_err(|e| format!("vorbis encoder init: {e}"))?
    .bitrate_management_strategy(VorbisBitrateManagementStrategy::QualityVbr {
        target_quality: quality,
    })
    .build()
    .map_err(|e| format!("vorbis encoder build: {e}"))?;

    // Feed in blocks to avoid large stack arrays.
    const BLOCK: usize = 4096;
    let ch = channels as usize;
    let frames = interleaved.len() / ch;
    let mut pos = 0;
    while pos < frames {
        let end = (pos + BLOCK).min(frames);
        // Deinterleave: vorbis_rs wants [channel][frame] layout.
        let block_frames = end - pos;
        let planar: Vec<Vec<f32>> = (0..ch)
            .map(|c| {
                (0..block_frames)
                    .map(|f| interleaved[(pos + f) * ch + c])
                    .collect()
            })
            .collect();
        let slices: Vec<&[f32]> = planar.iter().map(|v| v.as_slice()).collect();
        encoder
            .encode_audio_block(slices)
            .map_err(|e| format!("vorbis encode block: {e}"))?;
        pos = end;
    }
    encoder
        .finish()
        .map_err(|e| format!("vorbis encoder finish: {e}"))?;
    Ok(out)
}

/// Decode Ogg Vorbis bytes → interleaved stereo f32 PCM.
/// Returns `(pcm, channels, sample_rate)`.
pub fn decode(ogg: &[u8]) -> Result<(Vec<f32>, u32, u32), String> {
    use lewton::inside_ogg::OggStreamReader;
    let cursor = std::io::Cursor::new(ogg);
    let mut reader = OggStreamReader::new(cursor)
        .map_err(|e| format!("lewton init: {e}"))?;
    let channels = reader.ident_hdr.audio_channels as u32;
    let sample_rate = reader.ident_hdr.audio_sample_rate;
    let mut pcm: Vec<f32> = Vec::new();
    loop {
        match reader.read_dec_packet_itl() {
            Ok(Some(pkt)) => {
                pcm.extend(pkt.iter().map(|&s| s as f32 / 32768.0));
            }
            Ok(None) => break,
            Err(e) => return Err(format!("lewton decode: {e}")),
        }
    }
    Ok((pcm, channels, sample_rate))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    fn sine(sr: u32, hz: f32, seconds: f32) -> Vec<f32> {
        let n = (sr as f32 * seconds) as usize;
        let mut v = Vec::with_capacity(n * 2);
        for i in 0..n {
            let s = (2.0 * PI * hz * i as f32 / sr as f32).sin() * 0.5;
            v.push(s); v.push(s);
        }
        v
    }

    #[test]
    fn ogg_round_trip_rms() {
        let sr = 48_000u32;
        let pcm = sine(sr, 440.0, 0.5); // 500 ms sine
        let encoded = encode(&pcm, 2, sr, 0.6).expect("encode");
        assert!(encoded.starts_with(b"OggS"), "must start with Ogg magic");
        let (decoded, ch, dsr) = decode(&encoded).expect("decode");
        assert_eq!(ch, 2);
        assert_eq!(dsr, sr);
        // Compare RMS (Vorbis is lossy; allow ±3 dB headroom).
        let rms = |s: &[f32]| -> f32 {
            (s.iter().map(|&x| x * x).sum::<f32>() / s.len() as f32).sqrt()
        };
        let rms_in  = rms(&pcm);
        let rms_out = rms(&decoded);
        let ratio_db = 20.0 * (rms_out / rms_in.max(1e-9)).log10();
        assert!(
            ratio_db.abs() < 3.0,
            "decoded RMS should be within ±3 dB of source, got {ratio_db:.1} dB"
        );
    }
}
