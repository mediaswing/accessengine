//! The operating system's own voices, via the `tts` crate.
//!
//! Backends differ in what they support, so this wrapper normalises three
//! things: how a finished utterance is detected, how rate/pitch/volume ranges
//! are expressed, and what happens when a feature is simply missing.

use anyhow::{Context, Result};
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};
use tts::{Features, Tts};

use super::{map_pos, pos_of};

/// A voice offered by the OS.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SystemVoice {
    pub id: String,
    pub name: String,
    pub language: String,
}

/// How the engine knows an utterance has ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tracking {
    /// The back end calls us back. Exact.
    Callback,
    /// We ask "still talking?" each frame. Needs a guard against the race
    /// between `speak` returning and the audio actually starting.
    Poll,
    /// Neither is available: queue the whole document and let it run.
    None,
}

/// How long after `speak()` to ignore `is_speaking()`, which reports false on
/// several back ends until the audio device has actually opened.
const POLL_GRACE: Duration = Duration::from_millis(400);

pub struct SystemEngine {
    tts: Tts,
    features: Features,
    tracking: Tracking,
    end_rx: Receiver<()>,
    pub voices: Vec<SystemVoice>,
    /// Set while an utterance we started is believed to be in flight.
    in_flight: bool,
    spoke_at: Option<Instant>,
    /// Ranges reported by the back end, cached for the sliders.
    pub rate_range: (f32, f32, f32),
    pub pitch_range: (f32, f32, f32),
    pub volume_range: (f32, f32, f32),
}

impl SystemEngine {
    /// Build the engine. `repaint` is called from the speech callback so the
    /// UI wakes up the instant a sentence finishes rather than at the next
    /// frame it happens to draw.
    pub fn new(repaint: impl Fn() + Send + 'static) -> Result<Self> {
        let tts = Tts::default().context("no system speech synthesiser is available")?;
        let features = tts.supported_features();
        log::info!("system speech features: {features:?}");

        let (tx, end_rx) = channel();
        let mut tracking = Tracking::None;
        if features.utterance_callbacks {
            let result = tts.on_utterance_end(Some(Box::new(move |_id| {
                let _ = tx.send(());
                repaint();
            })));
            match result {
                Ok(()) => tracking = Tracking::Callback,
                Err(e) => log::warn!("utterance callbacks advertised but refused: {e}"),
            }
        }
        if tracking == Tracking::None && features.is_speaking {
            tracking = Tracking::Poll;
        }
        log::info!("system speech progress tracking: {tracking:?}");

        let voices = match tts.voices() {
            Ok(list) => list
                .into_iter()
                .map(|v| SystemVoice {
                    id: v.id(),
                    name: v.name(),
                    language: v.language().to_string(),
                })
                .collect::<Vec<_>>(),
            Err(e) => {
                log::warn!("could not enumerate system voices: {e}");
                Vec::new()
            }
        };
        log::info!("found {} system voices", voices.len());

        Ok(Self {
            rate_range: (tts.min_rate(), tts.normal_rate(), tts.max_rate()),
            pitch_range: (tts.min_pitch(), tts.normal_pitch(), tts.max_pitch()),
            volume_range: (tts.min_volume(), tts.normal_volume(), tts.max_volume()),
            tts,
            features,
            tracking,
            end_rx,
            voices,
            in_flight: false,
            spoke_at: None,
        })
    }

    /// True when this back end can report per-sentence progress. When false the
    /// UI hides the progress bar rather than showing something untrue.
    pub fn tracks_progress(&self) -> bool {
        self.tracking != Tracking::None
    }

    pub fn supports_rate(&self) -> bool {
        self.features.rate
    }

    pub fn supports_pitch(&self) -> bool {
        self.features.pitch
    }

    pub fn supports_volume(&self) -> bool {
        self.features.volume
    }

    /// Slider position corresponding to the back end's own default.
    pub fn default_rate_pos(&self) -> f32 {
        pos_of(self.rate_range.1, self.rate_range.0, self.rate_range.2)
    }

    pub fn default_pitch_pos(&self) -> f32 {
        pos_of(self.pitch_range.1, self.pitch_range.0, self.pitch_range.2)
    }

    pub fn set_voice_by_id(&mut self, id: &str) -> Result<()> {
        if !self.features.voice {
            return Ok(());
        }
        let voices = self.tts.voices().unwrap_or_default();
        let Some(voice) = voices.into_iter().find(|v| v.id() == id) else {
            log::warn!("voice {id} is no longer installed; keeping the current one");
            return Ok(());
        };
        self.tts.set_voice(&voice).context("selecting voice")?;
        log::info!("system voice set to {id}");
        Ok(())
    }

    /// Apply the slider positions. Unsupported parameters are skipped quietly:
    /// the UI already hides those sliders.
    pub fn apply_settings(&mut self, rate_pos: Option<f32>, pitch_pos: Option<f32>, volume: f32) {
        if self.features.rate {
            let (min, normal, max) = self.rate_range;
            let value = rate_pos.map_or(normal, |p| map_pos(p, min, max));
            if let Err(e) = self.tts.set_rate(value) {
                log::warn!("set_rate({value}) failed: {e}");
            }
        }
        if self.features.pitch {
            let (min, normal, max) = self.pitch_range;
            let value = pitch_pos.map_or(normal, |p| map_pos(p, min, max));
            if let Err(e) = self.tts.set_pitch(value) {
                log::warn!("set_pitch({value}) failed: {e}");
            }
        }
        if self.features.volume {
            let (min, _, max) = self.volume_range;
            let value = map_pos(volume, min, max);
            if let Err(e) = self.tts.set_volume(value) {
                log::warn!("set_volume({value}) failed: {e}");
            }
        }
    }

    /// Speak one chunk, interrupting whatever is in progress.
    pub fn speak(&mut self, text: &str) -> Result<()> {
        self.drain_stale_events();
        self.tts.speak(text, true).context("speaking")?;
        self.in_flight = true;
        self.spoke_at = Some(Instant::now());
        Ok(())
    }

    /// Queue everything at once. Used only when the back end cannot report
    /// progress, where speaking chunk-by-chunk would leave gaps we never close.
    pub fn speak_all(&mut self, texts: &[String]) -> Result<()> {
        self.drain_stale_events();
        let mut first = true;
        for text in texts {
            self.tts
                .speak(text.as_str(), first)
                .context("queueing speech")?;
            first = false;
        }
        self.in_flight = true;
        self.spoke_at = Some(Instant::now());
        Ok(())
    }

    pub fn stop(&mut self) {
        if self.features.stop {
            if let Err(e) = self.tts.stop() {
                log::warn!("stop failed: {e}");
            }
        }
        self.in_flight = false;
        self.spoke_at = None;
        self.drain_stale_events();
    }

    /// Events that arrived for an utterance we have already moved past would
    /// otherwise advance the next sentence early.
    fn drain_stale_events(&mut self) {
        while self.end_rx.try_recv().is_ok() {}
    }

    /// Call once per frame. Returns true when the current utterance has ended.
    pub fn poll_finished(&mut self) -> bool {
        if !self.in_flight {
            return false;
        }
        match self.tracking {
            Tracking::Callback => {
                if self.end_rx.try_recv().is_ok() {
                    self.in_flight = false;
                    return true;
                }
                false
            }
            Tracking::Poll => {
                // Ignore the window where the back end has not started yet.
                if self.spoke_at.is_some_and(|t| t.elapsed() < POLL_GRACE) {
                    return false;
                }
                match self.tts.is_speaking() {
                    Ok(false) => {
                        self.in_flight = false;
                        true
                    }
                    Ok(true) => false,
                    Err(e) => {
                        log::warn!("is_speaking failed, dropping to untracked playback: {e}");
                        self.tracking = Tracking::None;
                        false
                    }
                }
            }
            Tracking::None => false,
        }
    }
}
