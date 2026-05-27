//! cpal audio engine: capture (input) + monitor (output), bridged by an
//! `rtrb` SPSC ring. The mixing math lives in the pure `mix_block`
//! function (unit-tested below); the input callback only snapshots the
//! per-source atomics and feeds them in. The output callback applies
//! master gain and plays.
//!
//! Borrowed from FIELD's `audio.rs`: `BufferSize::Fixed` (Default silently
//! fails on RME/CoreAudio), and picking the F32 config with the most
//! channels at the device's default sample rate. FIELD is output-only;
//! the input stream + ring bridge is net-new here.

use crate::params::HubParams;
use crate::recording::{make_rings, writer_loop, RecordState, WriterCommand};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::atomic::Ordering;
use std::sync::mpsc::Receiver;
use std::sync::Arc;

/// RME-safe fixed block; `BufferSize::Default` silently fails to fire
/// callbacks on some CoreAudio interfaces (FIELD lesson).
const DEFAULT_BUFFER_SIZE: u32 = 512;
/// Cap on stack/scratch sizing in the callback.
const MAX_SOURCES: usize = 32;
/// Max frames a single callback can hand us (output scratch sizing).
const MAX_BLOCK_FRAMES: usize = 8192;
/// Stereo frames of slack absorbing input/output clock drift.
const RING_FRAMES: usize = 8192;

/// Per-block snapshot of one source's mix controls. The audio thread
/// reads these from the atomics once per block; tests build them
/// directly. Keeping this plain makes `mix_block` pure and testable.
#[derive(Debug, Clone, Copy, Default)]
pub struct SourceMix {
    pub gain: f32,
    pub muted: bool,
    pub soloed: bool,
    pub inverted: bool,
}

/// Mix interleaved `in_ch`-channel `input` down to stereo `out_stereo`,
/// one source per channel pair (`s` → channels `2s`, `2s+1`). Applies
/// per-source gain, polarity invert, mute, and solo gating (if any
/// source is soloed, non-soloed sources are muted). Writes post-gain
/// absolute-peak per source into `peaks`. Pure: no atomics, no I/O.
///
/// `out_stereo` must hold at least `frames*2`; `peaks` at least
/// `mixes.len()`; `input` at least `frames*in_ch` where
/// `frames = input.len() / in_ch`.
pub fn mix_block(
    input: &[f32],
    in_ch: usize,
    mixes: &[SourceMix],
    out_stereo: &mut [f32],
    peaks: &mut [(f32, f32)],
) {
    let frames = if in_ch == 0 { 0 } else { input.len() / in_ch };
    let any_solo = mixes.iter().any(|m| m.soloed);
    for p in peaks.iter_mut() {
        *p = (0.0, 0.0);
    }

    for f in 0..frames {
        let base = f * in_ch;
        let mut sum_l = 0.0f32;
        let mut sum_r = 0.0f32;
        for (s, m) in mixes.iter().enumerate() {
            if m.muted || (any_solo && !m.soloed) {
                continue;
            }
            let sign = if m.inverted { -1.0 } else { 1.0 };
            let l = input[base + s * 2] * m.gain * sign;
            let r = input[base + s * 2 + 1] * m.gain * sign;
            peaks[s].0 = peaks[s].0.max(l.abs());
            peaks[s].1 = peaks[s].1.max(r.abs());
            sum_l += l;
            sum_r += r;
        }
        out_stereo[f * 2] = sum_l;
        out_stereo[f * 2 + 1] = sum_r;
    }
}

pub struct AudioEngine {
    _input_stream: cpal::Stream,
    _output_stream: cpal::Stream,
    #[allow(dead_code)] // diagnostics / future status line + recording
    pub sample_rate: f32,
    #[allow(dead_code)]
    pub input_channels: usize,
    pub num_sources: usize,
}

impl AudioEngine {
    pub fn start(
        params: HubParams,
        record_state: Arc<RecordState>,
        rec_cmd_rx: Receiver<WriterCommand>,
        input_name: Option<&str>,
        output_name: Option<&str>,
    ) -> Result<Self, String> {
        let host = cpal::default_host();
        let in_dev = resolve_input(&host, input_name)?;
        let out_dev = resolve_output(&host, output_name)?;

        // ---- input config: F32, most channels, at the default rate ----
        let in_default = in_dev
            .default_input_config()
            .map_err(|e| format!("default_input_config: {e}"))?;
        let rate = in_default.sample_rate();
        let in_supported = best_f32_input_config(&in_dev, rate)?;
        let in_ch = in_supported.channels() as usize;
        let mut in_cfg: cpal::StreamConfig = in_supported.into();
        in_cfg.buffer_size = cpal::BufferSize::Fixed(DEFAULT_BUFFER_SIZE);

        // ---- output config: default monitor device ----
        let out_default = out_dev
            .default_output_config()
            .map_err(|e| format!("default_output_config: {e}"))?;
        let out_rate = out_default.sample_rate();
        let mut out_cfg: cpal::StreamConfig = out_default.into();
        out_cfg.buffer_size = cpal::BufferSize::Fixed(DEFAULT_BUFFER_SIZE);
        let out_ch = out_cfg.channels as usize;

        let num_sources = (in_ch / 2).min(MAX_SOURCES).min(params.sources.len());

        let (mut prod, mut cons) = rtrb::RingBuffer::<f32>::new(RING_FRAMES * 2);

        // Recording: one ring per source feeding a background WAV writer thread.
        // The thread exits when `rec_cmd_rx` disconnects (engine teardown).
        let (mut rec_prods, rec_cons) = make_rings(num_sources);
        std::thread::spawn(move || writer_loop(rec_cons, rec_cmd_rx));

        // ---- input (capture + mix + meter) ----
        let params_in = params.clone();
        let mut rec_armed = [false; MAX_SOURCES];
        // Pre-allocated scratch — no heap allocation in the steady state.
        let mut mix_scratch: Vec<SourceMix> = vec![SourceMix::default(); num_sources];
        let mut out_buf: Vec<f32> = vec![0.0; MAX_BLOCK_FRAMES * 2];
        let mut peaks: Vec<(f32, f32)> = vec![(0.0, 0.0); num_sources];
        // Opt-in diagnostic (`GATHERER_DEBUG_INPUT=1`): raw input peak across
        // ALL channels, printed ~1×/sec. Distinguishes "no audio arriving"
        // (e.g. missing mic permission → exact 0.0) from a downstream issue.
        let debug_input = std::env::var_os("GATHERER_DEBUG_INPUT").is_some();
        let mut cb_count: u64 = 0;
        let mut raw_peak: f32 = 0.0;
        let input_stream = in_dev
            .build_input_stream(
                &in_cfg,
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    // Snapshot per-source controls from the atomics.
                    for (s, m) in mix_scratch.iter_mut().enumerate() {
                        let sp = &params_in.sources[s];
                        m.gain = sp.load_gain();
                        m.muted = sp.is_muted();
                        m.soloed = sp.is_soloed();
                        m.inverted = sp.is_inverted();
                    }

                    let raw_frames = if in_ch == 0 { 0 } else { data.len() / in_ch };
                    let frames = raw_frames.min(MAX_BLOCK_FRAMES);
                    let usable = &data[..frames * in_ch];
                    mix_block(usable, in_ch, &mix_scratch, &mut out_buf, &mut peaks);

                    for f in 0..frames {
                        // Best-effort: drop on overrun (output clock behind).
                        let _ = prod.push(out_buf[f * 2]);
                        let _ = prod.push(out_buf[f * 2 + 1]);
                    }

                    for s in 0..num_sources {
                        let sp = &params_in.sources[s];
                        fmax_store(&sp.peak_l, peaks[s].0);
                        fmax_store(&sp.peak_r, peaks[s].1);
                    }

                    // Recording: push raw (pre-mix) stereo for armed sources
                    // into their rings; the writer thread drains to WAV. Mute/
                    // solo/gain/invert are monitoring-only and not captured.
                    if record_state.is_active() {
                        for s in 0..num_sources {
                            rec_armed[s] = record_state.is_armed(s);
                        }
                        for f in 0..frames {
                            let base = f * in_ch;
                            for s in 0..num_sources {
                                if rec_armed[s] {
                                    let _ = rec_prods[s].push(usable[base + s * 2]);
                                    let _ = rec_prods[s].push(usable[base + s * 2 + 1]);
                                }
                            }
                        }
                    }

                    // Opt-in diagnostic: raw peak over every input channel.
                    if debug_input {
                        for &x in data {
                            raw_peak = raw_peak.max(x.abs());
                        }
                        cb_count += 1;
                        if cb_count % 100 == 0 {
                            eprintln!(
                                "gatherer-hub: input cb#{cb_count} frames={frames} \
                                 raw_peak={raw_peak:.5} (across {in_ch} ch)"
                            );
                            raw_peak = 0.0;
                        }
                    }
                },
                move |err| eprintln!("gatherer-hub: input stream error: {err}"),
                None,
            )
            .map_err(|e| format!("build_input_stream: {e}"))?;

        // ---- output (monitor) ----
        let params_out = params.clone();
        let output_stream = out_dev
            .build_output_stream(
                &out_cfg,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    let master = params_out.load_master_gain();
                    let frames = data.len() / out_ch.max(1);
                    let mut mpk_l = 0f32;
                    let mut mpk_r = 0f32;
                    for f in 0..frames {
                        // Underrun (input clock behind) → silence.
                        let l = cons.pop().unwrap_or(0.0) * master;
                        let r = cons.pop().unwrap_or(0.0) * master;
                        let base = f * out_ch;
                        if out_ch >= 1 {
                            data[base] = l;
                        }
                        if out_ch >= 2 {
                            data[base + 1] = r;
                        }
                        for c in 2..out_ch {
                            data[base + c] = 0.0;
                        }
                        mpk_l = mpk_l.max(l.abs());
                        mpk_r = mpk_r.max(r.abs());
                    }
                    fmax_store(&params_out.master_peak_l, mpk_l);
                    fmax_store(&params_out.master_peak_r, mpk_r);
                },
                move |err| eprintln!("gatherer-hub: output stream error: {err}"),
                None,
            )
            .map_err(|e| format!("build_output_stream: {e}"))?;

        input_stream.play().map_err(|e| format!("input play: {e}"))?;
        output_stream.play().map_err(|e| format!("output play: {e}"))?;

        eprintln!(
            "gatherer-hub: in='{}' {in_ch}ch@{}Hz  out {out_ch}ch@{}Hz  sources={num_sources}",
            in_dev.name().unwrap_or_default(),
            rate.0,
            out_rate.0,
        );

        Ok(Self {
            _input_stream: input_stream,
            _output_stream: output_stream,
            sample_rate: rate.0 as f32,
            input_channels: in_ch,
            num_sources,
        })
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        // Explicitly stop the OS audio units. On macOS/CoreAudio, relying on
        // `cpal::Stream`'s own drop to halt callbacks proved unreliable when
        // switching capture devices — the old input stream kept firing. An
        // explicit `pause()` guarantees the callbacks stop before teardown.
        let _ = self._input_stream.pause();
        let _ = self._output_stream.pause();
    }
}

/// Resolve a device and report the channel count of its best F32 input
/// config (same selection the engine uses), so the UI sizes its sources
/// to match exactly. 0 if the device can't be resolved/opened.
pub fn input_channel_count(name: Option<&str>) -> usize {
    let host = cpal::default_host();
    let Ok(dev) = resolve_input(&host, name) else {
        return 0;
    };
    let Ok(default) = dev.default_input_config() else {
        return 0;
    };
    best_f32_input_config(&dev, default.sample_rate())
        .map(|c| c.channels() as usize)
        .unwrap_or(0)
}

fn best_f32_input_config(
    dev: &cpal::Device,
    rate: cpal::SampleRate,
) -> Result<cpal::SupportedStreamConfig, String> {
    dev.supported_input_configs()
        .map_err(|e| format!("supported_input_configs: {e}"))?
        .filter(|c| c.sample_format() == cpal::SampleFormat::F32)
        .filter(|c| c.min_sample_rate() <= rate && rate <= c.max_sample_rate())
        .max_by_key(|c| c.channels())
        .map(|c| c.with_sample_rate(rate))
        .ok_or_else(|| "input device has no F32 config at its default rate".to_string())
}

/// Single-writer (audio thread) running max; the UI clears via `swap(0)`.
#[inline]
fn fmax_store(a: &atomic_float::AtomicF32, v: f32) {
    if v > a.load(Ordering::Relaxed) {
        a.store(v, Ordering::Relaxed);
    }
}

fn resolve_input(host: &cpal::Host, name: Option<&str>) -> Result<cpal::Device, String> {
    match name {
        Some(n) => host
            .input_devices()
            .map_err(|e| e.to_string())?
            .find(|d| d.name().map(|dn| dn == n).unwrap_or(false))
            .ok_or_else(|| format!("input device '{n}' not found")),
        None => host
            .default_input_device()
            .ok_or_else(|| "no default input device".to_string()),
    }
}

fn resolve_output(host: &cpal::Host, name: Option<&str>) -> Result<cpal::Device, String> {
    match name {
        Some(n) => host
            .output_devices()
            .map_err(|e| e.to_string())?
            .find(|d| d.name().map(|dn| dn == n).unwrap_or(false))
            .ok_or_else(|| format!("output device '{n}' not found")),
        None => host
            .default_output_device()
            .ok_or_else(|| "no default output device".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build interleaved input: `frames` frames, `in_ch` channels, where
    /// each source pair `s` carries `signal(f)` on both L and R.
    fn make_input(frames: usize, in_ch: usize, signal: &dyn Fn(usize) -> f32) -> Vec<f32> {
        let mut v = vec![0.0f32; frames * in_ch];
        let sources = in_ch / 2;
        for f in 0..frames {
            let s = signal(f);
            for src in 0..sources {
                v[f * in_ch + src * 2] = s;
                v[f * in_ch + src * 2 + 1] = s;
            }
        }
        v
    }

    fn unity(inverted: bool) -> SourceMix {
        SourceMix {
            gain: 1.0,
            muted: false,
            soloed: false,
            inverted,
        }
    }

    /// The M1 acceptance criterion, in code: two identical sources, one
    /// polarity-inverted, sum to digital silence.
    #[test]
    fn polarity_null_two_identical_one_inverted() {
        let (frames, in_ch) = (16, 4);
        let input = make_input(frames, in_ch, &|f| (f as f32 * 0.37).sin());
        let mixes = [unity(false), unity(true)];
        let mut out = vec![0.0; frames * 2];
        let mut peaks = vec![(0.0, 0.0); 2];

        mix_block(&input, in_ch, &mixes, &mut out, &mut peaks);

        for &s in &out {
            assert!(s.abs() < 1e-6, "expected null, got {s}");
        }
        // Each source individually was non-silent (the null is from summing).
        assert!(peaks[0].0 > 0.1 && peaks[1].0 > 0.1);
    }

    #[test]
    fn gain_scales_and_sums() {
        let (frames, in_ch) = (8, 4);
        let input = make_input(frames, in_ch, &|_| 1.0);
        let mixes = [
            SourceMix { gain: 0.5, ..unity(false) },
            SourceMix { gain: 0.25, ..unity(false) },
        ];
        let mut out = vec![0.0; frames * 2];
        let mut peaks = vec![(0.0, 0.0); 2];

        mix_block(&input, in_ch, &mixes, &mut out, &mut peaks);

        // 0.5 + 0.25 summed on each channel.
        for &s in &out {
            assert!((s - 0.75).abs() < 1e-6, "got {s}");
        }
        assert!((peaks[0].0 - 0.5).abs() < 1e-6);
        assert!((peaks[1].0 - 0.25).abs() < 1e-6);
    }

    #[test]
    fn mute_silences_only_that_source() {
        let (frames, in_ch) = (8, 4);
        let input = make_input(frames, in_ch, &|_| 1.0);
        let mixes = [SourceMix { muted: true, ..unity(false) }, unity(false)];
        let mut out = vec![0.0; frames * 2];
        let mut peaks = vec![(0.0, 0.0); 2];

        mix_block(&input, in_ch, &mixes, &mut out, &mut peaks);

        for &s in &out {
            assert!((s - 1.0).abs() < 1e-6, "muted src should leave 1.0, got {s}");
        }
        assert_eq!(peaks[0], (0.0, 0.0)); // muted → no metered peak
        assert!((peaks[1].0 - 1.0).abs() < 1e-6);
    }

    #[test]
    fn solo_gates_non_soloed_sources() {
        let (frames, in_ch) = (8, 6); // 3 sources
        let input = make_input(frames, in_ch, &|_| 1.0);
        let mixes = [
            unity(false),
            SourceMix { soloed: true, ..unity(false) },
            unity(false),
        ];
        let mut out = vec![0.0; frames * 2];
        let mut peaks = vec![(0.0, 0.0); 3];

        mix_block(&input, in_ch, &mixes, &mut out, &mut peaks);

        // Only the soloed source contributes.
        for &s in &out {
            assert!((s - 1.0).abs() < 1e-6, "got {s}");
        }
        assert_eq!(peaks[0], (0.0, 0.0));
        assert!((peaks[1].0 - 1.0).abs() < 1e-6);
        assert_eq!(peaks[2], (0.0, 0.0));
    }
}
