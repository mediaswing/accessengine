//! Audio decoding, encoding and playback.
//!
//! The two engines hand us different things — ElevenLabs returns MP3 over the
//! wire, `say` writes a WAV file — so everything is normalised to [`Pcm`] and
//! written back out in whichever format the user asked for.

use anyhow::{Context, Result, bail};
use std::io::Cursor;
use std::path::Path;

/// The file formats the app can save.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AudioFormat {
    #[default]
    Wav,
    Mp3,
}

impl AudioFormat {
    pub const ALL: [Self; 2] = [Self::Wav, Self::Mp3];

    pub fn extension(self) -> &'static str {
        match self {
            Self::Wav => "wav",
            Self::Mp3 => "mp3",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Wav => "WAV (uncompressed)",
            Self::Mp3 => "MP3 (compressed)",
        }
    }

    /// Picks the format from a filename the user typed, defaulting to WAV.
    pub fn from_path(path: &Path) -> Self {
        match path.extension().and_then(|e| e.to_str()) {
            Some(ext) if ext.eq_ignore_ascii_case("mp3") => Self::Mp3,
            _ => Self::Wav,
        }
    }
}

/// Interleaved 16-bit signed samples, the common currency between formats.
pub struct Pcm {
    pub samples: Vec<i16>,
    pub channels: u16,
    pub sample_rate: u32,
}

/// Decodes MP3 bytes to PCM by running them through the same decoder used for
/// playback, so what gets saved is what was heard.
pub fn decode_mp3(bytes: &[u8]) -> Result<Pcm> {
    use rodio::Source as _;

    let decoder = rodio::Decoder::new(Cursor::new(bytes.to_vec()))
        .context("the audio returned by ElevenLabs could not be decoded")?;
    let channels = decoder.channels().get();
    let sample_rate = decoder.sample_rate().get();
    // rodio yields f32 in [-1.0, 1.0]; scale into the i16 range without wrapping.
    let samples: Vec<i16> = decoder
        .map(|s| (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
        .collect();

    if samples.is_empty() {
        bail!("the synthesised audio was empty");
    }
    Ok(Pcm {
        samples,
        channels,
        sample_rate,
    })
}

pub fn read_wav(path: &Path) -> Result<Pcm> {
    let mut reader = hound::WavReader::open(path)
        .with_context(|| format!("could not open {}", path.display()))?;
    let spec = reader.spec();
    let samples: Vec<i16> = match spec.sample_format {
        hound::SampleFormat::Int if spec.bits_per_sample <= 16 => {
            reader.samples::<i16>().collect::<Result<_, _>>()?
        }
        hound::SampleFormat::Int => reader
            .samples::<i32>()
            .map(|s| s.map(|v| (v >> (spec.bits_per_sample - 16)) as i16))
            .collect::<Result<_, _>>()?,
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .map(|s| s.map(|v| (v.clamp(-1.0, 1.0) * i16::MAX as f32) as i16))
            .collect::<Result<_, _>>()?,
    };
    Ok(Pcm {
        samples,
        channels: spec.channels,
        sample_rate: spec.sample_rate,
    })
}

pub fn write_wav(path: &Path, pcm: &Pcm) -> Result<()> {
    let spec = hound::WavSpec {
        channels: pcm.channels,
        sample_rate: pcm.sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec)
        .with_context(|| format!("could not create {}", path.display()))?;
    for sample in &pcm.samples {
        writer.write_sample(*sample)?;
    }
    writer
        .finalize()
        .with_context(|| format!("could not finish writing {}", path.display()))
}

/// Encodes PCM to MP3 at 128 kbps, which is transparent enough for speech.
pub fn encode_mp3(pcm: &Pcm) -> Result<Vec<u8>> {
    use mp3lame_encoder::{Builder, FlushNoGap, InterleavedPcm, MonoPcm};

    if !matches!(pcm.channels, 1 | 2) {
        bail!("cannot encode {}-channel audio to MP3", pcm.channels);
    }

    let mut builder = Builder::new().context("could not initialise the MP3 encoder")?;
    builder
        .set_num_channels(pcm.channels as u8)
        .map_err(|e| anyhow::anyhow!("MP3 encoder rejected the channel count: {e}"))?;
    builder
        .set_sample_rate(pcm.sample_rate)
        .map_err(|e| anyhow::anyhow!("MP3 encoder rejected the sample rate: {e}"))?;
    builder
        .set_brate(mp3lame_encoder::Bitrate::Kbps128)
        .map_err(|e| anyhow::anyhow!("MP3 encoder rejected the bitrate: {e}"))?;
    builder
        .set_quality(mp3lame_encoder::Quality::Good)
        .map_err(|e| anyhow::anyhow!("MP3 encoder rejected the quality setting: {e}"))?;
    let mut encoder = builder
        .build()
        .map_err(|e| anyhow::anyhow!("could not build the MP3 encoder: {e}"))?;

    let mut out = Vec::with_capacity(mp3lame_encoder::max_required_buffer_size(pcm.samples.len()));
    let written = if pcm.channels == 1 {
        encoder.encode(MonoPcm(&pcm.samples), out.spare_capacity_mut())
    } else {
        encoder.encode(InterleavedPcm(&pcm.samples), out.spare_capacity_mut())
    }
    .map_err(|e| anyhow::anyhow!("MP3 encoding failed: {e}"))?;
    // Safety: the encoder reports how many bytes it initialised in the spare
    // capacity, and the buffer was sized by LAME's own worst-case estimate.
    unsafe { out.set_len(out.len() + written) };

    let flushed = encoder
        .flush::<FlushNoGap>(out.spare_capacity_mut())
        .map_err(|e| anyhow::anyhow!("MP3 encoding failed while finishing the file: {e}"))?;
    unsafe { out.set_len(out.len() + flushed) };

    Ok(out)
}

/// Writes PCM out in the requested format.
pub fn save(path: &Path, pcm: &Pcm, format: AudioFormat) -> Result<()> {
    match format {
        AudioFormat::Wav => write_wav(path, pcm),
        AudioFormat::Mp3 => {
            let bytes = encode_mp3(pcm)?;
            std::fs::write(path, bytes)
                .with_context(|| format!("could not write {}", path.display()))
        }
    }
}

/// A sound that is currently playing, whichever engine produced it.
///
/// The `say` variant is a child process rather than decoded audio because
/// macOS speaks it directly; stopping it means killing the process.
pub enum Playback {
    Decoded {
        // Held only to keep the output device open for the player's lifetime.
        _device: rodio::MixerDeviceSink,
        player: rodio::Player,
    },
    Process(std::process::Child),
}

impl Playback {
    /// Starts playing MP3 bytes on the default output device.
    pub fn play_mp3(bytes: Vec<u8>) -> Result<Self> {
        let device = rodio::DeviceSinkBuilder::open_default_sink()
            .context("could not open an audio output device")?;
        let player = rodio::Player::connect_new(device.mixer());
        let decoder =
            rodio::Decoder::new(Cursor::new(bytes)).context("could not decode the audio")?;
        player.append(decoder);
        Ok(Self::Decoded {
            _device: device,
            player,
        })
    }

    pub fn is_finished(&mut self) -> bool {
        match self {
            Self::Decoded { player, .. } => player.empty(),
            Self::Process(child) => matches!(child.try_wait(), Ok(Some(_)) | Err(_)),
        }
    }

    pub fn stop(&mut self) {
        match self {
            Self::Decoded { player, .. } => player.stop(),
            Self::Process(child) => {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(channels: u16, sample_rate: u32, secs: f32) -> Pcm {
        let frames = (sample_rate as f32 * secs) as usize;
        let samples = (0..frames)
            .flat_map(|i| {
                let v = ((i as f32 / sample_rate as f32) * 440.0 * std::f32::consts::TAU).sin();
                std::iter::repeat_n((v * 8000.0) as i16, channels as usize)
            })
            .collect();
        Pcm {
            samples,
            channels,
            sample_rate,
        }
    }

    #[test]
    fn format_is_taken_from_the_extension() {
        assert_eq!(AudioFormat::from_path(Path::new("a.MP3")), AudioFormat::Mp3);
        assert_eq!(AudioFormat::from_path(Path::new("a.wav")), AudioFormat::Wav);
        // Anything unrecognised falls back to WAV rather than failing.
        assert_eq!(
            AudioFormat::from_path(Path::new("a.aiff")),
            AudioFormat::Wav
        );
    }

    #[test]
    fn wav_survives_a_write_and_read_round_trip() {
        let pcm = tone(1, 22_050, 0.25);
        let path = std::env::temp_dir().join("soe-roundtrip-test.wav");
        write_wav(&path, &pcm).unwrap();
        let back = read_wav(&path).unwrap();
        std::fs::remove_file(&path).ok();

        assert_eq!(back.channels, 1);
        assert_eq!(back.sample_rate, 22_050);
        assert_eq!(back.samples, pcm.samples);
    }

    #[test]
    fn mp3_encoding_produces_a_decodable_file_of_the_right_length() {
        let pcm = tone(2, 44_100, 0.5);
        let bytes = encode_mp3(&pcm).unwrap();
        assert!(bytes.len() > 512, "suspiciously small MP3: {}", bytes.len());

        let decoded = decode_mp3(&bytes).unwrap();
        let seconds =
            decoded.samples.len() as f32 / (decoded.channels as f32 * decoded.sample_rate as f32);
        // MP3 pads with encoder delay, so compare durations loosely.
        assert!(
            (seconds - 0.5).abs() < 0.1,
            "expected roughly 0.5s, got {seconds}"
        );
    }
}
