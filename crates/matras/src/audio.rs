//! Live audio for a call: microphone → Opus → the SFU, and back out to the
//! speakers.
//!
//! Everything runs at 48 kHz mono in 20 ms frames, which is the only shape the
//! Mattermost SFU and every other client speak. The capture side downmixes
//! whatever the device gives us; the playback side sums all remote speakers
//! into one stream.
//!
//! The two `cpal` streams live on the GTK thread inside [`AudioIo`] — dropping
//! it stops both devices, which is exactly what leaving a call should do.
//!
//! Incoming RTP passes through a per-speaker de-jitter buffer ([`Jitter`])
//! before it reaches the output queue, so frames play in sequence order rather
//! than arrival order.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, FromSample, Sample as _, SampleFormat, SizedSample, StreamConfig};
use mattermost_calls::{write_audio_sample, AnyCall, Sample, TrackLocalStaticSample, TrackRemote};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::runtime;

const SAMPLE_RATE: u32 = 48_000;
/// 20 ms at 48 kHz.
const FRAME: usize = 960;
/// The most audio one Opus packet can decode to (120 ms).
const MAX_DECODED: usize = FRAME * 6;
/// A queue that has run more than 200 ms ahead of the speakers is dropped
/// rather than left to drift.
const MAX_QUEUE: usize = SAMPLE_RATE as usize / 5;
/// How many frames [`Jitter`] will hold while waiting for a missing sequence
/// number before it gives up on it.
///
/// Three frames is 60 ms: enough to undo the usual single swap or a short
/// burst, short enough that the added mouth-to-ear delay goes unnoticed. It is
/// a ceiling and not a fixed delay — a frame that arrives in order is released
/// the moment it is decoded, so a clean link pays nothing for it.
const JITTER_DEPTH: usize = 3;
/// Further ahead than this (2 s) is a restarted stream rather than loss, so
/// resync instead of concealing a hundred frames one at a time.
const MAX_GAP: u16 = 100;

/// The sample formats the streams below can convert, best first. ALSA offers
/// every format it can emulate — including `u8` for a mono device — so the
/// choice has to be ours rather than "whatever came first".
const FORMATS: [SampleFormat; 3] = [SampleFormat::F32, SampleFormat::I16, SampleFormat::U16];

/// Decoded audio waiting for the output callback, one queue per speaker.
type Queues = Arc<Mutex<HashMap<String, VecDeque<f32>>>>;

/// A running capture/playback pair. Drop it to stop both.
pub struct AudioIo {
    _input: cpal::Stream,
    _output: cpal::Stream,
    queues: Queues,
    /// Set once the user unmutes; until then captured frames are discarded.
    track: Arc<Mutex<Option<Arc<TrackLocalStaticSample>>>>,
}

impl AudioIo {
    /// Opens the default microphone and speakers and starts encoding.
    pub fn start(session: Arc<AnyCall>) -> Result<Self, String> {
        let host = cpal::default_host();
        let mic = host
            .default_input_device()
            .ok_or("no microphone is available")?;
        let speakers = host
            .default_output_device()
            .ok_or("no audio output is available")?;

        let (in_cfg, in_fmt) = config_at_48k(&mic, true)?;
        let (out_cfg, out_fmt) = config_at_48k(&speakers, false)?;

        let queues: Queues = Arc::default();
        let track: Arc<Mutex<Option<Arc<TrackLocalStaticSample>>>> = Arc::default();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

        let input = build_input(&mic, &in_cfg, in_fmt, tx)?;
        let output = build_output(&speakers, &out_cfg, out_fmt, queues.clone())?;
        input.play().map_err(|e| e.to_string())?;
        output.play().map_err(|e| e.to_string())?;

        runtime::runtime().spawn(encode_loop(session, track.clone(), rx));

        Ok(AudioIo {
            _input: input,
            _output: output,
            queues,
            track,
        })
    }

    /// Starts playing a remote participant's track.
    pub fn play(&self, session_id: String, track: Arc<TrackRemote>) {
        let queues = self.queues.clone();
        runtime::runtime().spawn(async move {
            let mut decoder = match opus::Decoder::new(SAMPLE_RATE, opus::Channels::Mono) {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!(error = %e, "could not create an Opus decoder");
                    return;
                }
            };
            let mut pcm = vec![0f32; MAX_DECODED];
            let mut jitter = Jitter::default();
            let mut ready: Vec<f32> = Vec::with_capacity(MAX_DECODED);
            let mut packets = 0u64;
            while let Ok((packet, _)) = track.read_rtp().await {
                if packet.payload.is_empty() {
                    continue;
                }
                let Ok(n) = decoder.decode_float(&packet.payload, &mut pcm, false) else {
                    continue;
                };
                let released = jitter.push(packet.header.sequence_number, pcm[..n].to_vec());
                if released.is_empty() {
                    continue;
                }
                ready.clear();
                for slot in released {
                    match slot {
                        Some(frame) => ready.extend_from_slice(&frame),
                        // Opus hides a lost frame better than silence does: an
                        // empty payload runs its concealer. The length of the
                        // output buffer is how much it conceals, hence `FRAME`.
                        None => match decoder.decode_float(&[], &mut pcm[..FRAME], false) {
                            Ok(n) => ready.extend_from_slice(&pcm[..n]),
                            Err(_) => ready.resize(ready.len() + FRAME, 0.0),
                        },
                    }
                }
                // The output callback wants this lock every few milliseconds,
                // so decoding and reordering both happen outside it.
                let mut queues = queues.lock().unwrap();
                let queue = queues.entry(session_id.clone()).or_default();
                if queue.len() > MAX_QUEUE {
                    queue.clear();
                }
                queue.extend(&ready);
                packets += 1;
                // One line a second is enough to see whether audio is flowing.
                if packets % 50 == 0 {
                    tracing::debug!(%session_id, packets, queued = queue.len(), "receiving");
                }
            }
            queues.lock().unwrap().remove(&session_id);
        });
    }

    /// Hands the capture loop the track to write into, after an unmute.
    pub fn set_track(&self, track: Arc<TrackLocalStaticSample>) {
        *self.track.lock().unwrap() = Some(track);
    }
}

/// Reorders decoded frames by RTP sequence number, one of these per speaker.
///
/// RTP arrives out of order and unevenly spaced, and playing frames in arrival
/// order turns a swapped pair into a warble. Frames go in keyed by sequence
/// number and come out in sequence order, with a `None` standing in for every
/// sequence number that never turned up.
///
/// Every comparison goes through `wrapping_sub`, because sequence numbers are
/// 16 bits and wrap every 65536 frames — 22 minutes at 20 ms a frame. Treating
/// the wrap as a backwards jump would stall the buffer for good.
#[derive(Default)]
struct Jitter {
    /// The sequence number that plays next; `None` until the first frame
    /// decides where the stream starts.
    next: Option<u16>,
    /// Frames that arrived early, in no particular order. Never more than
    /// [`JITTER_DEPTH`] of them, so a linear scan is the whole index.
    pending: Vec<(u16, Vec<f32>)>,
}

impl Jitter {
    /// Takes one decoded frame and returns whatever is now playable, in order:
    /// `Some(frame)` for a frame that arrived, `None` for one that is lost and
    /// has to be concealed by the caller.
    fn push(&mut self, seq: u16, frame: Vec<f32>) -> Vec<Option<Vec<f32>>> {
        let mut next = *self.next.get_or_insert(seq);
        if self.pending.iter().any(|(s, _)| *s == seq) {
            return Vec::new(); // A duplicate; we already hold this frame.
        }
        // Distance from the slot we are waiting for. Half the sequence space
        // away means the frame is behind us rather than ahead of us.
        let ahead = seq.wrapping_sub(next);
        if ahead >= 0x8000 {
            // Its slot has already played. Playing it now, out of place, would
            // sound worse than the gap it left.
            return Vec::new();
        }
        if ahead > MAX_GAP {
            self.pending.clear();
            next = seq;
        }
        self.pending.push((seq, frame));

        let mut out = Vec::new();
        loop {
            while let Some(i) = self.pending.iter().position(|(s, _)| *s == next) {
                out.push(Some(self.pending.swap_remove(i).1));
                next = next.wrapping_add(1);
            }
            if self.pending.len() < JITTER_DEPTH {
                break;
            }
            // `next` has had the whole window to show up. Conceal it and move
            // on; stalling the stream for a frame that is not coming costs
            // more than the frame is worth.
            out.push(None);
            next = next.wrapping_add(1);
        }
        self.next = Some(next);
        out
    }
}

/// Picks a device configuration that runs at exactly 48 kHz.
///
/// Resampling is the one thing this pipeline refuses to do: ALSA's `Nearest`
/// rate matching would silently hand us 44.1 kHz and every frame would go out
/// pitched up.
fn config_at_48k(device: &Device, input: bool) -> Result<(StreamConfig, SampleFormat), String> {
    let supported: Vec<_> = if input {
        device
            .supported_input_configs()
            .map_err(|e| e.to_string())?
            .collect()
    } else {
        device
            .supported_output_configs()
            .map_err(|e| e.to_string())?
            .collect()
    };

    supported
        .into_iter()
        .filter(|c| c.min_sample_rate() <= SAMPLE_RATE && SAMPLE_RATE <= c.max_sample_rate())
        .filter_map(|c| Some((FORMATS.iter().position(|f| *f == c.sample_format())?, c)))
        // Best format first, then fewest channels: we only ever need one, and
        // asking for eight on a surround card wastes a conversion.
        .min_by_key(|(rank, c)| (*rank, c.channels()))
        .map(|(_, c)| c)
        .map(|c| {
            let c = c.with_sample_rate(SAMPLE_RATE);
            (c.config(), c.sample_format())
        })
        .ok_or_else(|| {
            let what = if input { "microphone" } else { "audio output" };
            format!("the default {what} does not support 48 kHz")
        })
}

fn build_input(
    device: &Device,
    cfg: &StreamConfig,
    fmt: SampleFormat,
    tx: UnboundedSender<Vec<f32>>,
) -> Result<cpal::Stream, String> {
    fn make<T>(
        device: &Device,
        cfg: &StreamConfig,
        tx: UnboundedSender<Vec<f32>>,
    ) -> Result<cpal::Stream, cpal::Error>
    where
        T: SizedSample,
        f32: FromSample<T>,
    {
        let channels = cfg.channels as usize;
        device.build_input_stream::<T, _, _>(
            *cfg,
            move |data: &[T], _| {
                let mono = data
                    .chunks(channels)
                    .map(|frame| {
                        frame.iter().map(|s| f32::from_sample(*s)).sum::<f32>() / channels as f32
                    })
                    .collect();
                let _ = tx.send(mono);
            },
            |e| tracing::warn!(error = %e, "microphone stream error"),
            None,
        )
    }

    match fmt {
        SampleFormat::F32 => make::<f32>(device, cfg, tx),
        SampleFormat::I16 => make::<i16>(device, cfg, tx),
        SampleFormat::U16 => make::<u16>(device, cfg, tx),
        other => return Err(format!("unsupported capture format {other}")),
    }
    .map_err(|e| e.to_string())
}

fn build_output(
    device: &Device,
    cfg: &StreamConfig,
    fmt: SampleFormat,
    queues: Queues,
) -> Result<cpal::Stream, String> {
    fn make<T>(
        device: &Device,
        cfg: &StreamConfig,
        queues: Queues,
    ) -> Result<cpal::Stream, cpal::Error>
    where
        T: SizedSample + FromSample<f32>,
    {
        let channels = cfg.channels as usize;
        device.build_output_stream::<T, _, _>(
            *cfg,
            move |data: &mut [T], _| {
                let mut queues = queues.lock().unwrap();
                for frame in data.chunks_mut(channels) {
                    let mixed: f32 = queues
                        .values_mut()
                        .filter_map(|q| q.pop_front())
                        .sum::<f32>()
                        .clamp(-1.0, 1.0);
                    let sample = T::from_sample(mixed);
                    frame.fill(sample);
                }
            },
            |e| tracing::warn!(error = %e, "playback stream error"),
            None,
        )
    }

    match fmt {
        SampleFormat::F32 => make::<f32>(device, cfg, queues),
        SampleFormat::I16 => make::<i16>(device, cfg, queues),
        SampleFormat::U16 => make::<u16>(device, cfg, queues),
        other => return Err(format!("unsupported playback format {other}")),
    }
    .map_err(|e| e.to_string())
}

/// Encodes captured audio into 20 ms Opus packets for as long as the channel
/// stays open — which is until [`AudioIo`] is dropped.
async fn encode_loop(
    session: Arc<AnyCall>,
    track: Arc<Mutex<Option<Arc<TrackLocalStaticSample>>>>,
    mut rx: UnboundedReceiver<Vec<f32>>,
) {
    let mut encoder =
        match opus::Encoder::new(SAMPLE_RATE, opus::Channels::Mono, opus::Application::Voip) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, "could not create an Opus encoder");
                return;
            }
        };

    let mut pending: Vec<f32> = Vec::with_capacity(FRAME * 2);
    let mut frames = 0u64;
    while let Some(chunk) = rx.recv().await {
        pending.extend_from_slice(&chunk);
        while pending.len() >= FRAME {
            let frame: Vec<f32> = pending.drain(..FRAME).collect();
            // Muted means the SFU drops our RTP anyway; not encoding it saves
            // the CPU and keeps us off the "speaking" list for certain.
            if session.is_muted() {
                continue;
            }
            let Some(track) = track.lock().unwrap().clone() else {
                continue;
            };
            let Ok(data) = encoder.encode_vec_float(&frame, 4000) else {
                continue;
            };
            let sample = Sample {
                data: data.into(),
                duration: Duration::from_millis(20),
                ..Default::default()
            };
            let level = level_dbov(&frame);
            frames += 1;
            if frames % 50 == 0 {
                tracing::debug!(frames, bytes = sample.data.len(), level, "sending");
            }
            if let Err(e) = write_audio_sample(&track, &sample, level).await {
                tracing::warn!(error = %e, "could not send audio; stopping capture");
                return;
            }
        }
    }
}

/// RFC 6464 level of a frame: 0 is loudest, 127 is silence.
///
/// The SFU's voice detector watches the *variance* of this over a 50-packet
/// window, so it has to be measured per frame — a constant never registers as
/// speech, however loud it claims to be.
fn level_dbov(frame: &[f32]) -> u8 {
    let rms = (frame.iter().map(|s| s * s).sum::<f32>() / frame.len() as f32).sqrt();
    if rms <= 1e-6 {
        return 127;
    }
    (-20.0 * rms.log10()).clamp(0.0, 127.0) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One-sample frames stand in for 20 ms of audio: the buffer never looks
    /// inside a frame, so the tests do not need real ones.
    fn push(jitter: &mut Jitter, seq: u16) -> Vec<Option<u16>> {
        jitter
            .push(seq, vec![seq as f32])
            .into_iter()
            .map(|slot| slot.map(|frame| frame[0] as u16))
            .collect()
    }

    #[test]
    fn frames_in_order_play_without_being_held() {
        let mut jitter = Jitter::default();
        for seq in 100..110u16 {
            assert_eq!(push(&mut jitter, seq), vec![Some(seq)]);
        }
    }

    #[test]
    fn a_swapped_pair_is_put_back_in_order() {
        let mut jitter = Jitter::default();
        assert_eq!(push(&mut jitter, 0), vec![Some(0)]);
        assert!(push(&mut jitter, 2).is_empty(), "2 waits for 1");
        assert_eq!(push(&mut jitter, 1), vec![Some(1), Some(2)]);
    }

    #[test]
    fn a_lost_frame_is_concealed_rather_than_stalling() {
        let mut jitter = Jitter::default();
        assert_eq!(push(&mut jitter, 0), vec![Some(0)]);
        assert!(push(&mut jitter, 2).is_empty());
        assert!(push(&mut jitter, 3).is_empty());
        // Three frames waiting is the whole window: 1 is not coming.
        assert_eq!(
            push(&mut jitter, 4),
            vec![None, Some(2), Some(3), Some(4)],
            "the buffer should give up on 1 and drain"
        );
        assert_eq!(push(&mut jitter, 5), vec![Some(5)]);
    }

    #[test]
    fn a_frame_whose_slot_already_played_is_dropped() {
        let mut jitter = Jitter::default();
        for seq in 0..4u16 {
            push(&mut jitter, seq);
        }
        assert!(push(&mut jitter, 1).is_empty(), "1 played three frames ago");
        assert_eq!(push(&mut jitter, 4), vec![Some(4)], "and 4 still plays");
    }

    #[test]
    fn sequence_numbers_wrap_without_stalling() {
        let mut jitter = Jitter::default();
        assert_eq!(push(&mut jitter, u16::MAX - 1), vec![Some(u16::MAX - 1)]);
        // A swap straddling the wrap: 0 comes before 65535 does.
        assert!(push(&mut jitter, 0).is_empty());
        assert_eq!(push(&mut jitter, u16::MAX), vec![Some(u16::MAX), Some(0)]);
        assert_eq!(push(&mut jitter, 1), vec![Some(1)]);
        // And loss just past the wrap: 2 never arrives.
        assert!(push(&mut jitter, 3).is_empty());
        assert!(push(&mut jitter, 4).is_empty());
        assert_eq!(push(&mut jitter, 5), vec![None, Some(3), Some(4), Some(5)]);
        // A pre-wrap frame is 7 frames old, not 65529 frames early.
        assert!(push(&mut jitter, u16::MAX).is_empty());
    }

    #[test]
    fn a_restarted_stream_resyncs_instead_of_concealing_its_way_there() {
        let mut jitter = Jitter::default();
        push(&mut jitter, 0);
        assert_eq!(push(&mut jitter, 5_000), vec![Some(5_000)]);
    }

    #[test]
    fn level_runs_from_silence_to_full_scale() {
        assert_eq!(level_dbov(&[0.0; FRAME]), 127);
        // Full-scale square wave is 0 dBov by definition.
        assert_eq!(level_dbov(&[1.0; FRAME]), 0);
        // Quieter speech sits somewhere in between, and louder means smaller.
        let loud = level_dbov(&[0.5; FRAME]);
        let quiet = level_dbov(&[0.01; FRAME]);
        assert!(loud < quiet, "{loud} should be below {quiet}");
        assert!((1..127).contains(&quiet));
    }
}
