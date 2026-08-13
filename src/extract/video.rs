//! Turning a pile of described frames into something worth listening to.
//!
//! The frames themselves come from [`crate::ffmpeg`] and their descriptions
//! from [`crate::ollama`]; what is left — and what lives here — is the shape of
//! the finished text. Two shapes, in fact:
//!
//! * [`transcript`] is the frames as they were described, each labelled with
//!   when it appears. Nothing is added and nothing is joined up, so every
//!   sentence in it can be traced back to a still.
//! * That same transcript is then given to a text model to be rewritten as
//!   continuous narration — the thing a listener actually wants — and the
//!   transcript stays as what to fall back to when that second pass fails.

use std::time::Duration;

/// When a frame appears, worded the way the rest of the app words time.
///
/// Deliberately not `00:07`: this text is written to be read aloud, and a
/// synthesiser says "colon" or races the digits together. [`crate::audio::spoken_time`]
/// is the same function the player's countdown uses, so a video description and
/// the transport under it describe time identically.
pub fn moment(at: Duration) -> String {
    // Under a second in is the opening frame, and "0 seconds in" is a clumsy
    // way to say where a video starts.
    if at < Duration::from_secs(1) {
        return "At the start".to_string();
    }
    format!("{} in", crate::audio::spoken_time(at))
}

/// The described frames, in order, each under the time it was taken from.
///
/// Blank descriptions are dropped rather than labelled: a model that had
/// nothing to say about one frame should cost that frame's line, not a line
/// saying it was silent.
pub fn transcript(described: &[(Duration, String)]) -> String {
    described
        .iter()
        .filter(|(_, text)| !text.trim().is_empty())
        .map(|(at, text)| format!("{}: {}", moment(*at), text.trim()))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// What the narration pass is asked to work from: the instruction, then the
/// transcript under it.
pub fn narration_request(prompt: &str, transcript: &str) -> String {
    format!("{}\n\n{}", prompt.trim(), transcript)
}

/// Whether the narration a text model produced is worth using.
///
/// The second pass is a rewrite, so its answer should be in the same league as
/// what it was given. A model that returns a sentence and a half when handed
/// forty frames has not narrated them — it has ignored them, which small models
/// do when the transcript fills their context — and the transcript itself is a
/// better answer than a summary that lost most of the video.
///
/// The bar is deliberately low: prose really is shorter than a labelled list,
/// since it drops the timestamps and the repetition between frames.
pub fn narration_is_usable(narration: &str, transcript: &str) -> bool {
    let narration_length = narration.trim().chars().count();
    if narration_length < MINIMUM_NARRATION_CHARS {
        return false;
    }
    narration_length * 4 >= transcript.chars().count()
}

/// Below this, an answer is a refusal or a stub rather than a narration.
const MINIMUM_NARRATION_CHARS: usize = 80;

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    #[test]
    fn moments_are_worded_to_be_read_aloud() {
        assert_eq!(moment(Duration::ZERO), "At the start");
        // Anything inside the first second is still the start.
        assert_eq!(moment(Duration::from_millis(400)), "At the start");
        assert_eq!(moment(secs(1)), "1 second in");
        assert_eq!(moment(secs(12)), "12 seconds in");
        assert_eq!(moment(secs(60)), "1 minute in");
        assert_eq!(moment(secs(125)), "2 minutes 5 seconds in");
        // No colon-separated clock anywhere in it; that is the whole point.
        assert!(!moment(secs(125)).contains(':'));
    }

    #[test]
    fn the_transcript_labels_each_frame_with_when_it_appears() {
        let described = vec![
            (
                Duration::ZERO,
                "A harbour, boats moored along a quay.".to_string(),
            ),
            (
                secs(7),
                "A woman in a red coat walks along the quay.".to_string(),
            ),
        ];
        let text = transcript(&described);
        assert_eq!(
            text,
            "At the start: A harbour, boats moored along a quay.\n\n\
             7 seconds in: A woman in a red coat walks along the quay."
        );
    }

    #[test]
    fn frames_the_model_had_nothing_to_say_about_are_left_out() {
        let described = vec![
            (Duration::ZERO, "A harbour.".to_string()),
            (secs(7), "   ".to_string()),
            (secs(9), "A rope being tied off.".to_string()),
        ];
        let text = transcript(&described);
        assert_eq!(text.lines().filter(|l| !l.is_empty()).count(), 2);
        assert!(!text.contains("7 seconds in"));
    }

    /// The check that decides whether the user hears the prose or the list.
    #[test]
    fn a_narration_that_dropped_the_video_is_rejected() {
        let transcript = "At the start: ".to_string() + &"a long description. ".repeat(40);

        // A real rewrite: shorter than the list, but recognisably the same video.
        let good = "a long description. ".repeat(12);
        assert!(narration_is_usable(&good, &transcript));

        // A model that answered with a stub, which is the failure seen in
        // practice when a transcript overflows a small model's context.
        assert!(!narration_is_usable("A video.", &transcript));
        assert!(!narration_is_usable("", &transcript));
        // Long enough in absolute terms, but a fraction of what it was given.
        let stub = "The video shows a series of scenes in a coastal town somewhere.";
        assert!(!narration_is_usable(stub, &transcript));
    }

    #[test]
    fn a_short_video_can_have_a_short_narration() {
        // Three frames, so prose of a couple of sentences is a full answer and
        // must not be thrown away for being brief.
        let transcript = "At the start: A harbour with boats.\n\n\
                          7 seconds in: A woman walks along the quay.\n\n\
                          20 seconds in: A rope is tied to a bollard.";
        let narration = "A harbour comes into view, boats moored along a stone quay. A woman \
                         in a red coat walks along it, and the camera settles on a rope being \
                         tied off at a bollard.";
        assert!(narration_is_usable(narration, transcript));
    }

    #[test]
    fn the_narration_request_puts_the_instruction_first() {
        let request = narration_request("  Write this up.  ", "At the start: A harbour.");
        assert_eq!(request, "Write this up.\n\nAt the start: A harbour.");
    }
}
