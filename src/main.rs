use std::{
    fs::File,
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

use indicatif::{ProgressBar, ProgressStyle};
use shine_rs::{Mp3Encoder, Mp3EncoderConfig, StereoMode};

use symphonia::core::{
    audio::Channels,
    codecs::audio::AudioDecoderOptions,
    errors::Error as SymphoniaError,
    formats::{FormatOptions, TrackType, probe::Hint},
    io::MediaSourceStream,
    meta::MetadataOptions,
};

type AppResult<T> = Result<T, Box<dyn std::error::Error>>;

#[cfg(windows)]
fn install_context_menu() -> std::io::Result<()> {
    use winreg::{RegKey, enums::HKEY_CURRENT_USER};

    const REG_PATH: &str =
        r"Software\Classes\SystemFileAssociations\.mp4\shell\ToMp3";

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let exe = std::env::current_exe()?;
    let exe = exe.to_string_lossy();

    let (key, _) = hkcu.create_subkey(REG_PATH)?;

    key.set_value("", &"To MP3")?;
    key.set_value("Icon", &format!("\"{exe}\""))?;

    let (command, _) = key.create_subkey("command")?;
    command.set_value("", &format!("\"{exe}\" \"%1\""))?;

    println!("Context menu registered!");

    Ok(())
}

#[cfg(windows)]
fn uninstall_context_menu() -> std::io::Result<()> {
    use winreg::{RegKey, enums::HKEY_CURRENT_USER};

    const REG_PATH: &str =
        r"Software\Classes\SystemFileAssociations\.mp4\shell\ToMp3";

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);

    match hkcu.delete_subkey_all(REG_PATH) {
        Ok(()) => println!("Context menu unregistered!"),

        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            println!("Context menu is not registered.");
        }

        Err(err) => return Err(err),
    }

    Ok(())
}

#[cfg(not(windows))]
fn install_context_menu() -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "Context menu registration is only supported on Windows",
    ))
}

#[cfg(not(windows))]
fn uninstall_context_menu() -> std::io::Result<()> {
    install_context_menu()
}

fn parse_args() -> AppResult<Option<(PathBuf, PathBuf)>> {
    let args: Vec<String> = std::env::args().collect();

    match args.get(1).map(String::as_str) {
        Some("install") => {
            install_context_menu()?;
            return Ok(None);
        }

        Some("uninstall") => {
            uninstall_context_menu()?;
            return Ok(None);
        }

        None => {
            print_usage();
            return Ok(None);
        }

        _ => {}
    }

    let input = PathBuf::from(&args[1]);

    let output = args
        .get(2)
        .map(PathBuf::from)
        .unwrap_or_else(|| input.with_extension("mp3"));

    Ok(Some((input, output)))
}

fn print_usage() {
    println!("Usage:");
    println!("  mp4tomp3.exe install");
    println!("  mp4tomp3.exe uninstall");
    println!("  mp4tomp3.exe <input.mp4> [output.mp3]");
}

fn create_encoder(
    sample_rate: u32,
    channels: usize,
) -> AppResult<Mp3Encoder> {
    if channels != 1 && channels != 2 {
        return Err(
            format!("Unsupported channel count: {channels}").into()
        );
    }

    let config = Mp3EncoderConfig::new()
        .sample_rate(sample_rate)
        .bitrate(256)
        .channels(channels as u8)
        .stereo_mode(if channels == 1 {
            StereoMode::Mono
        } else {
            StereoMode::Stereo
        });

    Ok(Mp3Encoder::new(config)?)
}

fn create_progress_bar() -> ProgressBar {
    let pb = ProgressBar::new_spinner();

    pb.set_style(
        ProgressStyle::default_spinner()
            .template(
                "{spinner:.green} Decoding & encoding audio... {pos} frames"
            )
            .unwrap(),
    );

    pb.enable_steady_tick(Duration::from_millis(100));

    pb
}

fn encode_audio_buffer(
    decoded: &symphonia::core::audio::GenericAudioBufferRef<'_>,
    encoder: &mut Mp3Encoder,
    output: &mut File,
) -> AppResult<()> {
    let mut pcm = Vec::new();

    decoded.copy_to_vec_interleaved(&mut pcm);

    let encoded_chunks = encoder.encode_interleaved(&pcm)?;

    for chunk in encoded_chunks {
        output.write_all(&chunk)?;
    }

    Ok(())
}

fn decode_and_encode(
    format: &mut dyn symphonia::core::formats::FormatReader,
    decoder: &mut dyn symphonia::core::codecs::audio::AudioDecoder,
    track_id: u32,
    output: &mut File,
) -> AppResult<()> {
    let pb = create_progress_bar();

    let mut encoder: Option<Mp3Encoder> = None;
    let mut audio_spec: Option<(u32, Channels)> = None;

    loop {
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,

            Err(SymphoniaError::ResetRequired) => {
                return Err(
                    "Audio format requires a decoder reset".into()
                );
            }

            Err(err) => return Err(err.into()),
        };

        while !format.metadata().is_latest() {
            format.metadata().pop();
        }

        if packet.track_id != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,

            Err(SymphoniaError::DecodeError(_)) => continue,

            Err(err) => return Err(err.into()),
        };

        let spec = decoded.spec();

        let sample_rate = spec.rate();
        let channels = spec.channels().count();
        let channel_layout = spec.channels().to_owned();

        
        if let Some((expected_rate, expected_channels)) = &audio_spec {
            if *expected_rate != sample_rate
                || *expected_channels != channel_layout
            {
                return Err(
                    "Audio parameters changed during decoding".into()
                );
            }
        } else {
            encoder = Some(create_encoder(sample_rate, channels)?);

            audio_spec = Some((sample_rate, channel_layout));
        }

        let frames = decoded.frames();

        encode_audio_buffer(
            &decoded,
            encoder.as_mut().ok_or("Encoder was not initialized")?,
            output,
        )?;

        pb.inc(frames as u64);
    }

    pb.finish_with_message("Decoding and encoding completed!");

    let mut encoder = encoder.ok_or("No audio data was decoded")?;

    let final_data = encoder.finish()?;

    output.write_all(&final_data)?;

    Ok(())
}

fn convert_mp4_to_mp3(
    input_path: &Path,
    output_path: &Path,
) -> AppResult<()> {
    
    let src = File::open(input_path)?;

    let media_source_stream =
        MediaSourceStream::new(Box::new(src), Default::default());

    let mut hint = Hint::new();
    hint.with_extension("mp4");

    let meta_opts = MetadataOptions::default();
    let fmt_opts = FormatOptions::default();

    let mut format = symphonia::default::get_probe()
        .probe(&hint, media_source_stream, fmt_opts, meta_opts)?;

    let track = format
        .default_track(TrackType::Audio)
        .ok_or("No audio track found")?;

    let track_id = track.id;

    let codec_params = track
        .codec_params
        .as_ref()
        .ok_or("Codec parameters missing")?;

    let audio_codec_params = codec_params
        .audio()
        .ok_or("Audio codec parameters missing")?;

    let dec_opts = AudioDecoderOptions::default();

    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(audio_codec_params, &dec_opts)?;

    let mut output = File::create(output_path)?;

    decode_and_encode(
        &mut *format,
        &mut *decoder,
        track_id,
        &mut output,
    )?;

    Ok(())
}

fn main() -> AppResult<()> {
    let Some((input, output)) = parse_args()? else {
        return Ok(());
    };

    convert_mp4_to_mp3(&input, &output)?;

    println!(
        "Successfully converted:\n{} -> {}",
        input.display(),
        output.display()
    );

    Ok(())
}