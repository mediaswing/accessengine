//! Speech synthesis back ends.
//!
//! Two shapes of engine, deliberately different:
//!
//! * [`system`] wraps the OS synthesiser. It has to be driven from the main
//!   thread (AVSpeechSynthesizer and SAPI both insist), so the app pumps it
//!   once per frame.
//! * [`cloud`] is network-bound, so it owns a worker thread and reports back
//!   over a channel. Every hosted provider shares that one worker: the queue,
//!   the prefetch and the audio device are the same work whoever is being
//!   asked, and only the request itself differs. What differs lives in a
//!   module of its own — [`elevenlabs`], [`openai`], [`deepgram`], [`google`],
//!   [`polly`] — each of which knows how to synthesise one chunk and how to
//!   list the voices on the account, and nothing else.
//!
//! Both report progress as an index into the *speech plan* — the list of
//! chunks actually being spoken, which is not the same as the list of chunks
//! in the document once a wordlist has skipped some.

pub mod cloud;
pub mod deepgram;
pub mod elevenlabs;
pub mod google;
pub mod openai;
pub mod polly;
pub mod system;

use serde::{Deserialize, Deserializer, Serialize};

use crate::t;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum EngineKind {
    System,
    ElevenLabs,
    #[serde(rename = "OpenAI")]
    OpenAi,
    Deepgram,
    Google,
    Polly,
}

/// Read leniently, so a settings file naming an engine this build has never
/// heard of falls back to the system voices rather than taking the rest of the
/// file down with it.
///
/// The derived implementation would fail the whole `Config`, and `Config::load`
/// answers a parse failure with `Default::default()` — so one unknown word
/// would cost the user every other setting they have. That is the shape a
/// downgrade takes: settings written by a newer build, opened by an older one.
impl<'de> Deserialize<'de> for EngineKind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        Ok(match name.as_str() {
            "System" => Self::System,
            "ElevenLabs" => Self::ElevenLabs,
            "OpenAI" | "OpenAi" => Self::OpenAi,
            "Deepgram" => Self::Deepgram,
            "Google" => Self::Google,
            "Polly" => Self::Polly,
            other => {
                log::warn!("settings name a speech engine this build has not got ({other}); using the system voices");
                Self::System
            }
        })
    }
}

impl EngineKind {
    pub const ALL: [EngineKind; 6] = [
        EngineKind::System,
        EngineKind::ElevenLabs,
        EngineKind::OpenAi,
        EngineKind::Deepgram,
        EngineKind::Google,
        EngineKind::Polly,
    ];

    pub fn label(&self) -> String {
        match self {
            EngineKind::System => t!("engine.system"),
            EngineKind::ElevenLabs => t!("engine.elevenlabs"),
            EngineKind::OpenAi => t!("engine.openai"),
            EngineKind::Deepgram => t!("engine.deepgram"),
            EngineKind::Google => t!("engine.google"),
            EngineKind::Polly => t!("engine.polly"),
        }
    }

    /// Whether this engine sends the text somewhere else to be spoken.
    ///
    /// The one question most of the app actually wants to ask: everything that
    /// used to be "is this ElevenLabs" — the worker, the key, saving to MP3 —
    /// is really this.
    pub fn is_cloud(&self) -> bool {
        !matches!(self, EngineKind::System)
    }

    /// What the provider calls itself, for error messages and the log.
    ///
    /// Not translated, and not a [`EngineKind::label`]: these are company
    /// names, they appear in messages built off the UI thread, and a bug
    /// report is easier to read when the service in it is spelled the way its
    /// own documentation spells it.
    pub fn provider_name(&self) -> &'static str {
        match self {
            EngineKind::System => "the system voices",
            EngineKind::ElevenLabs => "ElevenLabs",
            EngineKind::OpenAi => "OpenAI",
            EngineKind::Deepgram => "Deepgram",
            EngineKind::Google => "Google Cloud Text-to-Speech",
            EngineKind::Polly => "Amazon Polly",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayState {
    Idle,
    Playing,
    Paused,
}

impl PlayState {
    pub fn is_active(&self) -> bool {
        matches!(self, PlayState::Playing | PlayState::Paused)
    }
}

/// One entry of a speech plan: what to say, and which document chunk it came
/// from so the UI can highlight the right text.
#[derive(Clone, Debug)]
pub struct PlanItem {
    pub chunk_index: usize,
    pub text: String,
}

/// A speech plan, and what the wordlists did to make it.
#[derive(Debug, Default)]
pub struct Plan {
    pub items: Vec<PlanItem>,
    pub hits: Vec<crate::wordlist::Hit>,
    /// Chunks dropped entirely by a `skip sentence` rule.
    pub skipped: usize,
}

/// Work out what will actually be spoken.
///
/// Here rather than in the interface because the command line has to reach the
/// same answer: a wordlist that keeps a word out of a reading must keep it out
/// of a file converted without the window ever opening, and two copies of this
/// loop would eventually disagree about that.
pub fn build_plan(
    document: &crate::document::Document,
    wordlists: &crate::wordlist::WordlistSet,
    filtering: bool,
) -> Plan {
    let mut plan = Plan::default();
    for (index, chunk) in document.chunks.iter().enumerate() {
        let text = if filtering {
            let applied = wordlists.apply(&chunk.display);
            plan.hits.extend(applied.hits);
            if applied.skipped {
                plan.skipped += 1;
                continue;
            }
            applied.text
        } else {
            chunk.display.clone()
        };

        if text.trim().is_empty() {
            continue;
        }
        plan.items.push(PlanItem {
            chunk_index: index,
            text,
        });
    }
    plan
}

/// Map a 0..=1 slider position onto a back end's own parameter range.
pub fn map_pos(pos: f32, min: f32, max: f32) -> f32 {
    min + (max - min) * pos.clamp(0.0, 1.0)
}

/// Inverse of [`map_pos`], for showing a back end default on the slider.
pub fn pos_of(value: f32, min: f32, max: f32) -> f32 {
    if (max - min).abs() < f32::EPSILON {
        0.5
    } else {
        ((value - min) / (max - min)).clamp(0.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two engines that existed before the others were added must still
    /// read back from a settings file written by an older build.
    #[test]
    fn the_original_engine_names_still_load() {
        for (written, expected) in [
            (r#""System""#, EngineKind::System),
            (r#""ElevenLabs""#, EngineKind::ElevenLabs),
        ] {
            let parsed: EngineKind = serde_json::from_str(written).expect("parses");
            assert_eq!(parsed, expected, "{written}");
        }
    }

    /// And every engine has to survive the round trip, or a setting would be
    /// silently forgotten between one launch and the next.
    #[test]
    fn every_engine_round_trips_through_the_settings_file() {
        for engine in EngineKind::ALL {
            let json = serde_json::to_string(&engine).expect("serialises");
            let back: EngineKind = serde_json::from_str(&json).expect("parses");
            assert_eq!(back, engine, "{json}");
        }
    }

    /// A settings file from a newer build must cost the user one setting, not
    /// the whole file.
    #[test]
    fn an_unknown_engine_falls_back_rather_than_failing() {
        let parsed: EngineKind = serde_json::from_str(r#""Klingon""#).expect("parses anyway");
        assert_eq!(parsed, EngineKind::System);
    }

    #[test]
    fn only_the_system_voices_stay_on_this_computer() {
        assert!(!EngineKind::System.is_cloud());
        for engine in EngineKind::ALL.iter().filter(|e| **e != EngineKind::System) {
            assert!(engine.is_cloud(), "{engine:?}");
        }
    }
}
