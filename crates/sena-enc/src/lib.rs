//! sena-enc: pipeline orchestration (WAV IO, subprocess drivers, packet extraction).

use sena_core::{PRE_GAIN, Profile, SAMPLE_RATE, es};
use sena_mux::mka::{self, SenaBlock};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::process::Command;

mod m4a;
pub mod ogg;
mod timing;
pub mod tools;
pub mod wav;
pub mod workdir;

pub use m4a::{extract_m4a, m4a_stream_rate};
pub use timing::StageTimes;
pub use tools::{
    SENAV_MIN_MILESTONE, check_exhale, check_senav_version, check_version, exhale_version,
    opusenc_version, parse_exhale_version, version_ge,
};

use timing::end_tick;
use tools::{apply_senav_env, cli_path, needs_windows_paths};

/// Errors surfaced to the CLI.
#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Subprocess(String),
    Format(String),
}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "io: {e}"),
            Error::Subprocess(s) => write!(f, "subprocess: {s}"),
            Error::Format(s) => write!(f, "format: {s}"),
        }
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

pub struct EncoderConfig<'a> {
    pub profile: Profile,
    pub total_kbps: u32,
    pub use_senav: bool,
    /// Three-track layout: split the 600 Hz-high band again at the Opus
    /// b19/b20 boundary (12000 Hz) and encode the top band as a second opus
    /// track (`A_OPUSHF`, tiered nominal: 96 kbit/s, 128 kbit/s at totals
    /// >= 320k). The top band is shifted down to baseband (analytic-signal
    /// SSB shift by 12000 Hz) and carried at 16 kHz: a directly coded
    /// top-octave-only track starves under Opus's content-blind band
    /// allocation, while shifted baseband content is served normally.
    pub three_track: bool,
    /// Mid-layer codec. `Xhe` = the "xfi" recipe (Fraunhofer IIS xHE-AAC
    /// CBR 128k at 44.1 kHz via ezcd-cli, [`sena_core::XFI_MID_KBPS`]);
    /// always three-track.
    pub mid_codec: sena_core::MidCodec,
    /// Top-band codec (three-track only): `Xhe` = Fraunhofer IIS xHE-AAC
    /// CBR 128k at 44.1 kHz via ezcd-cli (the xfi 288k variant; the stream
    /// then decodes natively at 44.1 kHz). Requires `mid_codec == Xhe`.
    pub hf_codec: sena_core::HfCodec,
    /// Value passed through to the opusenc-senav mid encode as
    /// `AUDIFF_TOPBAND_STEREO` (kbps, unmodified). None = knob off.
    pub topband_kbps: Option<u32>,
    pub exhale: &'a str,
    pub opusenc: &'a str,
    /// ezcd-cli executable (required when `mid_codec` is `Xhe`): probed as
    /// `ezcd-cli` then `ezcd-cli-portable` next to senaenc, then on PATH.
    pub ezcd_cli: Option<&'a str>,
    /// Base directory for the workdir: a unique `senaenc-<pid>-<rand>`
    /// subdir is created under it after a free-space preflight (needs the
    /// input length, known once the WAV header is parsed).
    pub workdir: &'a Path,
    /// Fallback base when the primary fails the preflight or fills up
    /// mid-encode (the LF spool is relocated there; see workdir.rs).
    pub workdir_fallback: Option<&'a Path>,
    /// Registry the chosen workdir(s) are reported to for exit cleanup.
    pub cleanup: Option<workdir::Cleanup>,
    /// Optional gentle HF tilt on the Opus-bound band, percent of the
    /// reference curve (None/0 = off). In the three-track layout the tilt
    /// applies to the mid band only.
    pub hf_tilt_pct: Option<f64>,
}

/// What an encode run left behind: the workdir actually used (and, when
/// the LF spool overflowed the primary volume, the fallback dir it was
/// relocated into).
pub struct EncodeOutcome {
    pub workdir: std::path::PathBuf,
    pub spill_dir: Option<std::path::PathBuf>,
}

/// Streaming input read size.
const READ_CHUNK_BYTES: usize = 256 * 1024;
/// Frames pulled from the WAV stream per iteration (1 s at 48 kHz).
const PIPELINE_CHUNK_FRAMES: usize = 48_000;
/// Zero-pad quantum for the xfi mid rail tail (0.1 s at 48 kHz).
const PAD_QUANTUM_FRAMES: usize = 4800;
/// Bail-out guard for the xfi tail pad loop.
const MAX_PAD_ITERS: u32 = 16;
/// Container cluster target (the muxer writes ~1 s clusters).
const MKA_CLUSTER_MS: u64 = 1000;

/// Apply the codec-domain PRE_GAIN to interleaved samples.
fn pre_gained<'a>(x: impl IntoIterator<Item = &'a f64>) -> Vec<f64> {
    x.into_iter().map(|v| v * PRE_GAIN).collect()
}

/// Run encoder subprocesses concurrently, waiting for all (their
/// stdout/stderr are piped so error text is preserved). Every child is
/// drained on its own thread: waiting one-by-one would let a chatty later
/// process stall on a full pipe while an earlier one is being waited on.
fn run_parallel(cmds: &mut [(&str, &mut Command)]) -> Option<Error> {
    use std::process::Stdio;
    for (_, c) in cmds.iter_mut() {
        c.stdout(Stdio::piped()).stderr(Stdio::piped());
    }
    let spawned: Vec<std::io::Result<(&str, std::process::Child)>> = cmds
        .iter_mut()
        .map(|(what, c)| {
            c.spawn()
                .map_err(|e| std::io::Error::new(e.kind(), format!("{what}: {e}")))
                .map(|h| (*what, h))
        })
        .collect();
    let results: Vec<Result<(&str, std::process::Output), Error>> = std::thread::scope(|s| {
        let handles: Vec<_> = spawned
            .into_iter()
            .map(|r| {
                s.spawn(move || -> Result<(&str, std::process::Output), Error> {
                    let (what, child) = r.map_err(Error::Io)?;
                    let out = child.wait_with_output().map_err(Error::Io)?;
                    Ok((what, out))
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|e| std::panic::resume_unwind(e)))
            .collect()
    });
    let mut err: Option<Error> = None;
    for r in results {
        match r {
            Ok((what, o)) => {
                if !o.status.success() && err.is_none() {
                    err = Some(Error::Subprocess(format!(
                        "{what} failed: {}",
                        String::from_utf8_lossy(&o.stderr)
                    )));
                }
            }
            Err(e) => {
                if err.is_none() {
                    err = Some(e);
                }
            }
        }
    }
    err
}

pub fn encode(cfg: &EncoderConfig, input: &Path, output: &Path) -> Result<EncodeOutcome, Error> {
    let mut f = std::fs::File::open(input)?;
    encode_stream(cfg, &mut f, output)
}

pub fn encode_bytes(cfg: &EncoderConfig, input: &[u8], output: &Path) -> Result<EncodeOutcome, Error> {
    let mut bytes = input;
    encode_stream(cfg, &mut bytes, output)
}

/// Audio SHA-256 of the encoded audio elementary streams carried by the
/// container: the Opus stream (OpusHead + Opus audio packets) and the
/// xHE-AAC stream (ASC + raw AUs), in Sena track order. The streams are the
/// Audio content hash (domain v3): SHA-256 over the zeroed-hash SENA-INIT
/// blob and all composite block payloads in decode order. The hash is
/// length-prefixed and domain-separated so it is deterministic and
/// independent of container layout, timestamps and tags. It intentionally
/// does NOT hash the input PCM or the decoded output.
pub fn encoded_audio_sha256_es(init_zeroed_blob: &[u8], blocks: &[Vec<u8>]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(es::SHA256_DOMAIN_V4);
    h.update((init_zeroed_blob.len() as u32).to_le_bytes());
    h.update(init_zeroed_blob);
    h.update((blocks.len() as u64).to_le_bytes());
    for b in blocks {
        h.update((b.len() as u32).to_le_bytes());
        h.update(b);
    }
    h.finalize().into()
}

/// Full encode pipeline over a streaming input (stdin-safe).
///
/// The WAV is parsed incrementally and pushed through the streaming DSP
/// Rail delivery sink: each encoder is fed in its own native input domain
/// (see the contract table on [`rail_layout`]). File sinks spool to the
/// workdir; pipe sinks stream WAV frames into the encoder's stdin (bounded
/// memory via pipe backpressure, nothing touches disk).
enum RailSink {
    File(wav::WavWriter),
    Pipe(&'static str, wav::PipeWavWriter<std::process::ChildStdin>),
}

impl RailSink {
    fn push(&mut self, x: &[f64]) -> Result<(), Error> {
        match self {
            RailSink::File(w) => w.push(x),
            RailSink::Pipe(name, w) => w.push(x).map_err(|e| match e {
                Error::Io(io) if io.kind() == std::io::ErrorKind::BrokenPipe => {
                    Error::Subprocess(format!("encoder {name} closed its input pipe early: {io}"))
                }
                other => other,
            }),
        }
    }

    fn finish(self) -> Result<(), Error> {
        match self {
            RailSink::File(w) => w.finish(),
            RailSink::Pipe(_, w) => {
                // Dropping the returned ChildStdin closes the pipe (EOF).
                w.finish().map(|_| ())
            }
        }
    }
}

/// Which rails exist, and with what delivery contract, for the selected
/// profile family. Contracts (encoder-native input domains):
///   exhale (LF):        s16 file @ lf_rate (frozen operating point — s24
///                       input shifted exhale's VBR allocation ~18% on the
///                       golden clip; its reader lseeks, so no pipe.)
///   opusenc (mid / HF): f32 pipe (unknown-length convention, read to EOF)
///   ezcd-cli (xfi mid): f32 pipe @ 44.1 kHz, clamped to [-1, 1] + non-
///                       finite -> 0 (the FhG finalize-stage contract),
///                       >= 0.4 s (one BS.1770 gating block) zero-padded
fn rail_layout(cfg: &EncoderConfig) -> Vec<RailRole> {
    if !cfg.three_track {
        vec![RailRole::Hf]
    } else if cfg.mid_codec == sena_core::MidCodec::Xhe {
        vec![RailRole::MidXhe44, RailRole::Hf]
    } else {
        vec![RailRole::Mid, RailRole::Hf]
    }
}

#[derive(Clone, Copy, PartialEq)]
enum RailRole {
    Mid,
    MidXhe44,
    Hf,
    HfXhe44,
}

/// ENOSPC (unix) / ERROR_DISK_FULL / ERROR_HANDLE_DISK_FULL (Windows).
const ENOSPC_UNIX: i32 = 28;
const ERROR_DISK_FULL: i32 = 112;
const ERROR_HANDLE_DISK_FULL: i32 = 39;

fn is_disk_full(e: &std::io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(ENOSPC_UNIX) | Some(ERROR_DISK_FULL) | Some(ERROR_HANDLE_DISK_FULL)
    )
}

/// Free-space floor when the input length is unknown (streaming input):
/// proceed only if the volume clearly has room for a typical encode.
const MIN_FREE_UNKNOWN_INPUT: u64 = 512 * 1024 * 1024;

/// Pick and create the workdir, now that the input header gave us the
/// track length: preflight the primary base's free space against the
/// estimated footprint; on short/unknown space or a failed mkdir, fall
/// back to the secondary base (a warning goes to stderr either way).
fn choose_workdir(cfg: &EncoderConfig, ws: &wav::WavStream) -> Result<std::path::PathBuf, Error> {
    let est = ws
        .declared_frames()
        .map(|f| workdir::estimate_bytes(cfg.profile.lf_rate(), ws.rate().unwrap_or(SAMPLE_RATE), f as u64, cfg.total_kbps));
    let fits = |base: &Path| -> bool {
        match (workdir::free_space(base), est) {
            (Some(free), Some(need)) => free >= need,
            (Some(free), None) => free >= MIN_FREE_UNKNOWN_INPUT,
            (None, _) => true, // space not measurable: proceed optimistically
        }
    };
    let primary = cfg.workdir;
    let chosen = if fits(primary) {
        match workdir::create_unique_dir(primary) {
            Ok(d) => d,
            Err(_) if cfg.workdir_fallback.is_some() => {
                let fb = cfg.workdir_fallback.unwrap();
                eprintln!(
                    "senaenc: workdir {} unusable, falling back to {}",
                    primary.display(),
                    fb.display()
                );
                workdir::create_unique_dir(fb)?
            }
            Err(e) => return Err(e),
        }
    } else if let Some(fb) = cfg.workdir_fallback {
        eprintln!(
            "senaenc: workdir {} low on space, falling back to {}",
            primary.display(),
            fb.display()
        );
        workdir::create_unique_dir(fb)?
    } else {
        // No fallback configured: proceed optimistically (the estimate is
        // conservative; a real ENOSPC still surfaces from the writers).
        workdir::create_unique_dir(primary)?
    };
    if let Some(c) = &cfg.cleanup {
        c.register(chosen.clone());
    }
    Ok(chosen)
}

/// The LF rail's file sink (exhale input): a WavWriter that relocates to
/// the fallback volume on ENOSPC and keeps appending there.
struct SpoolWriter {
    w: wav::WavWriter,
    spill_base: Option<std::path::PathBuf>,
    cleanup: Option<workdir::Cleanup>,
    spilled_to: Option<std::path::PathBuf>,
}

impl SpoolWriter {
    fn push(&mut self, x: &[f64]) -> Result<(), Error> {
        match self.w.push(x) {
            Err(Error::Io(e)) if is_disk_full(&e) && self.spill_base.is_some() => {
                let base = self.spill_base.as_ref().expect("guarded by is_some");
                let dir = workdir::create_unique_dir(base)?;
                let newp = dir.join("lf.wav");
                eprintln!(
                    "senaenc: workdir volume full; relocating the LF spool to {}",
                    newp.display()
                );
                self.w.relocate(&newp)?;
                if let Some(c) = &self.cleanup {
                    c.register(dir.clone());
                }
                self.spilled_to = Some(dir);
                self.w.push(x)
            }
            r => r,
        }
    }

    fn finish(self) -> Result<(), Error> {
        self.w.finish()
    }
}

/// A spawned encoder child consuming a rail pipe; reaped after the pass.
struct PipedChild {
    name: &'static str,
    child: std::process::Child,
}

struct PipedSet {
    children: Vec<PipedChild>,
}

impl PipedSet {
    /// Reap every child (its stdin was closed when the sink dropped).
    fn reap(mut self) -> Result<(), Error> {
        let mut err: Option<Error> = None;
        for pc in std::mem::take(&mut self.children) {
            match pc.child.wait_with_output() {
                Ok(out) if out.status.success() => {}
                Ok(out) => {
                    if err.is_none() {
                        err = Some(Error::Subprocess(format!(
                            "{} failed: {}",
                            pc.name,
                            String::from_utf8_lossy(&out.stderr)
                        )));
                    }
                }
                Err(e) => {
                    if err.is_none() {
                        err = Some(Error::Io(e));
                    }
                }
            }
        }
        match err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

impl Drop for PipedSet {
    fn drop(&mut self) {
        for pc in &mut self.children {
            let _ = pc.child.kill();
            let _ = pc.child.wait();
        }
    }
}

/// (rate normalize -> crossover split -> LF downsample) chunk by chunk,
/// writing the two codec-input WAVs as the input arrives. This keeps the
/// memory bounded and makes the input consumption rate track the actual
/// work, so a feeding host (foobar2000 converter) drives its progress bar
/// with the real pipeline instead of racing to 100% while senaenc then
/// does all the work.
/// Three-track top-band chain: the 12000 Hz second split, the SSB
/// down-shift to baseband and the 48k->16k downsample exist together or
/// not at all.
struct TopChain {
    split: sena_dsp::CrossoverStream,
    shift: sena_dsp::ShiftStream,
    rs: sena_dsp::StreamResampler,
}

/// The streaming DSP + rail-delivery state, built once the WAV header is
/// parsed (see [`encode_stream`]). `piped` is declared first so an
/// error-path drop kills the encoders before their stdin pipes close.
struct Pipeline {
    piped: PipedSet,
    norm: sena_dsp::StreamResampler,
    split_main: sena_dsp::CrossoverStream,
    top: Option<TopChain>,
    lf_rs: sena_dsp::StreamResampler,
    tilt: Option<sena_dsp::FirStream>,
    mid44_rs: Option<sena_dsp::StreamResampler>,
    mid_wr: Option<RailSink>,
    hf_wr: RailSink,
    hf_codec: sena_core::HfCodec,
    /// LF is always the spooled s16 file: exhale's WAV reader lseeks, so
    /// no pipe.
    lf_wr: SpoolWriter,
    wd: std::path::PathBuf,
    dump_rails: bool,
}

impl Pipeline {
    /// Build the DSP chain and spawn the rail encoders (or open the rail
    /// dump files). Statement order is load-bearing: the tool checks run
    /// before any child is spawned, children are spawned in rail order,
    /// and the LF spool is created last.
    fn build(cfg: &EncoderConfig, ws: &wav::WavStream, st: &mut StageTimes) -> Result<Self, Error> {
        let lf_rate = cfg.profile.lf_rate();
        let norm = sena_dsp::StreamResampler::new(ws.rate().unwrap(), SAMPLE_RATE);
        let split_main = sena_dsp::CrossoverStream::new(cfg.profile.crossover_hz());
        // Three-track layout: split the 600 Hz-high band again at 12000 Hz
        // and carry the top band SSB-shifted down to baseband, resampled to
        // its stream rate: 16 kHz (Opus top) or 44.1 kHz (xHE top — the
        // 13 kHz-passband kernel covers the 0-8 kHz shifted band).
        let top = cfg.three_track.then(|| TopChain {
            split: sena_dsp::CrossoverStream::new(sena_core::HF_SPLIT_HZ),
            shift: sena_dsp::ShiftStream::down(sena_core::HF_SPLIT_HZ, SAMPLE_RATE),
            rs: match cfg.hf_codec {
                sena_core::HfCodec::Xhe => sena_dsp::StreamResampler::new_with_passband(
                    SAMPLE_RATE,
                    sena_core::XFI_MID_RATE,
                    sena_core::XFI_MID_RESAMPLE_PASS_HZ,
                ),
                sena_core::HfCodec::Opus => {
                    sena_dsp::StreamResampler::new(SAMPLE_RATE, sena_core::HF_TRACK_RATE)
                }
            },
        });
        let lf_rs = sena_dsp::StreamResampler::new(SAMPLE_RATE, lf_rate);
        let tilt = cfg
            .hf_tilt_pct
            .filter(|&s| s > 0.0)
            .map(|s| sena_dsp::FirStream::new(sena_dsp::tilt_taps(s)));
        // Delivery: exhale is fed a spooled s16 file (its WAV reader
        // seeks); the piped encoders stream f32 over stdin. The
        // SENAENC_DUMP_RAILS knob writes every rail to a workdir file
        // instead of spawning encoders (DSP regression tests).
        let dump_rails = std::env::var_os("SENAENC_DUMP_RAILS").is_some();
        let wd = choose_workdir(cfg, ws)?;
        let wd_path: &Path = &wd;
        let hf_rate = if cfg.three_track {
            sena_core::HF_TRACK_RATE
        } else {
            SAMPLE_RATE
        };
        let mut piped = PipedSet { children: Vec::new() };
        let mut mid_wr = None;
        let mut hf_wr = None;
        let mut mid44_rs = None;
        if !dump_rails {
            let pipe_cmds = build_codec_commands(cfg, wd_path, st)?;
            for (role, name, mut cmd) in pipe_cmds {
                let mut child = cmd
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .map_err(|e| {
                        Error::Subprocess(format!(
                            "cannot start encoder {name} ({}): {e}",
                            cmd.get_program().to_string_lossy()
                        ))
                    })?;
                let stdin = child.stdin.take().expect("piped stdin");
                piped.children.push(PipedChild { name, child });

                match role {
                    RailRole::Mid => {
                        mid_wr = Some(RailSink::Pipe(
                            name,
                            wav::PipeWavWriter::new(stdin, SAMPLE_RATE, 2, false)?,
                        ));
                    }

                    RailRole::MidXhe44 => {
                        mid44_rs = Some(sena_dsp::StreamResampler::new_with_passband(
                            SAMPLE_RATE,
                            sena_core::XFI_MID_RATE,
                            sena_core::XFI_MID_RESAMPLE_PASS_HZ,
                        ));
                        mid_wr = Some(RailSink::Pipe(
                            name,
                            wav::PipeWavWriter::new(stdin, sena_core::XFI_MID_RATE, 2, true)?,
                        ));
                    }

                    RailRole::Hf => {
                        hf_wr = Some(RailSink::Pipe(
                            name,
                            wav::PipeWavWriter::new(stdin, hf_rate, 2, false)?,
                        ));
                    }
                    RailRole::HfXhe44 => {
                        hf_wr = Some(RailSink::Pipe(
                            name,
                            wav::PipeWavWriter::new(stdin, sena_core::XFI_MID_RATE, 2, true)?,
                        ));
                    }
                }
            }
        } else {
            for role in rail_layout(cfg) {
                match role {
                    RailRole::Mid => {
                        mid_wr = Some(RailSink::File(wav::WavWriter::create(
                            &wd_path.join("mid.wav"),
                            SAMPLE_RATE,
                            32,
                            2,
                        )?));
                    }

                    RailRole::MidXhe44 => {
                        mid44_rs = Some(sena_dsp::StreamResampler::new_with_passband(
                            SAMPLE_RATE,
                            sena_core::XFI_MID_RATE,
                            sena_core::XFI_MID_RESAMPLE_PASS_HZ,
                        ));
                        mid_wr = Some(RailSink::File(wav::WavWriter::create(
                            &wd_path.join("mid44.wav"),
                            sena_core::XFI_MID_RATE,
                            32,
                            2,
                        )?));
                    }

                    RailRole::Hf => {
                        hf_wr = Some(RailSink::File(wav::WavWriter::create(
                            &wd_path.join("hf.wav"),
                            hf_rate,
                            32,
                            2,
                        )?));
                    }
                    RailRole::HfXhe44 => {
                        hf_wr = Some(RailSink::File(wav::WavWriter::create(
                            &wd_path.join("hf44.wav"),
                            sena_core::XFI_MID_RATE,
                            32,
                            2,
                        )?));
                    }
                }
            }
        }
        // exhale stays on s16: its internal domain is 24-bit, but its
        // rate-distortion operating point was tuned and validated with
        // s16 input — feeding s24 consistently cut VBR allocation
        // 11-16% and worsened the 24-144 Hz envelope sigma on the
        // golden clip and all three real tracks (measured, card 80);
        // keep the frozen encoder's input distribution.
        let lf_wr = SpoolWriter {
            w: wav::WavWriter::create(&wd_path.join("lf.wav"), lf_rate, 16, 2)?,
            spill_base: cfg.workdir_fallback.map(|p| p.to_path_buf()),
            cleanup: cfg.cleanup.clone(),
            spilled_to: None,
        };
        let hf_wr = hf_wr.expect("the rail layout always creates the hf sink");
        Ok(Pipeline {
            piped,
            norm,
            split_main,
            top,
            lf_rs,
            tilt,
            mid44_rs,
            mid_wr,
            hf_wr,
            hf_codec: cfg.hf_codec,
            lf_wr,
            wd,
            dump_rails,
        })
    }

    /// One chunk through normalize -> split -> branches -> writers. In the
    /// three-track layout the LF branch (downsample + LF write) and the HF
    /// branch (12000 Hz split + tilt + mid write + top-band
    /// shift/downsample/write) are independent and run concurrently; their
    /// stage timers overlap, so the SENAENC_TIME sum can exceed the wall
    /// time.
    fn push_chunk(&mut self, x: &[f64], ch: usize, st: &mut StageTimes) -> Result<(), Error> {
        st.begin();
        let n48 = self.norm.push(x, ch);
        end_tick(&mut st.t, &mut st.norm);
        st.begin();
        let (low, high) = self.split_main.push(&n48, ch);
        end_tick(&mut st.t, &mut st.split);

        if let Some(top) = self.top.as_mut() {
            // Three-track: the two band branches are independent; run them
            // concurrently so the p600 LF downsample overlaps the second
            // crossover and the mid/top writes.
            let Pipeline { lf_rs, lf_wr, tilt, mid44_rs, mid_wr, hf_wr, .. } = self;
            let mut lf_times = (0.0f64, 0.0f64); // (lf, write)
            let mut hf_times = (0.0f64, 0.0f64); // (split, write)
            let (ra, rb) = std::thread::scope(|s| {
                let ha = s.spawn(|| lf_branch(&low, ch, lf_rs, lf_wr, &mut lf_times));
                let hb = s.spawn(|| {
                    hf_branch3(
                        &high,
                        ch,
                        &mut top.split,
                        &mut top.shift,
                        &mut top.rs,
                        tilt,
                        mid_wr.as_mut().unwrap(),
                        mid44_rs.as_mut(),
                        hf_wr,
                        &mut hf_times,
                    )
                });
                (
                    ha.join().unwrap_or_else(|e| std::panic::resume_unwind(e)),
                    hb.join().unwrap_or_else(|e| std::panic::resume_unwind(e)),
                )
            });
            st.lf += lf_times.0;
            st.write += lf_times.1 + hf_times.1;
            st.split += hf_times.0;
            ra?;
            rb?;
            return Ok(());
        }

        if !low.is_empty() {
            let low_p: Vec<f64> = pre_gained(&low);
            st.begin();
            let lf16 = self.lf_rs.push(&low_p, ch);
            end_tick(&mut st.t, &mut st.lf);
            st.begin();
            self.lf_wr.push(&lf16)?;
            end_tick(&mut st.t, &mut st.write);
        }
        let high = if let Some(t) = self.tilt.as_mut() {
            t.push(&high, ch)
        } else {
            high
        };
        if !high.is_empty() {
            let high_p: Vec<f64> = pre_gained(&high);
            st.begin();
            self.hf_wr.push(&high_p)?;
            end_tick(&mut st.t, &mut st.write);
        }
        Ok(())
    }

    /// Flush the pipeline latencies (normalize tail -> split tails -> LF
    /// tail) and push the final rail frames. Returns the playable frame
    /// count.
    fn flush_tail(&mut self, ch: usize, st: &mut StageTimes) -> Result<usize, Error> {
        let playable = self.norm.output_frames_total();
        st.begin();
        let n48_tail = self.norm.finish(ch);
        end_tick(&mut st.t, &mut st.norm);
        st.begin();
        let (low_a, high_a) = self.split_main.push(&n48_tail, ch);
        let (low_b, high_b) = self.split_main.finish(ch);
        end_tick(&mut st.t, &mut st.split);

        let low_p: Vec<f64> = pre_gained(low_a.iter().chain(low_b.iter()));
        st.begin();
        let lf16 = self.lf_rs.push(&low_p, ch);
        end_tick(&mut st.t, &mut st.lf);
        self.lf_wr.push(&lf16)?;
        st.begin();
        let lf16b = self.lf_rs.finish(ch);
        end_tick(&mut st.t, &mut st.lf);
        self.lf_wr.push(&lf16b)?;

        let high_in: Vec<f64> = high_a.iter().chain(high_b.iter()).cloned().collect();
        if let Some(top) = self.top.as_mut() {
            // Three-track tail: split the high band at 12000 Hz, tilt and
            // pad the mid band; shift the top band to baseband, resample
            // to 16 kHz and pad.
            st.begin();
            let (mid_a, top_a) = top.split.push(&high_in, ch);
            let (mid_b, top_b) = top.split.finish(ch);
            let top_s = {
                let mut v = top.shift.push(&top_a, ch);
                v.extend(top.shift.push(&top_b, ch));
                v.extend(top.shift.finish(ch));
                v
            };
            // xHE top: same delivery contract as the xfi mid rail — pad
            // zeros until the FhG finalize floor (0.4 s) and full playable
            // coverage (post warmup trim + AU quantization + seek guard)
            // are met, before the resampler tail is flushed.
            let top16 = {
                let mut v = top.rs.push(&top_s, ch);
                if self.hf_codec == sena_core::HfCodec::Xhe {
                    let min_frames = (sena_core::XFI_MID_RATE as usize * 2 / 5).max(
                        playable * sena_core::XFI_MID_RATE as usize / SAMPLE_RATE as usize
                            + 4 * sena_core::XHE_WARMUP_CORE,
                    );
                    let mut pad_iters = 0;
                    while top.rs.output_frames_total() < min_frames {
                        pad_iters += 1;
                        if pad_iters > MAX_PAD_ITERS {
                            return Err(Error::Format(
                                "top 44.1k resampler produced no output while padding".into(),
                            ));
                        }
                        v.extend(top.rs.push(&vec![0.0f64; PAD_QUANTUM_FRAMES * ch], ch));
                    }
                }
                v.extend(top.rs.finish(ch));
                v
            };
            end_tick(&mut st.t, &mut st.split);

            let mid_in: Vec<f64> = mid_a.iter().chain(mid_b.iter()).cloned().collect();
            let mid_f = if let Some(t) = self.tilt.as_mut() {
                let mut v = t.push(&mid_in, ch);
                v.extend(t.finish(ch));
                v
            } else {
                mid_in
            };
            let mid_p: Vec<f64> = pre_gained(&mid_f);
            let top_p: Vec<f64> = pre_gained(&top16);

            st.begin();
            if let Some(rs) = self.mid44_rs.as_mut() {
                // xfi mid: flush the 44.1k resampler tail, then zero-pad
                // until (a) the FhG finalize floor (one 0.4 s BS.1770
                // gating block) and (b) full playable coverage after the
                // 1024-sample warmup trim + AU quantization are both met
                // (the streaming finish tail alone was 7.6 ms short of the
                // playable end on the golden clip).
                let m = rs.push(&mid_p, ch);
                self.mid_wr.as_mut().unwrap().push(&m)?;
                // + 4 warmup frames of slack: the coverage gate needs
                // playable + warmup + AU quantization, and a seek into the
                // tail additionally needs the decode-side resampler guard
                // (a 2-AU pad left the tail 5.5 ms short on tail seeks).
                let min_frames = (sena_core::XFI_MID_RATE as usize * 2 / 5).max(
                    playable * sena_core::XFI_MID_RATE as usize / SAMPLE_RATE as usize
                        + 4 * sena_core::XHE_WARMUP_CORE,
                );
                let mut pad_iters = 0;
                while rs.output_frames_total() < min_frames {
                    pad_iters += 1;
                    if pad_iters > MAX_PAD_ITERS {
                        return Err(Error::Format(
                            "mid 44.1k resampler produced no output while padding".into(),
                        ));
                    }
                    let m = rs.push(&vec![0.0f64; PAD_QUANTUM_FRAMES * ch], ch);
                    self.mid_wr.as_mut().unwrap().push(&m)?;
                }

                let m = rs.finish(ch);
                self.mid_wr.as_mut().unwrap().push(&m)?;
            } else {
                self.mid_wr.as_mut().unwrap().push(&mid_p)?;
            }
            self.hf_wr.push(&top_p)?;
            end_tick(&mut st.t, &mut st.write);
        } else {
            let high_f = if let Some(t) = self.tilt.as_mut() {
                let mut v = t.push(&high_in, ch);
                v.extend(t.finish(ch));
                v
            } else {
                high_in
            };
            let high_p: Vec<f64> = pre_gained(&high_f);
            st.begin();
            self.hf_wr.push(&high_p)?;
            end_tick(&mut st.t, &mut st.write);
        }
        Ok(playable)
    }
}

/// Full encode pipeline over a streaming input (stdin-safe).
///
/// The WAV is parsed incrementally and pushed through the streaming DSP
/// ([`Pipeline`]); the WAV is read in PIPELINE_CHUNK_FRAMES chunks once the
/// header gives the channel count.
pub fn encode_stream<R: std::io::Read>(
    cfg: &EncoderConfig,
    input: &mut R,
    output: &Path,
) -> Result<EncodeOutcome, Error> {
    let mut st = StageTimes::new();
    let mut ws = wav::WavStream::new();
    let mut pipe: Option<Pipeline> = None;
    let mut ch = 2usize;
    let mut byte_buf = [0u8; READ_CHUNK_BYTES];
    let mut eof = false;

    loop {
        if pipe.is_none() {
            if !ws.parsed() {
                if eof {
                    return Err(Error::Format("no WAV header".into()));
                }
                let n = input.read(&mut byte_buf).map_err(Error::Io)?;
                if n == 0 {
                    eof = true;
                    continue;
                }
                ws.feed(&byte_buf[..n])?;
                continue;
            }
            // header is parsed: build the pipeline
            ch = ws.channels().unwrap();
            if ch != 2 {
                return Err(Error::Format(format!("expected stereo input, got {ch} ch")));
            }
            pipe = Some(Pipeline::build(cfg, &ws, &mut st)?);
            continue;
        }

        match ws.take(PIPELINE_CHUNK_FRAMES)? {
            Some(x) => {
                let p = pipe.as_mut().expect("pipeline built above");
                p.push_chunk(&x, ch, &mut st)?;
            }
            None => {
                if eof {
                    break;
                }
                let n = input.read(&mut byte_buf).map_err(Error::Io)?;
                if n == 0 {
                    eof = true;
                } else {
                    ws.feed(&byte_buf[..n])?;
                }
            }
        }
    }
    ws.finish()?;

    let mut pipe = pipe.expect("the loop cannot exit before the pipeline is built");
    let playable = pipe.flush_tail(ch, &mut st)?;

    // Finish the sinks in rail order (LF spool, mid, top), then reap the
    // piped encoders. The LF spool may have been relocated mid-pass; exhale
    // is built with the final path, and the fallback dir joins the cleanup
    // set.
    let Pipeline { piped, lf_wr, mid_wr, hf_wr, wd, dump_rails, .. } = pipe;
    let spill_dir = lf_wr.spilled_to.clone();
    let lf_wav = lf_wr.w.path().to_path_buf();
    lf_wr.finish()?;
    if let Some(mw) = mid_wr {
        mw.finish()?;
    }
    hf_wr.finish()?;
    let outcome = EncodeOutcome { workdir: wd.clone(), spill_dir };
    if dump_rails {
        // DSP-only mode (SENAENC_DUMP_RAILS): rails are workdir files,
        // no encoders were spawned.
        return Ok(outcome);
    }
    st.begin();
    let reap = piped.reap();
    end_tick(&mut st.t, &mut st.opus);
    reap?;
    let result = finish_codecs(cfg, &wd, playable, output, &mut st, &lf_wav);
    if result.is_ok() {
        st.report(playable);
    }
    result.map(|_| outcome)
}

/// LF branch: PRE_GAIN -> LF downsample -> s16 WAV write. Returns its
/// (lf, write) elapsed times through `t`.
fn lf_branch(
    low: &[f64],
    ch: usize,
    lf_rs: &mut sena_dsp::StreamResampler,
    lf_wr: &mut SpoolWriter,
    t: &mut (f64, f64),
) -> Result<(), Error> {
    if low.is_empty() {
        return Ok(());
    }
    let low_p: Vec<f64> = pre_gained(low);
    let t0 = std::time::Instant::now();
    let lf16 = lf_rs.push(&low_p, ch);
    t.0 += t0.elapsed().as_secs_f64();
    let t1 = std::time::Instant::now();
    let r = lf_wr.push(&lf16);
    t.1 += t1.elapsed().as_secs_f64();
    r
}

/// Three-track HF branch: 12000 Hz split (the tilt, if any, shapes the mid
/// band only) -> PRE_GAIN -> mid write; the top band is SSB-shifted down to
/// baseband and resampled to 16 kHz before its (PRE_GAIN) write. Returns
/// its (split, write) elapsed times through `t`.
#[allow(clippy::too_many_arguments)]
fn hf_branch3(
    high: &[f64],
    ch: usize,
    split_hf: &mut sena_dsp::CrossoverStream,
    shift_hf: &mut sena_dsp::ShiftStream,
    top_rs: &mut sena_dsp::StreamResampler,
    tilt: &mut Option<sena_dsp::FirStream>,
    mid_wr: &mut RailSink,
    mut mid44_rs: Option<&mut sena_dsp::StreamResampler>,
    hf_wr: &mut RailSink,
    t: &mut (f64, f64),
) -> Result<(), Error> {
    let t0 = std::time::Instant::now();
    let (mid, top) = split_hf.push(high, ch);
    let top16 = if top.is_empty() {
        top
    } else {
        let shifted = shift_hf.push(&top, ch);
        top_rs.push(&shifted, ch)
    };
    t.0 += t0.elapsed().as_secs_f64();
    let mut err: Option<Error> = None;
    if !mid.is_empty() {
        let mid = if let Some(tl) = tilt.as_mut() {
            tl.push(&mid, ch)
        } else {
            mid
        };
        let mid_p: Vec<f64> = pre_gained(&mid);
        let t1 = std::time::Instant::now();
        // xfi mid: resample 48k -> 44.1k inline (the sink clamps to [-1,1]
        // for the FhG finalize contract); opus mid: f32 as-is.
        let r = match mid44_rs.as_mut() {
            Some(rs) => mid_wr.push(&rs.push(&mid_p, ch)),
            None => mid_wr.push(&mid_p),
        };
        if let Err(e) = r {
            err = Some(e);
        }
        t.1 += t1.elapsed().as_secs_f64();
    }
    if err.is_none() && !top16.is_empty() {
        let top_p: Vec<f64> = pre_gained(&top16);
        let t2 = std::time::Instant::now();
        if let Err(e) = hf_wr.push(&top_p) {
            err = Some(e);
        }
        t.1 += t2.elapsed().as_secs_f64();
    }
    match err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Build every encoder command (and run the cheap tool checks) up front,
/// so a broken toolchain fails before the DSP pass. exhale runs after the
/// pass on the spooled s24 lf.wav; the piped encoders (opusenc / ezcd-cli)
/// are spawned at pipeline init and stream their rail over stdin.
fn build_codec_commands(
    cfg: &EncoderConfig,
    wd: &Path,
    st: &mut StageTimes,
) -> Result<Vec<(RailRole, &'static str, Command)>, Error> {
    st.begin();
    let ck_err: Option<Error> = check_exhale(cfg.exhale)
        .and_then(|_| {
            if cfg.use_senav {
                check_senav_version(cfg.opusenc)
            } else {
                check_version(cfg.opusenc, None, "opusenc")
            }
        })
        .err();
    end_tick(&mut st.t, &mut st.check);
    if let Some(e) = ck_err {
        return Err(e);
    }

    let hf_ogg = wd.join("hf.opus");
    let mid_ogg = wd.join("mid.opus");
    let mid_m4a = wd.join("mid.m4a");
    let xhe_mid = cfg.mid_codec == sena_core::MidCodec::Xhe;
    if xhe_mid && !cfg.three_track {
        return Err(Error::Format("xhe mid requires the three-track layout".into()));
    }
    let (mid_kbps, rail_kbps) = if cfg.three_track {
        if xhe_mid {
            let (_, mid, hf) = sena_core::account3_xfi(cfg.total_kbps, cfg.profile)
                .ok_or_else(|| Error::Format("total bitrate outside the xfi 256-288k range".into()))?;
            (mid, hf)
        } else {
            let (_, mid, hf) = sena_core::account3(cfg.total_kbps, cfg.profile)
                .ok_or_else(|| Error::Format("total bitrate below the three-track minimum".into()))?;
            (mid, hf)
        }
    } else {
        let (_, mid) = sena_core::account(cfg.total_kbps, cfg.profile)
            .ok_or_else(|| Error::Format("total bitrate below Sena minimum".into()))?;
        (mid, 0)
    };
    let mut pipes: Vec<(RailRole, &'static str, Command)> = Vec::new();
    let mut opus_mid = opus_cmd(cfg, mid_kbps, cfg.topband_kbps);
    if cfg.three_track {
        if xhe_mid {
            let ezcd_path = cfg.ezcd_cli.ok_or_else(|| {
                Error::Format("xfi profile requires ezcd-cli (next to senaenc or on PATH)".into())
            })?;
            let win_host = needs_windows_paths(ezcd_path);
            let mid_m4a_arg = cli_path(&mid_m4a, win_host)?;
            let mut c = Command::new(ezcd_path);
            c.arg("encode")
                .arg("--encoder")
                .arg("xhe-aac-fhg-iis")
                .arg("--mode")
                .arg("cbr")
                .arg("--bitrate")
                .arg(sena_core::XFI_MID_KBPS.to_string())
                .arg("--sample-rate")
                .arg(sena_core::XFI_MID_RATE.to_string())
                .arg("--resample")
                .arg("off")
                .arg("-o")
                .arg(&mid_m4a_arg)
                .arg("-");
            pipes.push((RailRole::MidXhe44, "ezcd-cli(mid)", c));
        } else {
            opus_mid.arg("-").arg(&mid_ogg);
            pipes.push((RailRole::Mid, "opusenc(mid)", opus_mid));
        }
    } else {
        // Two-track layout: the Hf-role rail carries the sole Opus track
        // (the 300/600 Hz-high band), written to hf.opus.
        opus_mid.arg("-").arg(&hf_ogg);
        pipes.push((RailRole::Hf, "opusenc(mid)", opus_mid));
    }
    // Three-track top-band encode: Opus pipe (tiered hf nominal 96-127k;
    // the topband-stereo knob is never armed for it) or, at the xfi 288k
    // point, the FhG-IIS CBR 128k @44.1 kHz pipe (same invocation shape as
    // the xfi mid).
    if cfg.three_track {
        match cfg.hf_codec {
            sena_core::HfCodec::Opus => {
                let mut opus_hf = opus_cmd(cfg, rail_kbps, None);
                opus_hf.arg("-").arg(&hf_ogg);
                pipes.push((RailRole::Hf, "opusenc(hf)", opus_hf));
            }
            sena_core::HfCodec::Xhe => {
                let ezcd_path = cfg.ezcd_cli.ok_or_else(|| {
                    Error::Format("xHE top band requires ezcd-cli".into())
                })?;
                let win_host = needs_windows_paths(ezcd_path);
                let hf_m4a = wd.join("hf.m4a");
                let hf_m4a_arg = cli_path(&hf_m4a, win_host)?;
                let mut c = Command::new(ezcd_path);
                c.arg("encode")
                    .arg("--encoder")
                    .arg("xhe-aac-fhg-iis")
                    .arg("--mode")
                    .arg("cbr")
                    .arg("--bitrate")
                    .arg(rail_kbps.to_string())
                    .arg("--sample-rate")
                    .arg(sena_core::XFI_MID_RATE.to_string())
                    .arg("--resample")
                    .arg("off")
                    .arg("-o")
                    .arg(&hf_m4a_arg)
                    .arg("-");
                pipes.push((RailRole::HfXhe44, "ezcd-cli(hf)", c));
            }
        }
    }
    Ok(pipes)
}

/// opusenc invocation shared by the mid and top-band encodes.
fn opus_cmd(cfg: &EncoderConfig, kbps: u32, topband_kbps: Option<u32>) -> Command {
    let mut c = Command::new(cfg.opusenc);
    c.arg("--quiet")
        .arg("--bitrate")
        .arg(kbps.to_string())
        .arg("--vbr");
    if cfg.use_senav {
        apply_senav_env(&mut c, topband_kbps);
    }
    c
}

/// Top-band stream: Opus packets (with OpusHead and pre-skip) or xHE-AAC
/// AUs (with AudioSpecificConfig).
enum TopStream {
    Opus { head: Vec<u8>, preskip: u16, packets: Vec<Vec<u8>> },
    Xhe { asc: Vec<u8>, aus: Vec<Vec<u8>> },
}

/// Per-layer codec streams extracted from the encoder outputs.
struct Extracted {
    mid_head: Vec<u8>,
    mid_stream: MidStream,
    top_stream: TopStream,
    lf_asc: Vec<u8>,
    lf_aus: Vec<Vec<u8>>,
    lf_stream_rate: u32,
}

/// Read the encoder outputs and split them into per-layer streams.
fn extract_streams(cfg: &EncoderConfig, wd: &Path) -> Result<Extracted, Error> {
    let hf_ogg = wd.join("hf.opus");
    let mid_ogg = wd.join("mid.opus");
    let mid_m4a = wd.join("mid.m4a");
    let lf_m4a = wd.join("lf.m4a");
    let xhe_mid = cfg.mid_codec == sena_core::MidCodec::Xhe;
    let xhe_hf = cfg.hf_codec == sena_core::HfCodec::Xhe;
    let (mid_head, mid_stream, top_stream) = if cfg.three_track {
        let top_stream = if xhe_hf {
            let hf_m4a = wd.join("hf.m4a");
            let (asc, aus) = extract_m4a(&hf_m4a)?;
            let rate = m4a_stream_rate(&hf_m4a)?;
            if rate != sena_core::XFI_MID_RATE {
                return Err(Error::Format(format!(
                    "ezcd-cli top stream rate {rate}, expected {}",
                    sena_core::XFI_MID_RATE
                )));
            }
            TopStream::Xhe { asc, aus }
        } else {
            let top_bytes = std::fs::read(&hf_ogg)?;
            let (head, preskip, packets) = ogg::extract_opus(&top_bytes)?;
            TopStream::Opus { head, preskip, packets }
        };
        if xhe_mid {
            let (asc, aus) = extract_m4a(&mid_m4a)?;
            let rate = m4a_stream_rate(&mid_m4a)?;
            if rate != sena_core::XFI_MID_RATE {
                return Err(Error::Format(format!(
                    "ezcd-cli mid stream rate {rate}, expected {}",
                    sena_core::XFI_MID_RATE
                )));
            }
            (asc, MidStream::Xhe(aus), top_stream)
        } else {
            let mid_bytes = std::fs::read(&mid_ogg)?;
            let (mid_head, _mid_preskip, mid_packets) = ogg::extract_opus(&mid_bytes)?;
            (mid_head, MidStream::Opus(mid_packets), top_stream)
        }
    } else {
        // Two-track: the Hf-role rail carries the sole Opus track.
        let ogg_bytes = std::fs::read(&hf_ogg)?;
        let (head, _preskip, packets) = ogg::extract_opus(&ogg_bytes)?;
        (head, MidStream::Opus(packets), TopStream::Opus { head: Vec::new(), preskip: 0, packets: Vec::new() })
    };
    let (lf_asc, lf_aus) = extract_m4a(&lf_m4a)?;
    let lf_stream_rate = m4a_stream_rate(&lf_m4a)?;
    Ok(Extracted {
        mid_head,
        mid_stream,
        top_stream,
        lf_asc,
        lf_aus,
        lf_stream_rate,
    })
}

/// Assemble the composite ES blocks on the 20 ms grid, hash the audio
/// content (v3: the zeroed-hash SENA-INIT blob plus the block payloads)
/// and write the container.
fn mux_container(
    cfg: &EncoderConfig,
    x: &Extracted,
    playable: usize,
    output: &Path,
) -> Result<(), Error> {
    // Grid length: the Opus mid packet count (Opus mid) or the block count
    // covering pre-skip + playable (xHE mid, whose AUs do not align to the
    // 20 ms grid and are placed by playable end like LF).
    let n_blocks = match &x.mid_stream {
        MidStream::Opus(packets) => packets.len() as u64,
        MidStream::Xhe(_) => es::block_count(playable as u64, x.top_stream.top_preskip() as u32),
    };

    // LF AU k lands in the block containing its last playable sample
    // (by-end placement, clamped to the final block at EOS).
    let mut lf_by_block: std::collections::BTreeMap<u64, Vec<(u32, &[u8])>> =
        Default::default();
    for (k, au) in x.lf_aus.iter().enumerate() {
        lf_by_block
            .entry(es::lf_block(k as u64, x.lf_stream_rate, n_blocks))
            .or_default()
            .push((k as u32, au));
    }
    // xHE mid AUs land by the same by-end rule at 44.1 kHz.
    let mut mid_by_block: std::collections::BTreeMap<u64, Vec<(u32, &[u8])>> =
        Default::default();
    if let MidStream::Xhe(aus) = &x.mid_stream {
        // Hard coverage gate: after the 1024-sample warmup trim the AUs
        // must still reach the end of the playable range (the inline
        // resampler tail plus the 0.4 s floor exist to guarantee this).
        let n = aus.len() as u64;
        let covered = es::xhe_au_end48(n, sena_core::XFI_MID_RATE).saturating_sub(
            sena_core::XHE_WARMUP_CORE as u64 * sena_core::SAMPLE_RATE as u64
                / sena_core::XFI_MID_RATE as u64,
        );
        if covered < playable as u64 {
            return Err(Error::Format(format!(
                "xHE mid covers {covered} of {playable} playable frames ({} AUs)",
                aus.len()
            )));
        }
        for (k, au) in aus.iter().enumerate() {
            mid_by_block
                .entry(es::xhe_block(k as u64, sena_core::XFI_MID_RATE, n_blocks))
                .or_default()
                .push((k as u32, au));
        }
    }
    // Top layer placement: Opus packet j lands in block j (clamped); xHE
    // AUs (23.22 ms at 44.1 kHz) follow the by-end rule with absolute AU
    // indices, plus the same hard coverage gate as the xHE mid.
    let mut top_by_block: std::collections::BTreeMap<u64, Vec<(u32, &[u8])>> =
        Default::default();
    match &x.top_stream {
        TopStream::Opus { packets, .. } => {
            for (j, p) in packets.iter().enumerate() {
                top_by_block
                    .entry(es::hf_block(j as u64, n_blocks))
                    .or_default()
                    .push((0, p));
            }
        }
        TopStream::Xhe { aus, .. } => {
            let n = aus.len() as u64;
            let covered = es::xhe_au_end48(n, sena_core::XFI_MID_RATE).saturating_sub(
                sena_core::XHE_WARMUP_CORE as u64 * sena_core::SAMPLE_RATE as u64
                    / sena_core::XFI_MID_RATE as u64,
            );
            if covered < playable as u64 {
                return Err(Error::Format(format!(
                    "xHE top covers {covered} of {playable} playable frames ({} AUs)",
                    aus.len()
                )));
            }
            for (k, au) in aus.iter().enumerate() {
                top_by_block
                    .entry(es::xhe_block(k as u64, sena_core::XFI_MID_RATE, n_blocks))
                    .or_default()
                    .push((k as u32, au));
            }
        }
    }

    let mut payloads: Vec<Vec<u8>> = Vec::with_capacity(n_blocks as usize);
    for i in 0..n_blocks {
        let lf = match lf_by_block.remove(&i) {
            Some(v) => {
                let au_index = v[0].0;
                let aus: Vec<&[u8]> = v.iter().map(|e| e.1).collect();
                (au_index, aus)
            }
            None => (0, Vec::new()),
        };
        let top = top_by_block.remove(&i).unwrap_or_default();
        let top: (u32, Vec<&[u8]>) = (
            top.first().map_or(0, |e| e.0),
            top.iter().map(|e| e.1).collect(),
        );
        let mid = match &x.mid_stream {
            MidStream::Opus(packets) => es::MidSection::Opus(&packets[i as usize]),
            MidStream::Xhe(_) => match mid_by_block.remove(&i) {
                Some(v) => es::MidSection::Xhe {
                    au_index: v[0].0,
                    aus: v.iter().map(|e| e.1).collect(),
                },
                None => es::MidSection::Xhe {
                    au_index: 0,
                    aus: Vec::new(),
                },
            },
        };
        payloads.push(es::encode_block(
            &es::BlockParts {
                mid,
                lf: (lf.0, &lf.1),
                hf: (top.0, &top.1),
            },
            cfg.hf_codec,
        ));
    }
    if !lf_by_block.is_empty() || !top_by_block.is_empty() || !mid_by_block.is_empty() {
        return Err(Error::Format("layer packet outside the mid grid".into()));
    }

    let mut init = es::Init {
        profile: cfg.profile,
        three_track: cfg.three_track,
        mid_codec: cfg.mid_codec,
        hf_codec: if cfg.three_track { cfg.hf_codec } else { sena_core::HfCodec::Opus },
        lf_rate: x.lf_stream_rate,
        playable_samples: playable as u64,
        audio_sha256: [0; 32],
        lf_asc: x.lf_asc.clone(),
        mid_head: x.mid_head.clone(),
        hf_head: if cfg.three_track {
            Some(match &x.top_stream {
                TopStream::Opus { head, .. } => head.clone(),
                TopStream::Xhe { asc, .. } => asc.clone(),
            })
        } else {
            None
        },
    };
    init.audio_sha256 = encoded_audio_sha256_es(&init.encode(), &payloads);

    let total_bytes: u64 = payloads.iter().map(|p| p.len() as u64).sum();
    let playable_ns = (playable as u64) * 1_000_000_000 / SAMPLE_RATE as u64;
    let avg_bps = ((total_bytes as u128 * 1_000_000_000).div_ceil(playable_ns as u128)) as u32;
    let codec_private = es::build_codec_private(&init, avg_bps);

    let blocks: Vec<SenaBlock> = payloads
        .into_iter()
        .enumerate()
        .map(|(i, data)| SenaBlock {
            t_ns: i as u64 * es::BLOCK_NS,
            data,
        })
        .collect();
    let mka_bytes = mka::write_sena(
        &codec_private,
        &blocks,
        &es::codec_name(cfg.profile, cfg.three_track, cfg.mid_codec, cfg.hf_codec),
        MKA_CLUSTER_MS,
        playable_ns,
    );
    std::fs::write(output, mka_bytes)?;
    Ok(())
}

/// Run exhale on the spooled LF WAV, extract all streams and mux the
/// container. The piped encoders were reaped by the caller, so their
/// outputs are already complete on disk.
fn finish_codecs(
    cfg: &EncoderConfig,
    wd: &Path,
    playable: usize,
    output: &Path,
    st: &mut StageTimes,
    lf_wav: &Path,
) -> Result<(), Error> {
    let lf_m4a = wd.join("lf.m4a");
    // exhale's input path is resolved only now: the LF spool may have been
    // relocated to the fallback volume by an ENOSPC during the pass.
    let mut ex = Command::new(cfg.exhale);
    ex.arg(cfg.profile.xhe_preset().to_string())
        .arg(lf_wav)
        .arg(&lf_m4a);
    let t0 = std::time::Instant::now();
    let mut cmds: Vec<(&str, &mut Command)> = vec![("exhale", &mut ex)];
    let codec_err = run_parallel(&mut cmds);
    if st.on {
        let d = t0.elapsed().as_secs_f64();
        st.exhale += d;
        st.opus += d;
        if cfg.three_track {
            st.opus_hf += d;
        }
    }
    if let Some(e) = codec_err {
        return Err(e);
    }

    // --- extract packets (timed as the mux stage; the block assembly tail
    // after it is tiny and rides the same accumulator) ---
    st.begin();
    let extracted = extract_streams(cfg, wd);
    end_tick(&mut st.t, &mut st.mux);
    mux_container(cfg, &extracted?, playable, output)
}

/// Extracted mid-layer stream: Opus packets (the block grid) or xHE-AAC
/// AUs (by-end placement at 44.1 kHz).
enum MidStream {
    Opus(Vec<Vec<u8>>),
    Xhe(Vec<Vec<u8>>),
}

impl TopStream {
    /// Opus pre-skip of the top stream (0 for xHE, which has no pre-skip).
    fn top_preskip(&self) -> u16 {
        match self {
            TopStream::Opus { preskip, .. } => *preskip,
            TopStream::Xhe { .. } => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoded_audio_sha256_es_known_vector_and_determinism() {
        // Expected value computed independently in Python with the same
        // length-prefixed layout (domain + INIT blob + blocks).
        let init = b"fake-init";
        let blocks = vec![vec![1, 2, 3], vec![4, 5]];
        let h = encoded_audio_sha256_es(init, &blocks);
        let hex: String = h.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "9f0c506f8220b88d8d5a243a0a62fdf9a8a881161d706cae7dafc0f9f5749475"
        );
        assert_eq!(encoded_audio_sha256_es(init, &blocks), h, "must be deterministic");

        // Different payloads must produce different hashes.
        let other = vec![vec![1, 2, 3], vec![4, 6]];
        assert_ne!(encoded_audio_sha256_es(init, &other), h);
        let other_init = b"fake-init2";
        assert_ne!(encoded_audio_sha256_es(other_init, &blocks), h);

        // Length-prefix collisions are impossible by construction: a block
        // split changes the block count and payload bytes.
        let split = vec![vec![1, 2], vec![3, 4, 5]];
        assert_ne!(encoded_audio_sha256_es(init, &split), h);
    }

    fn synth_wav(secs: usize, rate: u32, stream_len: bool) -> Vec<u8> {
        // Stereo s16 PCM: RIFF/WAVE/fmt/data with a streaming data length
        // (0xFFFFFFFF) when `stream_len`, else the real length.
        let frames = secs * rate as usize;
        let data_len = frames * 4usize;
        let mut w = Vec::new();
        w.extend_from_slice(b"RIFF");
        w.extend_from_slice(&((36 + data_len) as u32).to_le_bytes());
        w.extend_from_slice(b"WAVEfmt ");
        w.extend_from_slice(&16u32.to_le_bytes());
        w.extend_from_slice(&1u16.to_le_bytes()); // PCM
        w.extend_from_slice(&2u16.to_le_bytes()); // stereo
        w.extend_from_slice(&rate.to_le_bytes());
        w.extend_from_slice(&(rate * 4).to_le_bytes());
        w.extend_from_slice(&4u16.to_le_bytes());
        w.extend_from_slice(&16u16.to_le_bytes());
        w.extend_from_slice(b"data");
        w.extend_from_slice(
            &(if stream_len {
                0xFFFF_FFFFu32
            } else {
                data_len as u32
            })
            .to_le_bytes(),
        );
        for i in 0..frames {
            let t = i as f64 / rate as f64;
            let l = (2.0 * std::f64::consts::PI * 220.0 * t).sin() as i16;
            let r = (0.6 * (2.0 * std::f64::consts::PI * 440.0 * t).sin()) as i16;
            w.extend_from_slice(&l.to_le_bytes());
            w.extend_from_slice(&r.to_le_bytes());
        }
        w
    }

    #[test]
    fn wav_stream_matches_batch_reader() {
        for stream_len in [false, true] {
            let bytes = synth_wav(3, 44100, stream_len);
            let (batch, ch, rate) = wav::read_f64_bytes(&bytes).unwrap();
            // feed in odd-sized chunks
            let mut ws = wav::WavStream::new();
            for chunk in bytes.chunks(997) {
                ws.feed(chunk).unwrap();
            }
            ws.finish().unwrap();
            assert_eq!((ch, rate), (2, 44100));
            let mut got = Vec::new();
            loop {
                match ws.take(4096).unwrap() {
                    Some(x) => got.extend(x),
                    None => break,
                }
            }
            assert_eq!(got.len(), batch.len());
            let maxe = got
                .iter()
                .zip(batch.iter())
                .map(|(a, b)| (a - b).abs())
                .fold(0.0, f64::max);
            assert!(maxe < 1e-9, "stream reader vs batch error {maxe}");
        }
    }

    #[test]
    fn wav_stream_ignores_trailing_bytes_after_declared_data() {
        // foobar-style pipes: the header declares the real data size; bytes
        // after the data chunk (metadata/padding) must never become samples.
        let mut bytes = synth_wav(3, 44100, false);
        let declared = bytes.len() - 44;
        bytes.extend_from_slice(b"LIST\x00\x00\x00\x80"); // fake trailing junk
        bytes.extend_from_slice(&[0u8; 100]);
        let (batch, _, _) = wav::read_f64_bytes(&bytes).unwrap();
        let mut ws = wav::WavStream::new();
        for chunk in bytes.chunks(1800) {
            ws.feed(chunk).unwrap();
        }
        ws.finish().unwrap();
        let mut got = Vec::new();
        loop {
            match ws.take(4096).unwrap() {
                Some(x) => got.extend(x),
                None => break,
            }
        }
        assert_eq!(got.len(), batch.len());
        assert_eq!(
            got.len() / 2 * 4,
            declared,
            "decoded bytes must equal the declared data size"
        );
    }

    /// Config shared by the DSP-mode streaming tests: nonexistent codec
    /// paths (SENAENC_DUMP_RAILS keeps them from ever being spawned).
    fn test_cfg(wd: &Path, total_kbps: u32, three_track: bool, hf_tilt_pct: Option<f64>) -> EncoderConfig<'_> {
        EncoderConfig {
            profile: Profile::At300,
            total_kbps,
            use_senav: false,
            three_track,
            mid_codec: sena_core::MidCodec::Opus,
            topband_kbps: None,
            exhale: "/nonexistent/exhale",
            opusenc: "/nonexistent/opusenc",
            ezcd_cli: None,
            workdir: wd,
            workdir_fallback: None,
            cleanup: None,
            hf_tilt_pct,
        }
    }

    #[test]
    fn stream_dsp_matches_batch_dsp() {
        // Drive encode_stream far enough to write the temp band WAVs (the
        // codec step then fails on the fake exhale path), and compare those
        // against the batch DSP written by hand.
        let src = synth_wav(3, 44100, true);
        let wd = std::env::temp_dir().join(format!("sena-enc-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&wd);
        std::fs::create_dir_all(&wd).unwrap();
        let cfg = test_cfg(&wd, 160, false, None);
        // DSP-only mode: rails land as workdir files, no encoders spawned.
        unsafe { std::env::set_var("SENAENC_DUMP_RAILS", "1") };
        let mut bytes = &src[..];
        let out = wd.join("out.sena");
        let outcome = encode_stream(&cfg, &mut bytes, &out).unwrap();
        let wdp = outcome.workdir.as_path();

        // Reference: the same DSP via the batch functions.
        let (x, ch, in_rate) = wav::read_f64_bytes(&src).unwrap();
        assert_eq!(ch, 2);
        let x48 = sena_dsp::Resampler::new(in_rate, SAMPLE_RATE).process(&x, ch);
        let (low, high) = sena_dsp::split(&x48, 2, 300.0);
        let low_p: Vec<f64> = low.iter().map(|v| v * PRE_GAIN).collect();
        let low16 = sena_dsp::Resampler::new(SAMPLE_RATE, 16000).process(&low_p, 2);
        let high_p: Vec<f64> = high.iter().map(|v| v * PRE_GAIN).collect();
        let ref_lf = wd.join("ref_lf.wav");
        let ref_hf = wd.join("ref_hf.wav");
        wav::write_s16(&ref_lf, &low16, 16000).unwrap();
        wav::write_f32(&ref_hf, &high_p, SAMPLE_RATE).unwrap();

        let lf = wav::read_f64(&wdp.join("lf.wav")).unwrap().0;
        let hf = wav::read_f64(&wdp.join("hf.wav")).unwrap().0;
        let (rlf, _, _) = wav::read_f64(&ref_lf).unwrap();
        let (rhf, _, _) = wav::read_f64(&ref_hf).unwrap();
        assert_eq!(lf.len(), rlf.len(), "LF lengths differ");
        assert_eq!(hf.len(), rhf.len(), "HF lengths differ");
        // s16 quantization: allow 1 LSB; f32 path: allow 1e-6.
        let ml = lf
            .iter()
            .zip(rlf.iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f64::max);
        let mh = hf
            .iter()
            .zip(rhf.iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f64::max);
        assert!(ml <= 1.0 / 32768.0 + 1e-9, "LF max diff {ml}");
        assert!(mh < 1e-6, "HF max diff {mh}");
        let _ = std::fs::remove_dir_all(&wd);
    }

    #[test]
    fn stream_hf_tilt_matches_batch() {
        // hf_tilt on: the written HF band WAV must equal the batch reference
        // (split -> batch tilt convolution -> PRE_GAIN).
        let src = synth_wav(3, 44100, true);
        let wd = std::env::temp_dir().join(format!("sena-enc-tilt-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&wd);
        std::fs::create_dir_all(&wd).unwrap();
        let cfg = test_cfg(&wd, 160, false, Some(100.0));
        unsafe { std::env::set_var("SENAENC_DUMP_RAILS", "1") };
        let mut bytes = &src[..];
        let out = wd.join("out.sena");
        let outcome = encode_stream(&cfg, &mut bytes, &out).unwrap();
        let wdp = outcome.workdir.as_path();

        let (x, ch, in_rate) = wav::read_f64_bytes(&src).unwrap();
        let x48 = sena_dsp::Resampler::new(in_rate, SAMPLE_RATE).process(&x, ch);
        let (_low, high) = sena_dsp::split(&x48, 2, 300.0);
        // batch tilt: zero-phase conv (aligned like FirStream)
        let h = sena_dsp::tilt_taps(100.0);
        let d = (h.len() - 1) / 2;
        let n = high.len() / ch;
        let mut tilted = vec![0.0; high.len()];
        for i in 0..n {
            for c in 0..ch {
                let mut acc = 0.0;
                for (k, &t) in h.iter().enumerate() {
                    let ii = i as isize + k as isize - d as isize;
                    if ii >= 0 && (ii as usize) < n {
                        acc += t * high[(ii as usize) * ch + c];
                    }
                }
                tilted[i * ch + c] = acc;
            }
        }
        let ref_hf = wd.join("ref_hf_tilt.wav");
        let high_p: Vec<f64> = tilted.iter().map(|v| v * PRE_GAIN).collect();
        wav::write_f32(&ref_hf, &high_p, SAMPLE_RATE).unwrap();
        let hf = wav::read_f64(&wdp.join("hf.wav")).unwrap().0;
        let (rhf, _, _) = wav::read_f64(&ref_hf).unwrap();
        assert_eq!(hf.len(), rhf.len(), "HF lengths differ");
        let mh = hf
            .iter()
            .zip(rhf.iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f64::max);
        assert!(mh < 1e-6, "HF max diff {mh}");
        let _ = std::fs::remove_dir_all(&wd);
    }

    /// Three-track streaming DSP: mid.wav must equal the batch reference
    /// (split@600 -> split@12000 low -> PRE_GAIN) and hf.wav the shifted +
    /// downsampled top band (split@12000 high -> shift_down -> 48k->16k ->
    /// PRE_GAIN). Decoding the top track (16k->48k -> shift_up) and adding
    /// the mid band must reconstruct the 600 Hz-high band.
    #[test]
    fn stream_three_track_matches_batch() {
        let src = synth_wav(3, 44100, true);
        let wd = std::env::temp_dir().join(format!("sena-enc-3t-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&wd);
        std::fs::create_dir_all(&wd).unwrap();
        let cfg = test_cfg(&wd, 256, true, None);
        unsafe { std::env::set_var("SENAENC_DUMP_RAILS", "1") };
        let mut bytes = &src[..];
        let out = wd.join("out.sena");
        let outcome = encode_stream(&cfg, &mut bytes, &out).unwrap();
        let wdp = outcome.workdir.as_path();

        let (x, ch, in_rate) = wav::read_f64_bytes(&src).unwrap();
        let x48 = sena_dsp::Resampler::new(in_rate, SAMPLE_RATE).process(&x, ch);
        let (_low, high) = sena_dsp::split(&x48, 2, 300.0);
        let (mid, top) = sena_dsp::split(&high, 2, sena_core::HF_SPLIT_HZ);
        let mid_p: Vec<f64> = mid.iter().map(|v| v * PRE_GAIN).collect();
        let top_s = sena_dsp::shift_down(&top, 2, sena_core::HF_SPLIT_HZ, SAMPLE_RATE);
        let top16 = sena_dsp::Resampler::new(SAMPLE_RATE, sena_core::HF_TRACK_RATE)
            .process(&top_s, 2);
        let top_p: Vec<f64> = top16.iter().map(|v| v * PRE_GAIN).collect();
        let ref_mid = wd.join("ref_mid.wav");
        let ref_top = wd.join("ref_top.wav");
        wav::write_f32(&ref_mid, &mid_p, SAMPLE_RATE).unwrap();
        wav::write_f32(&ref_top, &top_p, sena_core::HF_TRACK_RATE).unwrap();

        let got_mid = wav::read_f64(&wdp.join("mid.wav")).unwrap().0;
        let (got_top, top_rate) = {
            let (v, _, r) = wav::read_f64(&wdp.join("hf.wav")).unwrap();
            (v, r)
        };
        assert_eq!(top_rate, sena_core::HF_TRACK_RATE);
        let (rmid, _, _) = wav::read_f64(&ref_mid).unwrap();
        let (rtop, _, _) = wav::read_f64(&ref_top).unwrap();
        assert_eq!(got_mid.len(), rmid.len(), "mid lengths differ");
        assert_eq!(got_top.len(), rtop.len(), "top lengths differ");
        let mm = got_mid
            .iter()
            .zip(rmid.iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f64::max);
        let mt = got_top
            .iter()
            .zip(rtop.iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f64::max);
        assert!(mm < 1e-6, "mid max diff {mm}");
        assert!(mt < 1e-6, "top max diff {mt}");

        // Reconstruction: mid + decode-side top chain (upsample + shift up)
        // recovers the 600 Hz-high band (settled region; the synth input
        // has essentially no 15.6 kHz+ content, so this stays tight).
        let n = high.len() / 2;
        let back16: Vec<f64> = got_top.iter().map(|v| v / PRE_GAIN).collect();
        let up = sena_dsp::Resampler::new(sena_core::HF_TRACK_RATE, SAMPLE_RATE)
            .process(&back16, 2);
        let back = sena_dsp::shift_up(&up, 2, sena_core::HF_SPLIT_HZ, SAMPLE_RATE);
        let settle = 6200usize;
        for i in settle..n - settle {
            let sum_l = got_mid[i * 2] / PRE_GAIN + back[i * 2];
            let sum_r = got_mid[i * 2 + 1] / PRE_GAIN + back[i * 2 + 1];
            assert!((sum_l - high[i * 2]).abs() < 1e-4, "frame {i} L");
            assert!((sum_r - high[i * 2 + 1]).abs() < 1e-4, "frame {i} R");
        }
        let _ = std::fs::remove_dir_all(&wd);
    }
}
