//! Speech synthesis back ends.
//!
//! Two engines with deliberately different shapes:
//!
//! * [`system`] wraps the OS synthesiser. It has to be driven from the main
//!   thread (AVSpeechSynthesizer and SAPI both insist), so the app pumps it
//!   once per frame.
//! * [`elevenlabs`] is network-bound, so it owns a worker thread and reports
//!   back over a channel.
//!
//! Both report progress as an index into the *speech plan* — the list of
//! chunks actually being spoken, which is not the same as the list of chunks
//! in the document once a wordlist has skipped some.

pub mod elevenlabs;
pub mod system;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EngineKind {
    System,
    ElevenLabs,
}

impl EngineKind {
    pub const ALL: [EngineKind; 2] = [EngineKind::System, EngineKind::ElevenLabs];

    pub fn label(&self) -> &'static str {
        match self {
            EngineKind::System => "System voices",
            EngineKind::ElevenLabs => "ElevenLabs",
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
