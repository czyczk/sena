//! senadec CLI: decode .sena/.mka to WAV or raw PCM.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use sena_dec::demux::Demuxed;
use sena_dec::output::{self, Dither, SampleFormat};
use sena_dec::pipeline::{decode, probe};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    WavF32,
    WavS24,
    WavS16,
    RawF32,
    RawS24,
    RawS16,
}

#[derive(Debug)]
struct Args {
    input: PathBuf,
    output: Option<PathBuf>,
    format: Format,
    dither: Dither,
    dump_prefix: Option<PathBuf>,
    info: bool,
    output_explicit: bool,
    force: bool,
}

fn usage() -> &'static str {
    "usage: senadec [--format FORMAT] [--dither none] [--dump-tracks PREFIX] [--info] IN.sena [-o OUT.wav]\n\
     \n\
     FORMAT: wav-f32 (default) | wav-s24 | wav-s16 | raw-f32 | raw-s24 | raw-s16\n\
     -o - or omitted -o writes to stdout; logs go to stderr only."
}

fn parse_args() -> Result<Args, String> {
    let mut args = std::env::args().skip(1);
    let mut input = None;
    let mut output = None;
    let mut format = Format::WavF32;
    let mut dither = Dither::Tpdf;
    let mut dump_prefix = None;
    let mut info = false;
    let mut output_explicit = false;
    let mut force = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--format" => {
                let v = args.next().ok_or("--format requires a value")?;
                format = match v.as_str() {
                    "wav-f32" => Format::WavF32,
                    "wav-s24" => Format::WavS24,
                    "wav-s16" => Format::WavS16,
                    "raw-f32" => Format::RawF32,
                    "raw-s24" => Format::RawS24,
                    "raw-s16" => Format::RawS16,
                    _ => return Err(format!("unknown --format {v}")),
                };
            }
            "--dither" => {
                let v = args.next().ok_or("--dither requires a value")?;
                dither = match v.as_str() {
                    "none" => Dither::None,
                    "tpdf" => Dither::Tpdf,
                    _ => return Err(format!("unknown --dither {v} (expected none|tpdf)")),
                };
            }
            "--dump-tracks" => {
                dump_prefix = Some(PathBuf::from(args.next().ok_or("--dump-tracks requires a prefix")?));
            }
            "--info" => info = true,
            "--force" => force = true,
            "-o" | "--output" => {
                output = Some(PathBuf::from(args.next().ok_or("-o requires a path")?));
                output_explicit = true;
            }
            "-h" | "--help" => {
                eprintln!("{}", usage());
                std::process::exit(0);
            }
            _ if arg.starts_with('-') && arg != "-" => return Err(format!("unknown option {arg}")),
            _ => {
                if input.is_some() {
                    return Err("multiple input files".into());
                }
                input = Some(PathBuf::from(arg));
            }
        }
    }
    Ok(Args {
        input: input.ok_or("missing input file")?,
        output,
        format,
        dither,
        dump_prefix,
        info,
        output_explicit,
        force,
    })
}

fn write_all(out: &mut dyn Write, data: &[u8], path: Option<&std::path::Path>) -> std::io::Result<()> {
    if let Some(path) = path {
        std::fs::write(path, data)
    } else {
        out.write_all(data)
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("senadec: {e}");
            eprintln!("{}", usage());
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let args = parse_args()?;
    let mut file = std::fs::File::open(&args.input).map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    let demux = Demuxed::parse(bytes).map_err(|e| e.to_string())?;

    if args.info {
        let (info, warnings) = probe(&demux).map_err(|e| e.to_string())?;
        for w in &warnings {
            eprintln!("warning: {w}");
        }
        eprintln!(
            "info: profile={} version={} rate={} ch={} playable={} frames ({:.6} s)",
            info.profile,
            info.sena_version,
            info.sample_rate,
            info.channels,
            info.playable_frames,
            info.playable_frames as f64 / info.sample_rate as f64
        );
        if !info.audio_sha256.is_empty() {
            eprintln!("info: audio-sha256={}", info.audio_sha256);
        }
        return Ok(());
    }

    let decoded = decode(&demux).map_err(|e| e.to_string())?;
    let info = decoded.info();
    for w in decoded.warnings() {
        eprintln!("warning: {w}");
    }
    eprintln!(
        "info: profile={} version={} rate={} ch={} playable={} frames ({:.6} s)",
        info.profile,
        info.sena_version,
        info.sample_rate,
        info.channels,
        info.playable_frames,
        info.playable_frames as f64 / info.sample_rate as f64
    );

    if let Some(prefix) = &args.dump_prefix {
        let lf_name = prefix.with_extension("lf.wav");
        let hf_name = prefix.with_extension("hf.wav");
        let lf = output::encode_wav(decoded.lf_track_pcm(), SampleFormat::F32, Dither::None)
            .map_err(|e| e.to_string())?;
        let hf = output::encode_wav(decoded.hf_track_pcm(), SampleFormat::F32, Dither::None)
            .map_err(|e| e.to_string())?;
        std::fs::write(&lf_name, lf).map_err(|e| format!("{}: {e}", lf_name.display()))?;
        std::fs::write(&hf_name, hf).map_err(|e| format!("{}: {e}", hf_name.display()))?;
        eprintln!("wrote {} and {}", lf_name.display(), hf_name.display());
        if !args.output_explicit {
            return Ok(());
        }
    }

    let pcm = decoded.pcm_f64();
    let out_path = args.output.as_deref().filter(|p| p.as_os_str() != "-");
    if let Some(p) = out_path {
        if p.exists() && !args.force {
            return Err(format!(
                "output file {} already exists; use --force to overwrite",
                p.display()
            ));
        }
    }
    match args.format {
        Format::WavF32 | Format::WavS24 | Format::WavS16 => {
            let (fmt, bits) = match args.format {
                Format::WavF32 => (SampleFormat::F32, 32),
                Format::WavS24 => (SampleFormat::S24, 24),
                _ => (SampleFormat::S16, 16),
            };
            let data = output::encode_wav(pcm, fmt, args.dither).map_err(|e| e.to_string())?;
            write_all(&mut std::io::stdout(), &data, out_path).map_err(|e| e.to_string())?;
            eprintln!("wrote {} {bits}-bit WAV ({} bytes)", out_path.map(|p| p.display().to_string()).unwrap_or_else(|| "stdout".into()), data.len());
        }
        Format::RawF32 | Format::RawS24 | Format::RawS16 => {
            let (fmt, bits) = match args.format {
                Format::RawF32 => (SampleFormat::F32, 32),
                Format::RawS24 => (SampleFormat::S24, 24),
                _ => (SampleFormat::S16, 16),
            };
            let data = output::encode_raw(pcm, fmt, args.dither).map_err(|e| e.to_string())?;
            write_all(&mut std::io::stdout(), &data, out_path).map_err(|e| e.to_string())?;
            eprintln!("wrote {} raw {bits}-bit PCM ({} bytes)", out_path.map(|p| p.display().to_string()).unwrap_or_else(|| "stdout".into()), data.len());
        }
    }
    Ok(())
}
