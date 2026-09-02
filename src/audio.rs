//! The two sounds the app makes: one when something worked, one when it did
//! not.
//!
//! Adapted from the same module in `watchspend`, which is where the asset
//! files come from too; see `assets/sounds/CREDITS.txt`.
//!
//! Sound is a courtesy, never a requirement — a machine with no audio device,
//! or one whose device disappears when a dock is unplugged, should still be a
//! perfectly good reader. So every failure in here is swallowed after being
//! logged once.
//!
//! Note this is a *separate* output from the one the ElevenLabs worker holds.
//! Sharing a device sink across threads would mean a cue could not sound while
//! a sentence was playing, and the failure cue is at its most useful precisely
//! when playback has just gone wrong.

use std::io::Cursor;

use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink, Source as _};

const SUCCESS_WAV: &[u8] = include_bytes!("../assets/sounds/success.wav");
const ERROR_WAV: &[u8] = include_bytes!("../assets/sounds/error.wav");

/// Both cues are levelled to the same loudness in the asset files, so a single
/// gain keeps them polite without making the failure sound the quiet one.
const GAIN: f32 = 0.7;

pub struct Sounds {
    /// Holds the audio device open; playback stops when this is dropped.
    ///
    /// Opened when there is first something to play, not at startup. Holding
    /// an output stream open keeps the audio hardware awake and puts the app
    /// in the list of things using it, which is a strange thing to do all
    /// afternoon on the off chance of a click.
    device: Option<MixerDeviceSink>,
    /// Whether opening it has already been tried and failed, so a machine with
    /// no sound card is not asked about it once per button press.
    unavailable: bool,
    /// User preference; the cues are off-putting for some listeners, and this
    /// app's whole job is playing audio the user actually asked for.
    pub enabled: bool,
}

impl Sounds {
    pub fn new(enabled: bool) -> Self {
        Self {
            device: None,
            unavailable: false,
            enabled,
        }
    }

    pub fn success(&mut self) {
        self.play(SUCCESS_WAV);
    }

    pub fn failure(&mut self) {
        self.play(ERROR_WAV);
    }

    fn play(&mut self, wav: &'static [u8]) {
        if !self.enabled {
            return;
        }
        if self.device.is_none() && !self.unavailable {
            match DeviceSinkBuilder::open_default_sink() {
                Ok(device) => self.device = Some(device),
                Err(err) => {
                    log::warn!("no audio output, carrying on in silence: {err}");
                    self.unavailable = true;
                }
            }
        }

        let Some(device) = &self.device else {
            return;
        };
        match Decoder::new_wav(Cursor::new(wav)) {
            // `add` mixes rather than queues, so a quick series of actions
            // does not build up a backlog of sounds playing late.
            Ok(source) => device.mixer().add(source.amplify(GAIN)),
            Err(err) => log::warn!("could not decode a sound effect: {err}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The assets are baked into the binary, so a bad or missing file is a
    /// build-time fact rather than something to discover on the day something
    /// goes wrong and the app has nothing to say about it.
    #[test]
    fn both_cues_decode_to_audible_samples() {
        for (name, wav) in [("success", SUCCESS_WAV), ("error", ERROR_WAV)] {
            let source = Decoder::new_wav(Cursor::new(wav))
                .unwrap_or_else(|e| panic!("{name}.wav does not decode: {e}"));
            assert!(source.count() > 0, "{name}.wav decodes to no samples");
        }
    }

    #[test]
    fn disabled_cues_never_open_the_audio_device() {
        let mut sounds = Sounds::new(false);
        sounds.success();
        sounds.failure();
        assert!(sounds.device.is_none());
    }
}
