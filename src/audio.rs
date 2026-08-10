//! Audio decoding, encoding and playback.
//!
//! The two engines hand us different things — ElevenLabs returns MP3 over the
//! wire, `say` writes a WAV file — so everything is normalised to [`Pcm`] and
//! written back out in whichever format the user asked for.

use anyhow::{Context, Result, bail};
use std::io::Cursor;
use std::path::Path;
use std::time::Duration;

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

/// The extensions the Audio Player will open. rodio is built here with only the
/// MP3 and WAV decoders, so offering more would be offering files it cannot play.
pub const PLAYABLE_EXTENSIONS: &[&str] = &["mp3", "wav", "wave"];

/// How far [`Playback::skip_back`] rewinds.
pub const SKIP_BACK: Duration = Duration::from_secs(10);

/// A sound that is currently playing, whichever engine produced it.
///
/// The `say` variant is a child process rather than decoded audio because
/// macOS speaks it directly; stopping it means killing the process — and it is
/// why the transport controls below all report "can't" rather than panicking:
/// a running `say` cannot be paused or rewound, only stopped.
pub enum Playback {
    Decoded {
        // Held only to keep the output device open for the player's lifetime.
        _device: rodio::MixerDeviceSink,
        player: rodio::Player,
        /// Total length, when the decoder could work one out. Audio stitched
        /// from several ElevenLabs responses often cannot report one, so this
        /// is an `Option` rather than a number to be trusted.
        duration: Option<Duration>,
    },
    Process(std::process::Child),
}

impl Playback {
    /// Starts playing MP3 bytes on the default output device.
    pub fn play_mp3(bytes: Vec<u8>) -> Result<Self> {
        let byte_len = bytes.len() as u64;
        let decoder = rodio::Decoder::builder()
            .with_data(Cursor::new(bytes))
            // Both are what make seeking work; without a byte length the
            // decoder can neither seek accurately nor estimate a duration.
            .with_byte_len(byte_len)
            .with_seekable(true)
            .build()
            .context("could not decode the audio")?;
        Self::start(decoder)
    }

    /// Starts a short sound effect built into the binary.
    ///
    /// Deliberately its own output device rather than a second source on the
    /// player already running: a cue has to be able to sound *over* a document
    /// being read aloud, since "that failed" is worth knowing before the
    /// reading ends, and it must not disturb a player the user can pause and
    /// rewind. Sharing one would make the cue part of what Stop stops and what
    /// the transport reports the position of.
    pub fn play_cue(wav: &'static [u8]) -> Result<Self> {
        let decoder = rodio::Decoder::builder()
            .with_data(Cursor::new(wav))
            .with_byte_len(wav.len() as u64)
            .build()
            .context("could not decode the sound effect")?;
        Self::start(decoder)
    }

    /// Starts playing an audio file from disk.
    pub fn play_file(path: &Path) -> Result<Self> {
        let file = std::fs::File::open(path)
            .with_context(|| format!("could not open {}", path.display()))?;
        // `TryFrom<File>` reads the length from the file metadata and turns
        // seeking on, which is exactly what the transport controls need.
        let decoder = rodio::Decoder::try_from(file).with_context(|| {
            format!(
                "{} could not be played — it may not be a WAV or MP3 file",
                path.file_name().unwrap_or_default().to_string_lossy()
            )
        })?;
        Self::start(decoder)
    }

    fn start<S>(source: S) -> Result<Self>
    where
        S: rodio::Source + Send + 'static,
    {
        let duration = source.total_duration();
        let mut device = rodio::DeviceSinkBuilder::open_default_sink()
            .context("could not open an audio output device")?;
        // Stopping is something the user asked for, so rodio's warning that
        // dropping the sink will end playback is noise on every Stop press.
        device.log_on_drop(false);
        let player = rodio::Player::connect_new(device.mixer());
        player.append(source);
        Ok(Self::Decoded {
            _device: device,
            player,
            duration,
        })
    }

    pub fn is_finished(&mut self) -> bool {
        match self {
            // A paused player is not an empty one, so this stays false while
            // the user is holding the file mid-sentence.
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

    pub fn is_paused(&self) -> bool {
        match self {
            Self::Decoded { player, .. } => player.is_paused(),
            Self::Process(_) => false,
        }
    }

    pub fn pause(&mut self) {
        if let Self::Decoded { player, .. } = self {
            player.pause();
        }
    }

    pub fn resume(&mut self) {
        if let Self::Decoded { player, .. } = self {
            player.play();
        }
    }

    /// Where playback has reached, counting from the start of the file.
    pub fn position(&self) -> Duration {
        match self {
            Self::Decoded { player, .. } => player.get_pos(),
            Self::Process(_) => Duration::ZERO,
        }
    }

    pub fn duration(&self) -> Option<Duration> {
        match self {
            Self::Decoded { duration, .. } => *duration,
            Self::Process(_) => None,
        }
    }

    /// Rewinds by [`SKIP_BACK`], stopping at the beginning rather than
    /// wrapping round or failing when there is less than that to rewind.
    pub fn skip_back(&mut self) -> Result<()> {
        let Self::Decoded { player, .. } = self else {
            bail!("this voice cannot be rewound; stop it and start again");
        };
        let target = player.get_pos().saturating_sub(SKIP_BACK);
        player
            .try_seek(target)
            .map_err(|e| anyhow::anyhow!("could not skip back in this file: {e}"))
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A position on the clock, written the way a person would say it.
///
/// Deliberately not `03:07` — a screen reader says "oh three colon oh seven"
/// for that, and this is the one piece of text in the player that a listener
/// has to take in while the audio is running.
pub fn spoken_time(time: Duration) -> String {
    let total = time.as_secs();
    let (minutes, seconds) = (total / 60, total % 60);
    match (minutes, seconds) {
        (0, 1) => "1 second".to_string(),
        (0, s) => format!("{s} seconds"),
        (1, 0) => "1 minute".to_string(),
        (m, 0) => format!("{m} minutes"),
        (1, 1) => "1 minute 1 second".to_string(),
        (1, s) => format!("1 minute {s} seconds"),
        (m, 1) => format!("{m} minutes 1 second"),
        (m, s) => format!("{m} minutes {s} seconds"),
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
    fn mp3_parts_joined_end_to_end_decode_as_one_continuous_recording() {
        // A document past ElevenLabs' per-request limit comes back as several
        // MP3s that the client concatenates. This is the check that the join
        // is real: a decoder must play straight through all three parts, not
        // stop at the end of the first.
        let part = encode_mp3(&tone(1, 44_100, 0.4)).unwrap();
        let joined: Vec<u8> = part.repeat(3);
        assert_eq!(joined.len(), part.len() * 3);

        let decoded = decode_mp3(&joined).unwrap();
        let seconds =
            decoded.samples.len() as f32 / (decoded.channels as f32 * decoded.sample_rate as f32);
        assert!(
            (seconds - 1.2).abs() < 0.25,
            "three 0.4s parts decoded to {seconds}s, so the join was not followed"
        );

        // And it has to survive being written out, which is what "save" does.
        let path = std::env::temp_dir().join("soe-stitch-test.wav");
        write_wav(&path, &decoded).unwrap();
        let back = read_wav(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(back.samples.len(), decoded.samples.len());
    }

    #[test]
    fn times_are_written_the_way_they_are_read_out() {
        let say = |secs| spoken_time(Duration::from_secs(secs));
        assert_eq!(say(0), "0 seconds");
        assert_eq!(say(1), "1 second");
        assert_eq!(say(45), "45 seconds");
        assert_eq!(say(60), "1 minute");
        assert_eq!(say(61), "1 minute 1 second");
        assert_eq!(say(125), "2 minutes 5 seconds");
        assert_eq!(say(180), "3 minutes");
        assert_eq!(say(121), "2 minutes 1 second");
    }

    #[test]
    fn a_saved_file_reports_a_duration_the_player_can_show() {
        // The transport needs a length to display and to seek within; a file
        // the decoder cannot measure would leave the player mute about both.
        for format in AudioFormat::ALL {
            let pcm = tone(1, 22_050, 1.5);
            let path =
                std::env::temp_dir().join(format!("soe-duration-test.{}", format.extension()));
            save(&path, &pcm, format).unwrap();

            let file = std::fs::File::open(&path).unwrap();
            let decoder = rodio::Decoder::try_from(file).unwrap();
            let duration = rodio::Source::total_duration(&decoder);
            std::fs::remove_file(&path).ok();

            let seconds = duration
                .unwrap_or_else(|| panic!("{format:?} reported no duration"))
                .as_secs_f32();
            assert!(
                (seconds - 1.5).abs() < 0.15,
                "{format:?} reported {seconds}s, expected about 1.5s"
            );
        }
    }

    /// The decoder the player builds has to be able to seek, or "Back 10
    /// Seconds" is a button that only ever reports an error. Checked without
    /// an output device, because that is the part CI can actually run.
    #[test]
    fn a_saved_file_can_be_rewound_which_is_what_skipping_back_needs() {
        use rodio::Source as _;

        for format in AudioFormat::ALL {
            let pcm = tone(1, 22_050, 30.0);
            let path = std::env::temp_dir().join(format!("soe-seek-test.{}", format.extension()));
            save(&path, &pcm, format).unwrap();

            let file = std::fs::File::open(&path).unwrap();
            let mut decoder = rodio::Decoder::try_from(file).unwrap();
            decoder
                .try_seek(Duration::from_secs(20))
                .unwrap_or_else(|e| panic!("{format:?} could not be seeked: {e}"));

            let sample_rate = decoder.sample_rate().get() as f32;
            let channels = decoder.channels().get() as f32;
            let remaining = decoder.count() as f32 / (sample_rate * channels);
            std::fs::remove_file(&path).ok();

            assert!(
                (remaining - 10.0).abs() < 1.0,
                "{format:?} left {remaining}s after seeking to 20s of 30s"
            );
        }
    }

    /// Held for as long as a test has an output device open.
    ///
    /// Windows will not have two of these at once from two test threads: the
    /// run dies with `STATUS_ACCESS_VIOLATION` — a native crash, no panic, no
    /// failing test — the moment a second one is opened while the first is
    /// still up. It survived for as long as exactly one test in the suite
    /// opened a device, and broke the first time a second one did.
    ///
    /// The poisoning is ignored deliberately. A panic in one of these tests
    /// should fail that test, not turn every other one into an unrelated
    /// "poisoned mutex" failure that hides it.
    static DEVICE: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn device_lock() -> std::sync::MutexGuard<'static, ()> {
        DEVICE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// A cue on a real output device, which is the one thing the decode tests
    /// in `app` cannot cover: the sounds are built into the binary and played
    /// through their own sink, and a failure to open one is swallowed into the
    /// log on purpose so it can never interrupt what the user is being told.
    /// Skipped where there is no audio hardware, as below.
    #[test]
    fn a_cue_plays_on_a_real_device_and_finishes_by_itself() {
        const CUE: &[u8] = include_bytes!("../assets/sounds/error.wav");
        let _device = device_lock();

        let Ok(mut cue) = Playback::play_cue(CUE) else {
            eprintln!("no audio output device; skipping the cue test");
            return;
        };
        assert!(!cue.is_finished(), "the cue was over before it started");
        // The error cue is 0.4s long. A full second and it has certainly had
        // its chance, so a cue still going is one that would talk over the
        // next thing the app says.
        std::thread::sleep(Duration::from_secs(1));
        assert!(
            cue.is_finished(),
            "the cue was still playing a second later"
        );
    }

    /// The whole transport, on a real output device.
    ///
    /// Skipped where there is no audio hardware — CI runners have none — since
    /// the point of this test is the device path, and a machine that cannot
    /// play sound has nothing to say about whether Pause works.
    #[test]
    fn the_transport_pauses_rewinds_and_stops_a_real_file() {
        let _device = device_lock();
        let path = std::env::temp_dir().join("soe-transport-test.wav");
        write_wav(&path, &tone(1, 22_050, 20.0)).unwrap();

        let Ok(mut playback) = Playback::play_file(&path) else {
            std::fs::remove_file(&path).ok();
            eprintln!("no audio output device; skipping the transport test");
            return;
        };
        assert!(!playback.is_paused());
        assert_eq!(playback.duration().map(|d| d.as_secs()), Some(20));

        std::thread::sleep(Duration::from_millis(500));
        playback.pause();
        assert!(playback.is_paused());

        // Read after the pause has settled: the controls are polled every few
        // milliseconds, so the position keeps creeping for an instant.
        std::thread::sleep(Duration::from_millis(100));
        let held = playback.position();
        assert!(held > Duration::ZERO, "the position never advanced");
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(playback.position(), held, "a paused file kept moving");

        // Less than ten seconds in, so this lands at the start rather than
        // before it — and does not quietly start playing again.
        playback.skip_back().unwrap();
        assert_eq!(playback.position(), Duration::ZERO);
        assert!(playback.is_paused(), "skipping back resumed playback");
        assert!(!playback.is_finished(), "a paused file counted as finished");

        playback.resume();
        assert!(!playback.is_paused());
        playback.stop();
        std::fs::remove_file(&path).ok();
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
