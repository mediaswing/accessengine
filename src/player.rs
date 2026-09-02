//! The audio player behind the Audio Player tab.
//!
//! Network-free, but built the same way as the cloud speech worker in
//! [`crate::speech::cloud`]: a worker thread taking commands down one channel
//! and posting events back up another, because decoding and playing must not
//! happen on the frame the UI is drawing.
//!
//! Two things are not like the speech worker.
//!
//! **It owns its own output device**, as the sound cues do — see the note in
//! [`crate::audio`]. A player and a reading are different things a listener
//! may want at once, and sharing one device sink would make each wait for the
//! other.
//!
//! **The playhead is shared state rather than an event.** A position readout
//! wants updating several times a second, and a channel message per tick would
//! be a queue of stale numbers the UI throws away. So the worker writes into a
//! [`Status`] behind a mutex and the UI reads it while it draws; the channel
//! carries only the things that actually happen — a track started, the running
//! order ended, something failed.
//!
//! ## The fade
//!
//! A playlist marks each track music or spoken word, and the one place that
//! matters is the join between them: where music follows speech, it fades up
//! under the end of the speech rather than starting flat afterwards. That is
//! what a bulletin sounds like on the radio, and it is the only transition
//! treated specially — see [`fades_in_after`]. Everything else plays straight
//! through.

use std::io::Cursor;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rodio::Source as _;

use crate::playlist::{Kind, Track};
use crate::speech::PlayState;
use crate::t;

/// How long the music takes to come up under the end of the speech.
///
/// Three seconds is what a radio bed does: long enough to be a fade rather
/// than a cut, short enough that the last words are not buried under it.
const FADE: Duration = Duration::from_secs(3);

/// How often the worker looks at the playhead while something is playing.
/// Fast enough to catch the fade point and to move a position readout
/// smoothly, slow enough to cost nothing.
const TICK: Duration = Duration::from_millis(50);

/// How long after starting a track to disbelieve an empty queue.
///
/// `empty()` reports true on several back ends until the audio device has
/// actually opened, which without this reads as "the track finished instantly"
/// and runs the whole playlist in a second. The same guard the speech worker
/// keeps for the same reason.
const START_GRACE: Duration = Duration::from_millis(750);

/// Whether the next track should come up under the end of this one.
///
/// Only music after speech. Speech after music cuts, because a voice fading in
/// loses its first words; music after music is a running order somebody wrote
/// that way; and speech after speech is two items of a bulletin, which must
/// not overlap at all.
pub fn fades_in_after(current: Kind, next: Kind) -> bool {
    current == Kind::Spoken && next == Kind::Music
}

/// How far into a track of this length the next one should start.
///
/// A track shorter than the fade is overlapped for as long as it lasts rather
/// than skipped or started early: a two-second sting still gets a fade, just a
/// two-second one.
fn fade_point(total: Duration) -> (Duration, Duration) {
    let overlap = FADE.min(total);
    (total.saturating_sub(overlap), overlap)
}

/// What the UI reads while it draws.
#[derive(Clone, Debug)]
pub struct Status {
    pub state: PlayState,
    pub elapsed: Duration,
    /// `None` when the decoder cannot say, which is not the same as zero — the
    /// UI shows a position without a seek bar rather than a bar that lies.
    pub total: Option<Duration>,
}

impl Default for Status {
    fn default() -> Self {
        Self {
            state: PlayState::Idle,
            elapsed: Duration::ZERO,
            total: None,
        }
    }
}

pub enum Command {
    /// Replace the running order and start at `start`.
    Load {
        tracks: Vec<Track>,
        start: usize,
    },
    Resume,
    Pause,
    Stop,
    /// Move by a whole track, forwards or back.
    Skip(isize),
    /// Jump within the track being played.
    Seek(Duration),
    SetVolume(f32),
    Shutdown,
}

#[derive(Debug)]
pub enum Event {
    /// A track began. The UI follows the running order by this rather than by
    /// counting, since a fade means one track starts before the last ends.
    Started(usize),
    /// The running order reached its end.
    Finished,
    Error(String),
}

pub struct Player {
    cmd_tx: Sender<Command>,
    evt_rx: Receiver<Event>,
    status: Arc<Mutex<Status>>,
    worker: Option<JoinHandle<()>>,
}

impl Player {
    pub fn new(repaint: impl Fn() + Send + 'static) -> Self {
        let (cmd_tx, cmd_rx) = channel::<Command>();
        let (evt_tx, evt_rx) = channel::<Event>();
        let status = Arc::new(Mutex::new(Status::default()));
        let worker_status = status.clone();
        let worker = std::thread::Builder::new()
            .name("audio-player".to_string())
            .spawn(move || worker_main(cmd_rx, evt_tx, worker_status, repaint))
            .ok();
        if worker.is_none() {
            log::error!("could not spawn the audio player thread");
        }
        Self {
            cmd_tx,
            evt_rx,
            status,
            worker,
        }
    }

    pub fn send(&self, cmd: Command) {
        if self.cmd_tx.send(cmd).is_err() {
            log::error!("audio player worker is gone; command dropped");
        }
    }

    pub fn try_recv(&self) -> Option<Event> {
        self.evt_rx.try_recv().ok()
    }

    /// The playhead, for drawing. Never blocks the frame on a poisoned lock:
    /// a player that has panicked should leave the rest of the app usable.
    pub fn status(&self) -> Status {
        self.status
            .lock()
            .map(|status| status.clone())
            .unwrap_or_default()
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(Command::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// One track in flight.
struct Playing {
    player: rodio::Player,
    index: usize,
    total: Option<Duration>,
    began: Instant,
    /// Whether audio has actually been heard from it yet — see [`START_GRACE`].
    heard: bool,
}

impl Playing {
    /// Whether this track has played out. Guarded against the window where the
    /// device has not opened and the queue merely looks empty.
    fn finished(&mut self) -> bool {
        if !self.player.empty() {
            self.heard = true;
            return false;
        }
        self.heard || self.began.elapsed() > START_GRACE
    }
}

fn worker_main(
    cmd_rx: Receiver<Command>,
    evt_tx: Sender<Event>,
    status: Arc<Mutex<Status>>,
    repaint: impl Fn(),
) {
    // Opened on first use, not at startup: someone who never opens the player
    // should not have this app holding their sound card open all afternoon.
    let mut device: Option<rodio::MixerDeviceSink> = None;
    let mut tracks: Vec<Track> = Vec::new();
    let mut current: Option<Playing> = None;
    // Where the running order got to. Kept because `current` is cleared by
    // Stop, by the end of the list and by a track that will not decode, and
    // Skip still has to mean "the one after the one you were on" afterwards
    // rather than silently meaning "the one after the first".
    let mut last_index: usize = 0;
    // The next track, already begun under the tail of this one.
    let mut faded: Option<Playing> = None;
    let mut volume = 1.0f32;

    let emit = |event: Event| {
        let _ = evt_tx.send(event);
        repaint();
    };
    let publish = |status: &Arc<Mutex<Status>>, next: Status| {
        if let Ok(mut held) = status.lock() {
            *held = next;
        }
    };

    loop {
        // Block while idle, poll while playing.
        let wait = if current.is_some() {
            TICK
        } else {
            Duration::from_millis(400)
        };
        match cmd_rx.recv_timeout(wait) {
            Ok(Command::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(Command::Load {
                tracks: loaded,
                start,
            }) => {
                stop_all(&mut current, &mut faded);
                tracks = loaded;
                if device.is_none() {
                    match rodio::DeviceSinkBuilder::open_default_sink() {
                        Ok(opened) => device = Some(opened),
                        Err(e) => {
                            log::error!("opening audio output: {e:#}");
                            emit(Event::Error(t!(
                                "error.no_audio_device",
                                reason = format!("{e}")
                            )));
                            continue;
                        }
                    }
                }
                if let Some(out) = device.as_ref() {
                    match begin(out, &tracks, start, None, volume) {
                        Ok(playing) => {
                            emit(Event::Started(playing.index));
                            last_index = playing.index;
                            current = Some(playing);
                        }
                        Err(e) => {
                            log::error!("starting playback: {e:#}");
                            emit(Event::Error(format!("{e:#}")));
                            // Nothing is playing now, and the status still
                            // says otherwise. Left alone, the tab keeps a
                            // Pause button and a frozen position that no
                            // press can clear.
                            publish(&status, Status::default());
                        }
                    }
                }
            }
            Ok(Command::Resume) => {
                for playing in [current.as_ref(), faded.as_ref()].into_iter().flatten() {
                    playing.player.play();
                }
            }
            Ok(Command::Pause) => {
                for playing in [current.as_ref(), faded.as_ref()].into_iter().flatten() {
                    playing.player.pause();
                }
            }
            Ok(Command::Stop) => {
                if let Some(playing) = current.as_ref() {
                    last_index = playing.index;
                }
                stop_all(&mut current, &mut faded);
                publish(&status, Status::default());
                repaint();
            }
            Ok(Command::Skip(delta)) => {
                let from = current.as_ref().map(|p| p.index).unwrap_or(last_index) as isize;
                let target = (from + delta).clamp(0, tracks.len().saturating_sub(1) as isize);
                stop_all(&mut current, &mut faded);
                if let Some(out) = device.as_ref() {
                    match begin(out, &tracks, target as usize, None, volume) {
                        Ok(playing) => {
                            emit(Event::Started(playing.index));
                            last_index = playing.index;
                            current = Some(playing);
                        }
                        Err(e) => {
                            emit(Event::Error(format!("{e:#}")));
                            publish(&status, Status::default());
                        }
                    }
                }
            }
            Ok(Command::Seek(to)) => {
                if let Some(playing) = current.as_ref() {
                    if let Err(e) = playing.player.try_seek(to) {
                        log::warn!("seeking: {e}");
                        emit(Event::Error(t!("error.cannot_seek")));
                    }
                    // A fade already under way belongs to a position that no
                    // longer exists, so it goes rather than playing over the
                    // middle of the track somebody has just jumped to.
                    if let Some(other) = faded.take() {
                        other.player.stop();
                    }
                }
            }
            Ok(Command::SetVolume(level)) => {
                volume = level.clamp(0.0, 1.0);
                for playing in [current.as_ref(), faded.as_ref()].into_iter().flatten() {
                    playing.player.set_volume(volume);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
        }

        // Advance the running order.
        let Some(playing) = current.as_mut() else {
            continue;
        };

        if playing.finished() {
            let ended = playing.index;
            // The next track may already be playing, faded up under the end of
            // this one; promoting it is what makes the join seamless.
            match faded.take() {
                Some(next) => {
                    emit(Event::Started(next.index));
                    last_index = next.index;
                    current = Some(next);
                }
                None => match ended + 1 {
                    next if next < tracks.len() => {
                        let started = device
                            .as_ref()
                            .map(|out| begin(out, &tracks, next, None, volume));
                        match started {
                            Some(Ok(playing)) => {
                                emit(Event::Started(playing.index));
                                last_index = playing.index;
                                current = Some(playing);
                            }
                            Some(Err(e)) => {
                                log::error!("starting the next track: {e:#}");
                                emit(Event::Error(format!("{e:#}")));
                                last_index = next;
                                current = None;
                                publish(&status, Status::default());
                            }
                            None => {
                                last_index = ended;
                                current = None;
                                publish(&status, Status::default());
                            }
                        }
                    }
                    _ => {
                        last_index = ended;
                        current = None;
                        publish(&status, Status::default());
                        emit(Event::Finished);
                    }
                },
            }
            continue;
        }

        // Should the next track start coming up under this one?
        if faded.is_none() {
            if let Some(next) = start_of_fade(&tracks, playing) {
                let (_, overlap) = next;
                let index = playing.index + 1;
                if let Some(out) = device.as_ref() {
                    match begin(out, &tracks, index, Some(overlap), volume) {
                        Ok(started) => {
                            // Worth a line: this is the one bit of playback
                            // behaviour that is not simply "play the next
                            // thing", and it is inaudible in a log otherwise.
                            log::debug!(
                                "fading track {} in over {:.1}s under the end of track {}",
                                index + 1,
                                overlap.as_secs_f32(),
                                playing.index + 1
                            );
                            faded = Some(started);
                        }
                        // A fade that cannot start is not worth stopping the
                        // running order for: the track will play on its own
                        // when this one ends.
                        Err(e) => log::warn!("could not fade in the next track: {e:#}"),
                    }
                }
            }
        }

        let paused = playing.player.is_paused();
        publish(
            &status,
            Status {
                state: if paused {
                    PlayState::Paused
                } else {
                    PlayState::Playing
                },
                elapsed: playing.player.get_pos(),
                total: playing.total,
            },
        );
        repaint();
    }

    stop_all(&mut current, &mut faded);
    log::info!("audio player stopped");
}

/// Whether the track after this one should be started now, and over how long.
///
/// `None` unless every condition holds: there is a next track, it is music
/// after speech, this track's length is known, and the playhead has reached
/// the point where the overlap begins.
fn start_of_fade(tracks: &[Track], playing: &Playing) -> Option<(usize, Duration)> {
    let next = playing.index + 1;
    let current_kind = tracks.get(playing.index)?.kind;
    let next_kind = tracks.get(next)?.kind;
    if !fades_in_after(current_kind, next_kind) {
        return None;
    }
    // A decoder that cannot say how long the track is cannot say when it ends,
    // so the join is a plain cut rather than a fade at a guessed moment.
    let total = playing.total?;
    let (at, overlap) = fade_point(total);
    (playing.player.get_pos() >= at).then_some((next, overlap))
}

/// Start one track, optionally fading it in.
fn begin(
    device: &rodio::MixerDeviceSink,
    tracks: &[Track],
    index: usize,
    fade: Option<Duration>,
    volume: f32,
) -> Result<Playing> {
    let track = tracks
        .get(index)
        .with_context(|| t!("error.track_missing"))?;
    let bytes = track.origin.read()?;
    let decoder = rodio::Decoder::try_from(Cursor::new(bytes))
        .with_context(|| t!("error.cannot_decode", name = track.name()))?;
    let total = decoder.total_duration();

    let player = rodio::Player::connect_new(device.mixer());
    player.set_volume(volume.clamp(0.0, 1.0));
    match fade {
        Some(over) => player.append(decoder.fade_in(over)),
        None => player.append(decoder),
    }
    player.play();

    Ok(Playing {
        player,
        index,
        total,
        began: Instant::now(),
        heard: false,
    })
}

fn stop_all(current: &mut Option<Playing>, faded: &mut Option<Playing>) {
    for playing in [current.take(), faded.take()].into_iter().flatten() {
        playing.player.stop();
    }
}

/// A duration as `3:07`, or `1:02:33` once it runs past an hour.
pub fn clock(d: Duration) -> String {
    let seconds = d.as_secs();
    let (hours, minutes, seconds) = (seconds / 3600, (seconds % 3600) / 60, seconds % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one join that is not a straight cut, and the three that are.
    #[test]
    fn only_music_after_speech_fades_in() {
        assert!(fades_in_after(Kind::Spoken, Kind::Music));
        // A voice fading in loses its first words.
        assert!(!fades_in_after(Kind::Music, Kind::Spoken));
        // Two items of a bulletin must never overlap.
        assert!(!fades_in_after(Kind::Spoken, Kind::Spoken));
        // A running order somebody wrote that way is played that way.
        assert!(!fades_in_after(Kind::Music, Kind::Music));
    }

    #[test]
    fn the_fade_starts_a_fade_length_before_the_end() {
        let (at, overlap) = fade_point(Duration::from_secs(60));
        assert_eq!(at, Duration::from_secs(57));
        assert_eq!(overlap, FADE);
    }

    /// A sting shorter than the fade still gets one, over its own length,
    /// rather than being started before it exists or not faded at all.
    #[test]
    fn a_track_shorter_than_the_fade_is_overlapped_for_as_long_as_it_lasts() {
        let (at, overlap) = fade_point(Duration::from_secs(2));
        assert_eq!(at, Duration::ZERO);
        assert_eq!(overlap, Duration::from_secs(2));

        let (at, overlap) = fade_point(Duration::ZERO);
        assert_eq!(at, Duration::ZERO);
        assert_eq!(overlap, Duration::ZERO);
    }

    #[test]
    fn the_clock_reads_the_way_a_player_shows_it() {
        assert_eq!(clock(Duration::from_secs(0)), "0:00");
        assert_eq!(clock(Duration::from_secs(7)), "0:07");
        assert_eq!(clock(Duration::from_secs(187)), "3:07");
        assert_eq!(clock(Duration::from_secs(3753)), "1:02:33");
        assert_eq!(clock(Duration::from_secs(36000)), "10:00:00");
    }
}
