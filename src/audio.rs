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

    pub fn label(self) -> String {
        match self {
            Self::Wav => crate::t!("format.wav.label"),
            Self::Mp3 => crate::t!("format.mp3.label"),
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

/// The extensions the audio player will open. rodio is built here with only the
/// MP3 and WAV decoders, so offering more would be offering files it cannot play.
pub const PLAYABLE_EXTENSIONS: &[&str] = &["mp3", "wav", "wave"];

/// How far [`Playback::skip_back`] rewinds.
pub const SKIP_BACK: Duration = Duration::from_secs(10);

/// How long a music track spends coming up underneath the speech before it.
///
/// The music starts this far from the end of the speech and fades in across
/// whatever is left of it, so it is at full volume as the last word lands
/// rather than arriving cold after a silence. Long enough to be a transition,
/// short enough that it never buries a sentence — this is the join a radio
/// bulletin makes between the newsreader and the outro.
pub const OVERLAP: Duration = Duration::from_millis(1250);

/// Where a playlist has got to, for the status line and the player pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackPosition {
    /// 1-based, because it is read out to a person.
    pub number: usize,
    pub total: usize,
    pub name: String,
}

/// Something the playlist did between one frame and the next. Both are worth
/// saying out loud: a listener who cannot see the pane has no other way to
/// know which track is playing, or that one was passed over.
pub enum PlaylistEvent {
    Started(TrackPosition),
    Skipped { name: String, error: String },
}

/// A sound that is currently playing, whichever engine produced it.
///
/// The `say` variant is a child process rather than decoded audio because
/// macOS speaks it directly; stopping it means killing the process — and it is
/// why the transport controls below all report "can't" rather than panicking:
/// a running `say` cannot be paused or rewound, only stopped.
pub enum Playback {
    /// A zip of audio files played one after another — see [`ListPlayback`].
    /// Boxed because it is much the largest of the three, and every `Playback`
    /// in the app would otherwise be sized for a playlist.
    List(Box<ListPlayback>),
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

    /// Opens a zip of audio files and starts the first track.
    pub fn play_playlist(path: &Path) -> Result<(Self, Vec<PlaylistEvent>)> {
        let mut list = ListPlayback::open(crate::playlist::Playlist::open(path)?)?;
        // Started here rather than on the first frame, so a playlist whose
        // every track is unplayable says so at the press of Play instead of
        // sitting silent for a moment and then reporting nothing at all.
        let events = list.poll();
        Ok((Self::List(Box::new(list)), events))
    }

    /// Lets a playlist move on to its next track. A no-op for everything else,
    /// which is why the caller can poll whatever is playing without asking
    /// what it is.
    pub fn poll(&mut self) -> Vec<PlaylistEvent> {
        match self {
            Self::List(list) => list.poll(),
            _ => Vec::new(),
        }
    }

    /// Which track of a playlist is playing, for the pane and the status line.
    pub fn track(&self) -> Option<TrackPosition> {
        match self {
            Self::List(list) => list.track(),
            _ => None,
        }
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
            // A playlist is over when nothing is sounding and nothing is left
            // to start — not when a track runs out, which happens between
            // every pair of them.
            Self::List(list) => list.is_finished(),
            // A paused player is not an empty one, so this stays false while
            // the user is holding the file mid-sentence.
            Self::Decoded { player, .. } => player.empty(),
            Self::Process(child) => matches!(child.try_wait(), Ok(Some(_)) | Err(_)),
        }
    }

    pub fn stop(&mut self) {
        match self {
            Self::List(list) => list.stop(),
            Self::Decoded { player, .. } => player.stop(),
            Self::Process(child) => {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    pub fn is_paused(&self) -> bool {
        match self {
            Self::List(list) => list.paused,
            Self::Decoded { player, .. } => player.is_paused(),
            Self::Process(_) => false,
        }
    }

    pub fn pause(&mut self) {
        match self {
            Self::List(list) => list.pause(),
            Self::Decoded { player, .. } => player.pause(),
            Self::Process(_) => {}
        }
    }

    pub fn resume(&mut self) {
        match self {
            Self::List(list) => list.resume(),
            Self::Decoded { player, .. } => player.play(),
            Self::Process(_) => {}
        }
    }

    /// Where playback has reached, counting from the start of the file.
    pub fn position(&self) -> Duration {
        match self {
            // Within the current track, which is what the countdown beside it
            // is counting down.
            Self::List(list) => list.position(),
            Self::Decoded { player, .. } => player.get_pos(),
            Self::Process(_) => Duration::ZERO,
        }
    }

    pub fn duration(&self) -> Option<Duration> {
        match self {
            Self::List(list) => list.duration(),
            Self::Decoded { duration, .. } => *duration,
            Self::Process(_) => None,
        }
    }

    /// Rewinds by [`SKIP_BACK`], stopping at the beginning rather than
    /// wrapping round or failing when there is less than that to rewind.
    pub fn skip_back(&mut self) -> Result<()> {
        let player = match self {
            Self::List(list) => return list.skip_back(),
            Self::Decoded { player, .. } => player,
            Self::Process(_) => {
                bail!("this voice cannot be rewound; stop it and start again")
            }
        };
        let target = player.get_pos().saturating_sub(SKIP_BACK);
        player
            .try_seek(target)
            .map_err(|e| anyhow::anyhow!("could not skip back in this file: {e}"))
    }
}

/// A playlist, playing.
///
/// One output device, and a `rodio::Player` per track connected to its mixer.
/// A player each rather than one queue of sources is what makes the overlap
/// possible at all: two players on the same mixer are heard together, and only
/// separate players can be at different volumes while they are.
///
/// Nothing here runs on a timer. [`Self::poll`] is called once a frame by the
/// window, which is the same clock the position readout already runs on, so a
/// track boundary is noticed within a frame of it happening and the app never
/// grows a thread that could outlive the sound it is watching.
pub struct ListPlayback {
    // Held only to keep the output device open for as long as any of the
    // players connected to its mixer.
    device: rodio::MixerDeviceSink,
    list: crate::playlist::Playlist,
    /// The track the transport reports on. `None` before the first has
    /// started, and between a track ending and the next one being opened.
    current: Option<Sounding>,
    /// Music that has already started underneath [`Self::current`].
    ahead: Option<Sounding>,
    /// The next track to open. Equal to the playlist's length once every track
    /// has had its turn.
    next: usize,
    /// Held here rather than read off a player: the two players are paused
    /// separately and a playlist between tracks has neither.
    paused: bool,
}

/// One track, sounding.
struct Sounding {
    at: usize,
    player: rodio::Player,
    duration: Option<Duration>,
    /// How long this track took to come up to full volume, which is how much
    /// of the track before it this one started underneath. Zero for every
    /// track that began in silence — see [`ListPlayback::sound`]. Kept for the
    /// tests, which is the only place the join can be inspected rather than
    /// heard.
    #[cfg(test)]
    fade: Duration,
}

impl ListPlayback {
    fn open(list: crate::playlist::Playlist) -> Result<Self> {
        let mut device = rodio::DeviceSinkBuilder::open_default_sink()
            .context("could not open an audio output device")?;
        device.log_on_drop(false);
        Ok(Self {
            device,
            list,
            current: None,
            ahead: None,
            next: 0,
            paused: false,
        })
    }

    /// Moves the playlist along by however much has happened since last time.
    ///
    /// Two things can happen at a track boundary and both are handled here so
    /// that neither depends on being noticed in the same frame: a track that
    /// has run out hands over, and a music track that is due comes in
    /// underneath the speech it follows.
    fn poll(&mut self) -> Vec<PlaylistEvent> {
        let mut events = Vec::new();
        if self.paused {
            // A paused playlist is not advancing, and must not start the next
            // track underneath a track that is being held mid-sentence.
            return events;
        }

        if self
            .current
            .as_ref()
            .is_some_and(|track| track.player.empty())
        {
            // Whatever was already sounding under it takes over, so the music
            // is not restarted at the moment the speech ends.
            self.current = self.ahead.take();
        }
        if self.current.is_none() {
            self.start_next(&mut events);
        }
        self.begin_overlap(&mut events);
        events
    }

    /// Opens tracks until one of them plays, reporting each that did not.
    fn start_next(&mut self, events: &mut Vec<PlaylistEvent>) {
        while self.next < self.list.len() {
            let at = self.next;
            self.next += 1;
            match self.sound(at, Duration::ZERO) {
                Ok(sounding) => {
                    events.push(PlaylistEvent::Started(self.position_of(at)));
                    self.current = Some(sounding);
                    return;
                }
                Err(error) => events.push(PlaylistEvent::Skipped {
                    name: self.name_of(at),
                    error: format!("{error:#}"),
                }),
            }
        }
    }

    /// Starts the next track early, under the one playing, when it is music
    /// following speech and the speech is [`OVERLAP`] from its end.
    ///
    /// The music comes up in however much of the speech is actually left,
    /// which is [`OVERLAP`] when a poll lands where it should and less when
    /// one lands late. A window that is not being redrawn — minimised, or
    /// behind something — is not polled on any schedule worth relying on, and
    /// the difference between a short fade and a full one is far less than the
    /// difference between either and a fade that begins after the speech has
    /// already stopped.
    ///
    /// Nothing happens here when the speech track's length is unknown, since
    /// there is then no end to count back from. The music follows it cleanly
    /// instead, which is the same join every other pair of tracks gets.
    fn begin_overlap(&mut self, events: &mut Vec<PlaylistEvent>) {
        use crate::playlist::TrackKind;

        if self.ahead.is_some() || self.next >= self.list.len() {
            return;
        }
        let Some(current) = &self.current else {
            return;
        };
        let (Some(speech), Some(music)) = (self.list.track(current.at), self.list.track(self.next))
        else {
            return;
        };
        if speech.kind != TrackKind::Speech || music.kind != TrackKind::Music {
            return;
        }
        let Some(total) = current.duration else {
            return;
        };
        let lead = total.saturating_sub(current.player.get_pos());
        if lead > OVERLAP {
            return;
        }

        let at = self.next;
        self.next += 1;
        match self.sound(at, lead) {
            Ok(sounding) => {
                events.push(PlaylistEvent::Started(self.position_of(at)));
                self.ahead = Some(sounding);
            }
            Err(error) => events.push(PlaylistEvent::Skipped {
                name: self.name_of(at),
                error: format!("{error:#}"),
            }),
        }
    }

    /// Decompresses one track and starts it on its own player.
    ///
    /// `fade` is how much of the previous track this one is starting
    /// underneath, and so how long it has to come up in. It is zero for a
    /// track that is taking over from one that has already stopped, because a
    /// fade with nothing playing under it is not a transition — it is silence,
    /// and a listener cannot tell that kind of silence from a playlist that
    /// has died. Only music coming up under speech is ever given one.
    fn sound(&mut self, at: usize, fade: Duration) -> Result<Sounding> {
        use rodio::Source as _;

        let name = self.name_of(at);
        let bytes = self.list.read(at)?;
        let byte_len = bytes.len() as u64;
        let decoder = rodio::Decoder::builder()
            .with_data(Cursor::new(bytes))
            // Both are what make seeking work, and what lets a track report a
            // length for the countdown — and for the overlap, which is worked
            // out from how much of the speech is left.
            .with_byte_len(byte_len)
            .with_seekable(true)
            .build()
            .with_context(|| format!("{name} could not be played"))?;
        let duration = decoder.total_duration();

        let player = rodio::Player::connect_new(self.device.mixer());
        if fade.is_zero() {
            player.append(decoder);
        } else {
            player.append(decoder.fade_in(fade));
        }
        Ok(Sounding {
            at,
            player,
            duration,
            #[cfg(test)]
            fade,
        })
    }

    fn name_of(&self, at: usize) -> String {
        self.list
            .track(at)
            .map(|track| track.name.clone())
            .unwrap_or_default()
    }

    fn position_of(&self, at: usize) -> TrackPosition {
        TrackPosition {
            number: at + 1,
            total: self.list.len(),
            name: self.name_of(at),
        }
    }

    fn track(&self) -> Option<TrackPosition> {
        self.current
            .as_ref()
            .map(|current| self.position_of(current.at))
    }

    fn is_finished(&self) -> bool {
        self.current.is_none() && self.ahead.is_none() && self.next >= self.list.len()
    }

    fn stop(&mut self) {
        for sounding in [self.current.take(), self.ahead.take()]
            .into_iter()
            .flatten()
        {
            sounding.player.stop();
        }
        // So that a stopped playlist reports itself finished rather than
        // starting again from wherever it had got to on the next poll.
        self.next = self.list.len();
        self.paused = false;
    }

    fn pause(&mut self) {
        self.paused = true;
        for sounding in [&self.current, &self.ahead].into_iter().flatten() {
            sounding.player.pause();
        }
    }

    fn resume(&mut self) {
        self.paused = false;
        for sounding in [&self.current, &self.ahead].into_iter().flatten() {
            sounding.player.play();
        }
    }

    fn position(&self) -> Duration {
        self.current
            .as_ref()
            .map_or(Duration::ZERO, |current| current.player.get_pos())
    }

    fn duration(&self) -> Option<Duration> {
        self.current.as_ref().and_then(|current| current.duration)
    }

    /// Back ten seconds within the current track.
    ///
    /// Music that had already come up underneath is taken away again: the
    /// speech it was fading under is no longer ending, and leaving it there
    /// would put the whole rest of the playlist a track ahead of itself. It
    /// starts again, from silence, when the speech next reaches its end.
    fn skip_back(&mut self) -> Result<()> {
        let Some(current) = &self.current else {
            bail!("there is nothing playing to rewind");
        };
        let target = current.player.get_pos().saturating_sub(SKIP_BACK);
        current
            .player
            .try_seek(target)
            .map_err(|e| anyhow::anyhow!("could not skip back in this track: {e}"))?;

        if let Some(ahead) = self.ahead.take() {
            ahead.player.stop();
            self.next = ahead.at;
        }
        Ok(())
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
    // Each half is counted separately and then joined, rather than written out
    // as eight cases: a language with more plural forms than English would need
    // far more than eight, and no translator should have to write them.
    match (minutes, seconds) {
        (0, s) => crate::tn!("time.seconds", s),
        (m, 0) => crate::tn!("time.minutes", m),
        (m, s) => crate::t!(
            "time.both",
            minutes = crate::tn!("time.minutes", m),
            seconds = crate::tn!("time.seconds", s)
        ),
    }
}

/// How long is left of a file that has reached `position` out of `total`.
///
/// Saturating rather than subtracting: a decoder's estimated length and its
/// reported position come from different places and the position can cross it
/// by a few milliseconds at the very end, which as a plain subtraction would
/// underflow and show a countdown of several hundred thousand years.
pub fn time_left(position: Duration, total: Duration) -> Duration {
    total.saturating_sub(position)
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

    /// The countdown under the transport, worked out without an output device
    /// — the arithmetic is the part that can be wrong.
    #[test]
    fn the_countdown_runs_down_to_nothing_and_stops_there() {
        let left = |position, total| {
            spoken_time(time_left(
                Duration::from_secs(position),
                Duration::from_secs(total),
            ))
        };
        assert_eq!(left(0, 180), "3 minutes");
        assert_eq!(left(60, 180), "2 minutes");
        assert_eq!(left(179, 180), "1 second");
        assert_eq!(left(180, 180), "0 seconds");
        // A position past the decoder's estimate reads as finished rather than
        // wrapping round to a countdown of geological length.
        assert_eq!(left(181, 180), "0 seconds");
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

    /// True on the GitHub Actions Windows runner, which cannot survive a
    /// second test opening an output device.
    ///
    /// This suite had exactly one test that opened a real output device for as
    /// long as it has existed, and the cue test below is the second. On
    /// `windows-latest` that combination kills the process with
    /// `STATUS_ACCESS_VIOLATION` — a native crash, so there is no panic, no
    /// named failing test, and no "test result" line to read. The one that
    /// dies is the transport test, *after* this one has opened a device and
    /// dropped it again on a different thread.
    ///
    /// It is not the two of them overlapping. A cross-test mutex was tried
    /// first and the run crashed in exactly the same place, which is what
    /// rules that out. What is left is the harness giving every test its own
    /// short-lived thread, and a WASAPI stream torn down as one of those
    /// threads exits leaving the next test to open a device on something no
    /// longer valid — which is a property of the harness, not of the app,
    /// where every device is opened and dropped on the one UI thread.
    ///
    /// So this runs everywhere except there, including on a real Windows
    /// machine, which is where it is worth running. Same reasoning, and the
    /// same journey from serializing to skipping, as the DPAPI tests that used
    /// to live in `keychain.rs`.
    fn skip_on_windows_ci() -> bool {
        cfg!(windows) && std::env::var_os("CI").is_some()
    }

    /// A cue on a real output device, which is the one thing the decode tests
    /// in `app` cannot cover: the sounds are built into the binary and played
    /// through their own sink, and a failure to open one is swallowed into the
    /// log on purpose so it can never interrupt what the user is being told.
    /// Skipped where there is no audio hardware, as below.
    #[test]
    fn a_cue_plays_on_a_real_device_and_finishes_by_itself() {
        const CUE: &[u8] = include_bytes!("../assets/sounds/error.wav");
        if skip_on_windows_ci() {
            return;
        }

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
        // The countdown the player shows comes off this same pair of numbers,
        // so a file half a second into twenty seconds has most of itself left.
        let total = playback.duration().expect("the length should be known");
        let left = time_left(held, total);
        assert!(
            left > Duration::from_secs(18) && left < total,
            "a file paused at {held:?} of {total:?} reported {left:?} left"
        );
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

    /// A playlist on a real output device: the running order is followed, and
    /// music that follows speech comes up *underneath* it rather than after it.
    ///
    /// The overlap is the part worth a real device. It is not a property of
    /// any one decoder — it is two players on one mixer, and whether the
    /// second was started while the first was still sounding. Skipped where
    /// there is no audio hardware, and on the Windows CI runner, for the
    /// reasons set out above.
    #[test]
    fn a_playlist_plays_in_order_with_the_music_coming_up_under_the_speech() {
        use std::io::Write as _;

        if skip_on_windows_ci() {
            return;
        }
        let path = std::env::temp_dir().join("soe-playlist-test.zip");
        let speech = std::env::temp_dir().join("soe-playlist-speech.wav");
        let music = std::env::temp_dir().join("soe-playlist-music.wav");
        write_wav(&speech, &tone(1, 22_050, 2.0)).unwrap();
        write_wav(&music, &tone(1, 22_050, 1.0)).unwrap();

        let file = std::fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options: zip::write::FileOptions<()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
        // Written the other way round on purpose: the manifest decides the
        // order, not the order the archive happens to store its entries in.
        for (name, from) in [("music.wav", &music), ("speech.wav", &speech)] {
            zip.start_file(name, options).unwrap();
            zip.write_all(&std::fs::read(from).unwrap()).unwrap();
        }
        zip.start_file("media.txt", options).unwrap();
        zip.write_all(
            br#"<music>
                <audio type="speech" pos="1">speech.wav</audio>
                <audio type="music" pos="2">music.wav</audio>
            </music>"#,
        )
        .unwrap();
        zip.finish().unwrap();
        std::fs::remove_file(&speech).ok();
        std::fs::remove_file(&music).ok();

        let tidy_up = || {
            std::fs::remove_file(&path).ok();
        };
        let Ok((mut playback, started)) = Playback::play_playlist(&path) else {
            tidy_up();
            eprintln!("no audio output device; skipping the playlist test");
            return;
        };

        let names: Vec<String> = started
            .iter()
            .filter_map(|event| match event {
                PlaylistEvent::Started(track) => Some(track.name.clone()),
                PlaylistEvent::Skipped { .. } => None,
            })
            .collect();
        assert_eq!(
            names,
            ["speech.wav"],
            "the manifest's order was not followed"
        );
        assert_eq!(
            playback.track().map(|track| (track.number, track.total)),
            Some((1, 2))
        );

        // Poll the way the window does, and catch the moment the music starts.
        let mut overlapped = None;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            for event in playback.poll() {
                if let PlaylistEvent::Started(track) = event
                    && track.number == 2
                {
                    // What makes it an overlap rather than a hand-over: the
                    // speech is still the track the transport is reporting on,
                    // and is still short of its own end.
                    let fade = match &playback {
                        Playback::List(list) => list.ahead.as_ref().map(|ahead| ahead.fade),
                        _ => None,
                    };
                    overlapped = Some((
                        playback.track().map(|track| track.number),
                        playback.position(),
                        playback.duration(),
                        fade,
                    ));
                }
            }
            if overlapped.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        let Some((reporting, position, duration, fade)) = overlapped else {
            playback.stop();
            tidy_up();
            panic!("the music never started");
        };
        assert_eq!(
            reporting,
            Some(1),
            "the music took over instead of coming up under the speech"
        );
        let total = duration.expect("a WAV reports its length");
        let left = time_left(position, total);
        assert!(
            left > Duration::ZERO && left <= OVERLAP + Duration::from_millis(250),
            "the music came up with {left:?} of the speech left, not about {OVERLAP:?}"
        );
        // And it comes up in the time it actually has, so that it is at full
        // volume as the speech runs out rather than still climbing.
        let fade = fade.expect("the music was started ahead of the speech");
        assert!(
            fade > Duration::ZERO && fade <= OVERLAP,
            "the music faded in over {fade:?}, which is not the {left:?} it had"
        );

        // And the music is what is left playing once the speech has run out.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline
            && playback.track().map(|track| track.number) != Some(2)
        {
            playback.poll();
            std::thread::sleep(Duration::from_millis(20));
        }
        let handed_over = playback.track().map(|track| track.name);
        playback.stop();
        tidy_up();
        assert_eq!(handed_over.as_deref(), Some("music.wav"));
    }

    /// The join when the overlap never had its chance.
    ///
    /// A playlist only moves between tracks when the window is polled, and a
    /// window that is minimised or hidden behind another is not redrawn on any
    /// schedule worth relying on. So the poll that ought to land in the last
    /// [`OVERLAP`] of the speech can land after the speech has already
    /// stopped — and when it does, the music has to start at full volume.
    /// Fading it up from silence there is a second of nothing on top of a
    /// second of nothing, and a listener who cannot see the window has no way
    /// to tell that from a playlist that has quietly died.
    #[test]
    fn music_that_missed_its_overlap_starts_at_full_volume_instead_of_fading_up_from_silence() {
        use std::io::Write as _;

        if skip_on_windows_ci() {
            return;
        }
        let dir = std::env::temp_dir();
        let path = dir.join("soe-late-poll-test.zip");
        let speech = dir.join("soe-late-poll-speech.wav");
        let music = dir.join("soe-late-poll-music.wav");
        // Comfortably longer than `OVERLAP`, so that the music is not due to
        // come up at the moment the speech starts.
        write_wav(&speech, &tone(1, 22_050, 2.5)).unwrap();
        write_wav(&music, &tone(1, 22_050, 2.0)).unwrap();

        let file = std::fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options: zip::write::FileOptions<()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (name, from) in [("speech.wav", &speech), ("music.wav", &music)] {
            zip.start_file(name, options).unwrap();
            zip.write_all(&std::fs::read(from).unwrap()).unwrap();
        }
        zip.start_file("media.txt", options).unwrap();
        zip.write_all(
            br#"<music>
                <audio type="speech" pos="1">speech.wav</audio>
                <audio type="music" pos="2">music.wav</audio>
            </music>"#,
        )
        .unwrap();
        zip.finish().unwrap();
        std::fs::remove_file(&speech).ok();
        std::fs::remove_file(&music).ok();

        let tidy_up = || {
            std::fs::remove_file(&path).ok();
        };
        let Ok((mut playback, _)) = Playback::play_playlist(&path) else {
            tidy_up();
            eprintln!("no audio output device; skipping the late-poll test");
            return;
        };

        // Not polled at all until the speech is well and truly over, which is
        // the whole point: this is the frame that never came.
        std::thread::sleep(Duration::from_millis(3000));
        playback.poll();

        let handed_over = playback.track().map(|track| track.name);
        let fade = match &playback {
            Playback::List(list) => list.current.as_ref().map(|current| current.fade),
            _ => None,
        };
        playback.stop();
        tidy_up();

        assert_eq!(
            handed_over.as_deref(),
            Some("music.wav"),
            "the music never took over"
        );
        assert_eq!(
            fade,
            Some(Duration::ZERO),
            "the music faded up from silence after the speech had already stopped"
        );
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
