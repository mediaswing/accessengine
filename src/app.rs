//! The window: state, layout, and the glue between the two.
//!
//! The UI is one column of full-width controls, read top to bottom: choose a
//! file, choose an engine, choose a voice, choose what to do, then Apply. That
//! order is also the Tab order and the order a screen reader walks, so the
//! keyboard, the pointer and the reader all describe the same app.
//!
//! Every control carries a caption tied to it with [`egui::Response::labelled_by`],
//! every button says what it does in words rather than only in an icon, and
//! everything reachable by mouse is reachable by key — see [`SHORTCUTS`].
//!
//! The UI thread never blocks. Anything slow becomes a [`Job`]; results arrive
//! as [`Update`]s that [`SpeechApp::drain_updates`] applies once per frame.

use crate::apikey::{self, KeySource};
use crate::audio::{self, AudioFormat, Playback};
use crate::config::{AUTO_LANGUAGE, Action, Config, EnginePreference, Formatting};
use crate::extract::{
    DOC_EXTENSIONS, FileKind, IMAGE_EXTENSIONS, PDF_EXTENSIONS, TABLE_EXTENSIONS, TEXT_EXTENSIONS,
    VIDEO_EXTENSIONS,
};
use crate::jobs::{self, Cancel, Job, Update};
use crate::theme::{self, CONTROL_HEIGHT, FORM_WIDTH, PROGRESS_HEIGHT};
use crate::tts::{self, Voice};
use crate::update;
use crate::{i18n, t, tn};
use egui::{Key, Modifiers, RichText};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

/// How the status line should read.
#[derive(PartialEq, Eq, Clone, Copy)]
enum Tone {
    Info,
    Success,
    Error,
}

impl Tone {
    /// A word in front of the message, because colour must never be the only
    /// thing carrying meaning — it is invisible to a screen reader and to
    /// roughly one in twelve men.
    fn prefix(self) -> String {
        match self {
            Self::Info => String::new(),
            Self::Success => t!("status.prefix.success"),
            Self::Error => t!("status.prefix.error"),
        }
    }

    /// The sound this outcome makes, if it makes one.
    ///
    /// `Info` deliberately makes none. It is the tone used for "reading…" and
    /// the other running commentary, and a document read aloud in three parts
    /// would otherwise chime through its own narration.
    fn sound(self) -> Option<&'static [u8]> {
        match self {
            Self::Info => None,
            Self::Success => Some(SUCCESS_SOUND),
            Self::Error => Some(ERROR_SOUND),
        }
    }
}

/// The three cues, built into the binary so the app has no files to lose.
///
/// All are CC0 recordings from freesound.org, trimmed of their silence and
/// levelled to the same loudness so that none is the startling one — see
/// `assets/sounds/CREDITS.txt`.
const SUCCESS_SOUND: &[u8] = include_bytes!("../assets/sounds/success.wav");
const ERROR_SOUND: &[u8] = include_bytes!("../assets/sounds/error.wav");
/// Sounded once as a job starts, so that pressing Apply is answered by
/// something other than a spinner appearing somewhere you may not be looking.
/// It is the opening bracket to the success or failure cue that closes the job.
const PROGRESS_SOUND: &[u8] = include_bytes!("../assets/sounds/progress.wav");

/// Sounded every [`TICK_EVERY`] while a job runs: "still working".
///
/// The one place the app makes a sound that reports nothing new, which is why
/// it is cut from the same recording as [`PROGRESS_SOUND`] but a fifth of the
/// length and around 11 dB quieter. A job here can run for forty minutes — a
/// video is a vision-model call per frame — and the alternative to this is a
/// user who cannot see the progress bar having no way to tell a long job from a
/// hung one. It has its own setting because it is also the cue most likely to
/// wear thin.
const TICK_SOUND: &[u8] = include_bytes!("../assets/sounds/tick.wav");

/// How long a sound holds the floor against the next one. See
/// [`SpeechApp::sound`].
const CUE_GAP: Duration = Duration::from_millis(250);

/// Whether a sound, having played, makes the next one wait for it.
#[derive(PartialEq, Eq, Clone, Copy)]
enum ClaimsTheGap {
    Yes,
    /// The tick, which yields to everything.
    No,
}

/// How often the tick sounds while a job runs.
///
/// Long enough not to nag, short enough that the silence between two of them is
/// never long enough to worry about. Fifteen seconds also happens to be about
/// one frame of video on a machine with no graphics card, so a video job ticks
/// roughly once per frame described.
const TICK_EVERY: Duration = Duration::from_secs(15);

/// Where an ElevenLabs key is created. Linked from the dialog that asks for
/// one, because "paste the key from your account" assumes you know where in
/// that account it lives.
const ELEVENLABS_KEYS_URL: &str = "https://elevenlabs.io/app/settings/api-keys";

/// Which engine the current settings actually resolve to.
#[derive(PartialEq, Eq, Clone, Copy)]
enum ActiveEngine {
    ElevenLabs,
    System,
    /// ElevenLabs is selected but there is no key.
    MissingKey,
    /// System voices were asked for on a platform that has none implemented.
    Unsupported,
}

/// A job in flight.
struct Busy {
    label: String,
    progress: Option<f32>,
    cancel: Cancel,
    cancellable: bool,
}

/// Audio already synthesised, so a listen followed by a save costs one API call.
struct CachedRender {
    fingerprint: u64,
    mp3: Arc<Vec<u8>>,
}

/// A question the app needs answered before it can carry on reading an image.
enum Prompt {
    InstallOllama,
    InstallFfmpeg,
    PullModel(String),
}

/// The panes listed down the left of the window. The form is one of them, so
/// everything the app can do is a place you can go to rather than a dialog that
/// appears over what you were doing — which also means every one of them has a
/// stable position in the Tab order.
#[derive(PartialEq, Eq, Clone, Copy)]
enum Pane {
    Read,
    Player,
    Dictionary,
    Settings,
    Shortcuts,
}

impl Pane {
    const ALL: [Self; 5] = [
        Self::Read,
        Self::Player,
        Self::Dictionary,
        Self::Settings,
        Self::Shortcuts,
    ];

    fn label(self) -> String {
        match self {
            Self::Read => t!("pane.read.label"),
            Self::Player => t!("pane.player.label"),
            Self::Dictionary => t!("pane.dictionary.label"),
            Self::Settings => t!("pane.settings.label"),
            Self::Shortcuts => t!("pane.shortcuts.label"),
        }
    }

    fn hint(self) -> String {
        match self {
            Self::Read => t!("pane.read.hint"),
            Self::Player => t!("pane.player.hint"),
            Self::Dictionary => t!("pane.dictionary.hint"),
            Self::Settings => t!("pane.settings.hint"),
            Self::Shortcuts => t!("pane.shortcuts.hint"),
        }
    }

    /// The shortcut that opens it. `{C}` becomes this platform's modifier.
    fn shortcut(self) -> String {
        let number = match self {
            Self::Read => 1,
            Self::Player => 2,
            Self::Dictionary => 3,
            Self::Settings => 4,
            Self::Shortcuts => 5,
        };
        format!("{}{number}", crate::i18n::MODIFIER)
    }
}

/// A control the keyboard can be sent to directly.
#[derive(PartialEq, Eq, Clone, Copy)]
enum Field {
    File,
    Formatting,
    Engine,
    Voice,
    Action,
    Apply,
    AudioFile,
    Play,
}

/// Voice list loading state, kept per engine.
#[derive(Default)]
struct VoiceList {
    voices: Vec<Voice>,
    error: Option<String>,
    loaded: bool,
}

/// Every keyboard shortcut, in the order the help pane lists them.
///
/// Names rather than sentences: each one is two entries in the language file,
/// `shortcuts.<name>.keys` and `shortcuts.<name>.what`. The keys are worth
/// translating as well as the descriptions — "Shift" and "Space" are not what
/// every keyboard in the world has printed on it.
pub const SHORTCUTS: &[&str] = &[
    "open", "apply", "stop", "panes", "play", "back", "rail", "tab", "operate", "adjust", "key",
    "log", "help",
];

/// One half of a shortcut row: `part` is `keys` or `what`.
fn shortcut_text(name: &str, part: &str) -> String {
    crate::i18n::text(&format!("shortcuts.{name}.{part}"), &[])
}

/// A rough seconds-per-frame for a vision model with no GPU behind it, used
/// only to turn a frame count into the wait it implies.
///
/// Deliberately pessimistic. The number is there so that dragging the cap to
/// its maximum says "around 50 minutes" rather than nothing at all, and a
/// guess that runs long is a guess that disappoints nobody.
const SECONDS_PER_FRAME_ESTIMATE: u64 = 15;

/// How a scene threshold reads in words, since the number itself means nothing
/// outside ffmpeg.
fn describe_sensitivity(threshold: f32) -> String {
    match threshold {
        v if v < 0.2 => t!("sensitivity.very_high"),
        v if v < 0.35 => t!("sensitivity.high"),
        v if v < 0.55 => t!("sensitivity.balanced"),
        v if v < 0.8 => t!("sensitivity.low"),
        _ => t!("sensitivity.very_low"),
    }
}

/// Whether a sound arriving `since` after the last one can be heard as itself
/// rather than as a collision. `None` means nothing has sounded yet.
fn cue_is_clear(since: Option<Duration>) -> bool {
    since.is_none_or(|elapsed| elapsed >= CUE_GAP)
}

/// Who holds the floor once a sound has played.
///
/// Pulled out of [`SpeechApp::sound`] so the rule can be checked without an app
/// and an output device, because the rule is not obvious and the cost of it
/// silently inverting is a success chime that goes missing once in a while —
/// the hardest kind of bug to be told about.
fn floor_after(
    previous: Option<Instant>,
    played: Instant,
    claims: ClaimsTheGap,
) -> Option<Instant> {
    match claims {
        ClaimsTheGap::Yes => Some(played),
        ClaimsTheGap::No => previous,
    }
}

/// Writes the percentage across the middle of a progress bar.
///
/// Painted here rather than left to `ProgressBar::show_percentage`, which puts
/// it against the left edge in the selection colour — white on both themes, so
/// on the white track of the light theme the first tenth of every job is a
/// percentage nobody can read. Centred, it is also where the eye already is.
///
/// The outline underneath is what makes one colour work over both the empty
/// track and the filled part; see [`crate::theme::PROGRESS_TEXT`].
fn percentage_across(ui: &egui::Ui, bar: egui::Rect, progress: f32) {
    let text = format!("{}%", (progress * 100.0).round() as u32);
    let font = egui::TextStyle::Button.resolve(ui.style());
    let painter = ui.painter().with_clip_rect(bar);
    for offset in [
        egui::vec2(-1.0, -1.0),
        egui::vec2(1.0, -1.0),
        egui::vec2(-1.0, 1.0),
        egui::vec2(1.0, 1.0),
    ] {
        painter.text(
            bar.center() + offset,
            egui::Align2::CENTER_CENTER,
            &text,
            font.clone(),
            theme::PROGRESS_TEXT_OUTLINE,
        );
    }
    painter.text(
        bar.center(),
        egui::Align2::CENTER_CENTER,
        &text,
        font,
        theme::PROGRESS_TEXT,
    );
}

pub struct SpeechApp {
    config: Config,
    api_key: Option<String>,
    key_source: KeySource,

    file: Option<PathBuf>,
    file_kind: Option<FileKind>,
    /// The extracted text. Held rather than shown: the app reads documents, it
    /// is not an editor, and a large read-only text box is one more thing
    /// between the user and the Apply button.
    text: String,
    text_note: String,

    system_voices: VoiceList,
    elevenlabs_voices: VoiceList,

    busy: Option<Busy>,
    /// Whatever is making sound right now. One field rather than one per pane,
    /// so two things can never talk over each other and a single Stop always
    /// stops the right one.
    playback: Option<Playback>,
    /// True when `playback` is a file opened in the audio player rather than a
    /// document being read aloud. The two want different words in the status
    /// line and different buttons enabled.
    playing_audio_file: bool,
    cached: Option<CachedRender>,

    /// The file loaded into the audio player, which is deliberately separate
    /// from the document in the Read pane: auditioning a saved recording
    /// should not throw away the document you just extracted.
    audio_file: Option<PathBuf>,

    status: Option<(String, Tone)>,
    /// The success or failure sound currently playing, held only because
    /// dropping it would cut the sound off mid-note.
    cue: Option<Playback>,
    /// When the last one started, so a cue cannot be retriggered faster than
    /// it can be heard. Every caller of [`SpeechApp::set_status`] today is an
    /// event handler, but a status set from drawing code instead would turn a
    /// helpful sound into sixty a second, and that failure is loud, obvious to
    /// the user and invisible in a test.
    cue_at: Option<Instant>,
    /// When the running job last ticked, or `None` when nothing is running.
    /// See [`TICK_SOUND`].
    tick_at: Option<Instant>,
    log: Vec<String>,
    show_log: bool,

    /// Whether the "reset settings" confirmation is open. Kept apart from
    /// [`Prompt`], whose answers all end in a job being started.
    confirm_reset: bool,
    prompt: Option<Prompt>,
    /// Re-run after the user resolves a [`Prompt`].
    deferred: Option<Job>,

    pane: Pane,
    /// The API key is still a dialog rather than a pane: it is answered on the
    /// spot, in the middle of choosing an engine, and then dismissed.
    show_api_key: bool,
    /// True on the frame the dialog opens, so it can claim the keyboard once
    /// rather than every frame — which would trap focus in its only field.
    dialog_opened: bool,
    key_input: String,
    /// A key was just entered and is being tried against ElevenLabs, so the
    /// answer to that attempt is worth a word and a sound — where the same
    /// voice list loading by itself, at launch or on switching engines, is not.
    checking_key: bool,
    /// The last key was refused and thrown away, so the dialog can say why it
    /// is asking again.
    key_rejected: bool,
    /// A control to hand the keyboard to on the next frame.
    focus: Option<Field>,
    /// A pane whose tab should take the keyboard on the next frame, after the
    /// arrow keys moved along the rail.
    focus_tab: Option<Pane>,
    /// Set by any control that changes `config`; written out at end of frame
    /// so a slider drag doesn't rewrite the file on every pixel.
    config_dirty: bool,
    /// Whether the Windows right-click "Speak to file" entry is registered.
    /// Read from the registry once at startup and after each toggle, rather
    /// than every frame — see [`crate::context_menu`].
    context_menu_installed: bool,

    tx: Sender<Update>,
    rx: Receiver<Update>,

    /// A release newer than this one, once the background check at startup
    /// finds one. Outside the `Job`/`busy` system entirely: it is a quiet,
    /// one-shot look on launch, not something that should occupy the busy
    /// slot and block the Apply button while it runs.
    update_available: Option<update::Available>,
    update_rx: Receiver<Option<update::Available>>,
}

impl SpeechApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        crate::theme::apply(&cc.egui_ctx);

        let (tx, rx) = channel();
        let (update_tx, update_rx) = channel();
        let (api_key, key_source) = apikey::load();
        let config = Config::load();

        let mut app = Self {
            config,
            api_key,
            key_source,
            file: None,
            file_kind: None,
            text: String::new(),
            text_note: String::new(),
            system_voices: VoiceList::default(),
            elevenlabs_voices: VoiceList::default(),
            busy: None,
            playback: None,
            playing_audio_file: false,
            cached: None,
            audio_file: None,
            status: None,
            cue: None,
            cue_at: None,
            tick_at: None,
            log: Vec::new(),
            show_log: false,
            confirm_reset: false,
            prompt: None,
            deferred: None,
            pane: Pane::Read,
            show_api_key: false,
            dialog_opened: false,
            key_input: String::new(),
            checking_key: false,
            key_rejected: false,
            // The keyboard starts on the first control, so a user who never
            // touches the mouse does not have to Tab in from nowhere.
            focus: Some(Field::File),
            focus_tab: None,
            config_dirty: false,
            context_menu_installed: crate::context_menu::is_installed(),
            tx,
            rx,
            update_available: None,
            update_rx,
        };
        app.load_system_voices();

        // The language was chosen before this window existed — by the setting,
        // or by asking the operating system. The prompts have to follow it here
        // as well as when it is changed in Settings, or somebody whose computer
        // is set to French gets a French interface asking the vision model in
        // English and reading the answer back in an English voice.
        //
        // Silent, unlike the same move made from Settings: nothing has just
        // happened from the user's point of view, and an app that opens with a
        // status line already talking about prompts is answering a question
        // nobody asked.
        app.retranslate_prompts();

        // A path on the command line opens straight away, which is what
        // "Open With" produces on both platforms.
        if let Some(path) = std::env::args_os().nth(1).map(PathBuf::from) {
            let ctx = cc.egui_ctx.clone();
            app.open_file(&ctx, path);
        }

        // A quiet, one-time look at startup — not on a timer, and not
        // re-checked for the rest of the session. Errors (offline, GitHub
        // unreachable) are dropped rather than shown: this is a nicety, not
        // something worth interrupting anyone over.
        {
            let ctx = cc.egui_ctx.clone();
            std::thread::spawn(move || {
                let available = update::check().ok().flatten();
                let _ = update_tx.send(available);
                ctx.request_repaint();
            });
        }
        app
    }

    // ---------------------------------------------------------------- state

    /// Resolves the engine choice against what is actually available.
    fn active_engine(&self) -> ActiveEngine {
        match self.config.engine {
            EnginePreference::ElevenLabs if self.api_key.is_some() => ActiveEngine::ElevenLabs,
            EnginePreference::ElevenLabs => ActiveEngine::MissingKey,
            EnginePreference::System if tts::system::SUPPORTED => ActiveEngine::System,
            EnginePreference::System => ActiveEngine::Unsupported,
        }
    }

    /// Builds the engine description a job needs, or `None` if we can't speak.
    fn job_engine(&self) -> Option<jobs::Engine> {
        match self.active_engine() {
            ActiveEngine::ElevenLabs => Some(jobs::Engine::ElevenLabs {
                api_key: self.api_key.clone()?,
                voice_id: self.config.elevenlabs_voice_id.clone(),
                model_id: self.config.elevenlabs_model_id.clone(),
            }),
            ActiveEngine::System => Some(jobs::Engine::System {
                voice: self.config.system_voice.clone(),
                rate: self.config.system_rate,
            }),
            _ => None,
        }
    }

    /// The text as it will actually be spoken, with the dictionary applied, and
    /// how many words it changed.
    ///
    /// Computed on demand rather than when the file is read, so editing the
    /// dictionary changes what the next Apply says without reopening anything —
    /// and so a long document isn't rescanned on every frame.
    fn spoken_text(&self) -> (String, usize) {
        crate::dictionary::apply(&self.text, &self.config.dictionary)
    }

    /// Identifies a render so cached audio is only reused for the same text,
    /// voice and model.
    fn fingerprint(&self) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.text.hash(&mut hasher);
        // Changing a replacement changes the audio, so it has to change this.
        for rule in &self.config.dictionary {
            rule.from.hash(&mut hasher);
            rule.to.hash(&mut hasher);
            rule.whole_word.hash(&mut hasher);
        }
        match self.active_engine() {
            ActiveEngine::ElevenLabs => {
                "elevenlabs".hash(&mut hasher);
                self.config.elevenlabs_voice_id.hash(&mut hasher);
                self.config.elevenlabs_model_id.hash(&mut hasher);
            }
            _ => {
                "system".hash(&mut hasher);
                self.config.system_voice.hash(&mut hasher);
                self.config.system_rate.hash(&mut hasher);
            }
        }
        hasher.finish()
    }

    fn cached_mp3(&self) -> Option<Arc<Vec<u8>>> {
        let cached = self.cached.as_ref()?;
        (cached.fingerprint == self.fingerprint()).then(|| Arc::clone(&cached.mp3))
    }

    fn is_playing(&self) -> bool {
        self.playback.is_some()
    }

    fn set_status(&mut self, message: impl Into<String>, tone: Tone) {
        self.status = Some((message.into(), tone));
        self.play_cue(tone);
    }

    /// The same, without the sound.
    ///
    /// For the audio player, and only for it. Everywhere else the cue is the
    /// point: it says an action you started and stopped watching has finished.
    /// In the player the sound *is* the output — a chime as a recording is
    /// loaded, or a chime landing on the last words of one, is the app talking
    /// over the thing it was asked to play.
    ///
    /// Failures there still make their noise. "That file would not open" is
    /// worth interrupting for; "that file opened" is not.
    fn set_status_quietly(&mut self, message: impl Into<String>, tone: Tone) {
        self.status = Some((message.into(), tone));
    }

    /// Sounds the cue for an outcome, if there is one for it and the user
    /// wants it.
    ///
    /// Hung off `set_status` rather than off each of the three dozen places an
    /// action can finish, because that is the one thing every one of them
    /// already does. Anything that reports an outcome gets the sound for free,
    /// and — the part worth having — anything added later cannot forget it.
    fn play_cue(&mut self, tone: Tone) {
        let Some(sound) = tone.sound() else {
            return;
        };
        self.play_sound(sound);
    }

    /// Sounds one of the built-in cues, subject to the setting and the gap.
    fn play_sound(&mut self, sound: &'static [u8]) {
        self.sound(sound, ClaimsTheGap::Yes);
    }

    /// Two cues inside [`CUE_GAP`] would talk over each other whichever two
    /// they are, so the second is dropped. `claims` is which of the two a sound
    /// is: an announcement waits for the one before it and makes the next one
    /// wait, while the tick only ever waits.
    ///
    /// The asymmetry is the whole point. Ticking every fifteen seconds means a
    /// tick lands within a quarter-second of the success chime roughly one job
    /// in sixty — and if the tick claimed the gap, that job would be the one
    /// where the sound saying "finished" went missing, swallowed by a sound
    /// that says nothing at all.
    fn sound(&mut self, sound: &'static [u8], claims: ClaimsTheGap) {
        if !self.config.sound_effects {
            return;
        }
        if !cue_is_clear(self.cue_at.map(|at| at.elapsed())) {
            return;
        }
        self.cue_at = floor_after(self.cue_at, Instant::now(), claims);
        // A cue is decoration. Failing to open an output device for one is not
        // worth a word to the user, who is at this moment being told something
        // that actually matters — but it goes in the log, because "the sounds
        // stopped working" is otherwise unanswerable.
        match Playback::play_cue(sound) {
            Ok(playback) => self.cue = Some(playback),
            Err(error) => crate::log::line(format!("sound: the cue could not play — {error:#}")),
        }
    }

    /// Sounds the "still working" tick when one is due.
    ///
    /// Called every frame from [`SpeechApp::update`], which already repaints on
    /// a timer while a job runs, so no separate clock is needed — the check is
    /// two comparisons and costs nothing on the frames it does not fire.
    fn tick_while_busy(&mut self) {
        if self.busy.is_none() || !self.config.progress_tick {
            return;
        }
        // Never over speech. A tick is "the app has not forgotten you", which
        // is worth nothing at all while the app is talking — and a document
        // being read aloud is exactly when an interruption is least welcome.
        if self.is_playing() {
            return;
        }
        let due = self.tick_at.is_none_or(|last| last.elapsed() >= TICK_EVERY);
        if !due {
            return;
        }
        // The clock advances even if the sound below is dropped for landing on
        // top of a cue: a tick skipped is a tick skipped, not one owed.
        self.tick_at = Some(Instant::now());
        self.sound(TICK_SOUND, ClaimsTheGap::No);
    }

    fn save_config(&mut self) {
        if let Err(error) = self.config.save() {
            self.set_status(
                t!("status.config_failed", error = format!("{error:#}")),
                Tone::Error,
            );
        }
    }

    // ----------------------------------------------------------------- jobs

    fn start(&mut self, ctx: &egui::Context, job: Job) {
        if self.busy.is_some() {
            return;
        }
        let cancel: Cancel = Arc::new(AtomicBool::new(false));
        self.busy = Some(Busy {
            label: job.status_label(),
            progress: None,
            cancel: Arc::clone(&cancel),
            cancellable: job.is_cancellable(),
        });
        self.status = None;
        self.play_sound(PROGRESS_SOUND);
        // Counted from the start tone, so the first tick lands a full interval
        // later rather than immediately on top of it.
        self.tick_at = Some(Instant::now());

        let ctx = ctx.clone();
        jobs::spawn(job, self.tx.clone(), cancel, move || ctx.request_repaint());
    }

    fn load_system_voices(&mut self) {
        // Reading the local voice list takes a few milliseconds on macOS and
        // rather longer on Windows, but both are far short of a visible pause,
        // so it happens inline rather than as a job.
        let (voices, error) = match tts::system::list_voices() {
            // An empty `system_voice` is left alone on purpose: it means "use
            // whatever voice this computer is set to", which is the right
            // default and the one the user's screen reader already speaks.
            Ok(voices) => (voices, None),
            Err(error) => (Vec::new(), Some(format!("{error:#}"))),
        };
        self.system_voices = VoiceList {
            voices,
            error,
            loaded: true,
        };
    }

    fn load_elevenlabs_voices(&mut self, ctx: &egui::Context) {
        let Some(key) = self.api_key.clone() else {
            return;
        };
        self.elevenlabs_voices.error = None;
        self.start(ctx, Job::LoadElevenLabsVoices(key));
    }

    /// ⌘O, which opens whichever kind of file the pane on screen is about.
    fn choose_file_for_pane(&mut self, ctx: &egui::Context) {
        if self.pane == Pane::Player {
            self.choose_audio_file();
        } else {
            self.choose_file(ctx);
        }
    }

    fn choose_file(&mut self, ctx: &egui::Context) {
        if self.busy.is_some() {
            return;
        }
        if let Some(path) = rfd::FileDialog::new()
            .set_title(t!("chooser.document.title"))
            .add_filter(
                t!("chooser.filter.all"),
                &[
                    TEXT_EXTENSIONS,
                    DOC_EXTENSIONS,
                    PDF_EXTENSIONS,
                    TABLE_EXTENSIONS,
                    IMAGE_EXTENSIONS,
                    VIDEO_EXTENSIONS,
                ]
                .concat(),
            )
            .add_filter(t!("chooser.filter.text"), TEXT_EXTENSIONS)
            .add_filter(t!("chooser.filter.docx"), DOC_EXTENSIONS)
            .add_filter(t!("chooser.filter.pdf"), PDF_EXTENSIONS)
            .add_filter(t!("chooser.filter.table"), TABLE_EXTENSIONS)
            .add_filter(t!("chooser.filter.image"), IMAGE_EXTENSIONS)
            .add_filter(t!("chooser.filter.video"), VIDEO_EXTENSIONS)
            .pick_file()
        {
            self.open_file(ctx, path);
        }
    }

    fn open_file(&mut self, ctx: &egui::Context, path: PathBuf) {
        let Some(kind) = FileKind::from_path(&path) else {
            self.set_status(
                t!(
                    "status.unreadable_type",
                    name = path.file_name().unwrap_or_default().to_string_lossy()
                ),
                Tone::Error,
            );
            return;
        };

        self.stop_playback();
        self.file = Some(path.clone());
        self.file_kind = Some(kind);
        self.text.clear();
        self.text_note.clear();
        self.cached = None;
        self.log.clear();

        let job = match kind {
            FileKind::Image => Job::ReadImage {
                path,
                config: Box::new(self.config.clone()),
            },
            FileKind::Video => Job::ReadVideo {
                path,
                config: Box::new(self.config.clone()),
            },
            _ => Job::ReadDocument {
                path,
                formatting: self.config.formatting,
            },
        };
        self.start(ctx, job);
    }

    /// The Apply button: run whichever action the dropdown is showing.
    fn apply(&mut self, ctx: &egui::Context) {
        if self.busy.is_some() {
            return;
        }
        if self.file.is_none() {
            self.set_status(t!("status.no_file"), Tone::Error);
            self.focus = Some(Field::File);
            return;
        }
        if self.text.trim().is_empty() {
            self.set_status(t!("status.no_text"), Tone::Error);
            return;
        }
        match self.active_engine() {
            ActiveEngine::MissingKey => {
                self.set_status(t!("status.no_key"), Tone::Error);
                self.open_api_key_dialog();
                return;
            }
            ActiveEngine::Unsupported => {
                self.set_status(tts::system::unsupported_message(), Tone::Error);
                return;
            }
            _ => {}
        }

        match self.config.action {
            Action::ReadAloud => self.read_aloud(ctx),
            Action::SaveAudio => self.save_audio(ctx),
        }
    }

    fn read_aloud(&mut self, ctx: &egui::Context) {
        let Some(engine) = self.job_engine() else {
            return;
        };
        let (text, replaced) = self.spoken_text();
        let reading = match replaced {
            0 => t!("status.reading_plain"),
            n => tn!("status.reading", n),
        };

        match engine {
            // The system engines speak straight to the output device, so there
            // is nothing to synthesise and nothing to wait for.
            jobs::Engine::System { voice, rate } => match tts::system::speak(&text, &voice, rate) {
                Ok(child) => {
                    self.playback = Some(Playback::Process(child));
                    self.playing_audio_file = false;
                    self.set_status(reading, Tone::Info);
                }
                Err(error) => self.set_status(format!("{error:#}"), Tone::Error),
            },
            jobs::Engine::ElevenLabs { .. } => {
                if self.config.elevenlabs_voice_id.is_empty() {
                    self.set_status(t!("status.no_voice"), Tone::Error);
                    self.focus = Some(Field::Voice);
                    return;
                }
                // Replay without paying for the same audio twice.
                if let Some(mp3) = self.cached_mp3() {
                    self.begin_playback(mp3);
                    return;
                }
                self.start(ctx, Job::Synthesize { engine, text });
            }
        }
    }

    fn begin_playback(&mut self, mp3: Arc<Vec<u8>>) {
        // rodio's output stream is not `Send` on macOS, so playback is always
        // created here on the UI thread rather than inside a job.
        match Playback::play_mp3(mp3.as_ref().clone()) {
            Ok(playback) => {
                self.playback = Some(playback);
                self.playing_audio_file = false;
                self.set_status(t!("status.reading_plain"), Tone::Info);
            }
            Err(error) => self.set_status(format!("{error:#}"), Tone::Error),
        }
    }

    fn stop_playback(&mut self) {
        if let Some(mut playback) = self.playback.take() {
            playback.stop();
        }
        self.playing_audio_file = false;
    }

    // -------------------------------------------------------- the audio player

    /// True when the sound currently playing is the player's file, which is
    /// what all four transport buttons key off.
    fn player_is_active(&self) -> bool {
        self.playing_audio_file && self.playback.is_some()
    }

    fn player_is_paused(&self) -> bool {
        self.player_is_active() && self.playback.as_ref().is_some_and(Playback::is_paused)
    }

    fn choose_audio_file(&mut self) {
        let mut dialog = rfd::FileDialog::new()
            .set_title(t!("chooser.audio.title"))
            .add_filter(t!("chooser.filter.audio"), audio::PLAYABLE_EXTENSIONS);
        if let Some(dir) = self
            .config
            .last_audio_dir
            .as_ref()
            .filter(|dir| dir.is_dir())
            .or(self.config.last_save_dir.as_ref())
            .filter(|dir| dir.is_dir())
        {
            dialog = dialog.set_directory(dir);
        }
        if let Some(path) = dialog.pick_file() {
            self.load_audio_file(path);
        }
    }

    /// Loads a file into the player, ready to play but not yet playing —
    /// shared by the chooser and by a file dropped on the window.
    fn load_audio_file(&mut self, path: PathBuf) {
        // A new file replaces whatever was playing, rather than joining it.
        self.stop_playback();
        self.config.last_audio_dir = path.parent().map(Path::to_path_buf);
        self.config_dirty = true;
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        self.set_status_quietly(t!("status.loaded", name = name), Tone::Success);
        self.audio_file = Some(path);
        self.focus = Some(Field::Play);
    }

    /// Play, or resume from where Pause left off.
    fn player_play(&mut self) {
        if self.player_is_paused() {
            if let Some(playback) = &mut self.playback {
                playback.resume();
            }
            self.set_status(t!("status.playing"), Tone::Info);
            return;
        }
        if self.player_is_active() {
            return;
        }
        let Some(path) = self.audio_file.clone() else {
            self.set_status(t!("status.no_audio_file"), Tone::Error);
            self.focus = Some(Field::AudioFile);
            return;
        };

        // Reading a document aloud and playing a file are both "sound out of
        // this app", so starting one ends the other.
        self.stop_playback();
        match Playback::play_file(&path) {
            Ok(playback) => {
                self.playback = Some(playback);
                self.playing_audio_file = true;
                self.set_status(
                    t!(
                        "status.playing_file",
                        name = path.file_name().unwrap_or_default().to_string_lossy()
                    ),
                    Tone::Info,
                );
            }
            Err(error) => self.set_status(format!("{error:#}"), Tone::Error),
        }
    }

    fn player_pause(&mut self) {
        if !self.player_is_active() || self.player_is_paused() {
            return;
        }
        if let Some(playback) = &mut self.playback {
            playback.pause();
            let at = audio::spoken_time(playback.position());
            self.set_status(t!("status.paused_at", at = at), Tone::Info);
        }
    }

    fn player_stop(&mut self) {
        if !self.player_is_active() {
            return;
        }
        self.stop_playback();
        self.set_status(t!("status.stopped"), Tone::Info);
    }

    /// Back ten seconds, without disturbing whether it was playing or paused.
    fn player_skip_back(&mut self) {
        if !self.player_is_active() {
            return;
        }
        let Some(playback) = &mut self.playback else {
            return;
        };
        match playback.skip_back() {
            Ok(()) => {
                let at = audio::spoken_time(playback.position());
                let message = if playback.is_paused() {
                    t!("status.resumed_paused", at = at)
                } else {
                    t!("status.resumed_playing", at = at)
                };
                self.set_status(message, Tone::Info);
            }
            Err(error) => self.set_status(format!("{error:#}"), Tone::Error),
        }
    }

    /// ⌘Space: one key for the thing the player is most often asked to do.
    fn player_toggle(&mut self) {
        if self.player_is_active() && !self.player_is_paused() {
            self.player_pause();
        } else {
            self.player_play();
        }
    }

    /// Escape and ⌘. mean "whatever is happening, stop".
    fn stop_everything(&mut self) {
        if self.is_playing() {
            let what = if self.playing_audio_file {
                t!("status.stopped")
            } else {
                t!("status.stopped_reading")
            };
            self.stop_playback();
            self.set_status(what, Tone::Info);
            return;
        }
        if let Some(busy) = &self.busy
            && busy.cancellable
        {
            busy.cancel.store(true, Ordering::Relaxed);
            self.set_status(t!("status.cancelling"), Tone::Info);
        }
    }

    fn save_audio(&mut self, ctx: &egui::Context) {
        let Some(engine) = self.job_engine() else {
            return;
        };
        let format = self.config.save_format;

        let suggested = format!(
            "{}.{}",
            self.file
                .as_ref()
                .and_then(|p| p.file_stem())
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| t!("chooser.save.fallback_name")),
            format.extension()
        );

        let mut dialog = rfd::FileDialog::new()
            .set_title(t!("chooser.save.title"))
            .add_filter(format.label(), &[format.extension()])
            .set_file_name(&suggested);
        if let Some(dir) = self.config.last_save_dir.as_ref().filter(|d| d.is_dir()) {
            dialog = dialog.set_directory(dir);
        }
        let Some(mut path) = dialog.save_file() else {
            self.set_status(t!("status.nothing_saved"), Tone::Info);
            return;
        };

        // The dropdown chose the format, so the extension follows it unless the
        // user deliberately typed the other one.
        if path.extension().is_none() {
            path.set_extension(format.extension());
        }
        let format = AudioFormat::from_path(&path);
        self.config.save_format = format;
        self.config.last_save_dir = path.parent().map(Path::to_path_buf);
        self.config_dirty = true;

        let cached_mp3 = self.cached_mp3();
        let (text, _) = self.spoken_text();
        self.start(
            ctx,
            Job::Save {
                engine,
                text,
                path,
                format,
                cached_mp3,
            },
        );
    }

    fn open_api_key_dialog(&mut self) {
        self.key_input.clear();
        self.show_api_key = true;
        self.dialog_opened = true;
    }

    /// Throws away a key ElevenLabs has refused, and asks for another one.
    ///
    /// A refused key is worth nothing: every request made with it fails the
    /// same way, and until now the only route back to the dialog was to switch
    /// the engine to System voices and back again to make the app notice. So
    /// the key goes, the dialog opens where it left off, and the sound says
    /// which way it went.
    ///
    /// Only an actual refusal gets here — see `jobs::is_key_rejection`. A key
    /// that could not be checked because the network was down is left alone;
    /// deleting someone's key because their wifi dropped would be its own bug.
    fn forget_rejected_key(&mut self) {
        self.checking_key = false;
        self.elevenlabs_voices = VoiceList::default();

        // A key from the environment is not this app's to delete, and clearing
        // the file would not stop it being loaded again at the next launch.
        if self.key_source == KeySource::Env {
            self.set_status(
                t!("status.key_rejected_env", variable = apikey::ENV_VAR),
                Tone::Error,
            );
            return;
        }

        if let Err(error) = apikey::clear() {
            // Worth logging, not worth saying: the key is out of use either
            // way, since it is gone from memory and never read again this
            // session, and the user is already being told the bigger thing.
            crate::log::line(format!(
                "api key: the rejected key could not be removed — {error:#}"
            ));
        }
        self.api_key = None;
        self.key_source = KeySource::None;
        self.key_rejected = true;
        self.set_status(t!("status.key_rejected"), Tone::Error);
        self.open_api_key_dialog();
    }

    // -------------------------------------------------------------- updates

    fn drain_updates(&mut self, ctx: &egui::Context) {
        if let Ok(available) = self.update_rx.try_recv() {
            self.update_available = available;
        }

        while let Ok(update) = self.rx.try_recv() {
            match update {
                Update::Status(message) => {
                    if let Some(busy) = &mut self.busy {
                        busy.label = message;
                    }
                }
                Update::Progress(fraction) => {
                    if let Some(busy) = &mut self.busy {
                        busy.progress = Some(fraction.clamp(0.0, 1.0));
                    }
                }
                Update::Log(line) => {
                    crate::log::line(&line);
                    self.show_log = true;
                    self.log.push(line);
                    // Keep the pane bounded; a model pull emits a lot of lines.
                    if self.log.len() > 500 {
                        self.log.drain(..self.log.len() - 500);
                    }
                }
                Update::TextReady { text, note } => {
                    // The note, not the text: how much was extracted is the
                    // useful part, and the document itself is the user's.
                    crate::log::line(format!("extracted {note}"));
                    self.text = text;
                    self.text_note = note;
                    self.cached = None;
                    self.set_status(t!("status.ready"), Tone::Success);
                }
                Update::ElevenLabsVoices(voices) => {
                    // Keep the saved voice if it still exists, otherwise take
                    // the first so the app is usable straight away.
                    let saved_is_valid = voices
                        .iter()
                        .any(|v| v.id == self.config.elevenlabs_voice_id);
                    if !saved_is_valid && let Some(first) = voices.first() {
                        self.config.elevenlabs_voice_id = first.id.clone();
                        self.config.elevenlabs_voice_name = first.name.clone();
                        self.config_dirty = true;
                    }
                    self.elevenlabs_voices = VoiceList {
                        voices,
                        error: None,
                        loaded: true,
                    };
                    // The voice list is the key check, so this is the moment a
                    // key the user just typed is known to be good.
                    if std::mem::take(&mut self.checking_key) {
                        self.set_status(t!("status.key_accepted"), Tone::Success);
                    }
                }
                Update::Mp3Ready(mp3) => {
                    self.cached = Some(CachedRender {
                        fingerprint: self.fingerprint(),
                        mp3: Arc::clone(&mp3),
                    });
                    self.begin_playback(mp3);
                }
                Update::Saved(path) => {
                    crate::log::line(format!("saved {}", path.display()));
                    self.set_status(t!("status.saved", path = path.display()), Tone::Success);
                }
                Update::NeedsFfmpegInstall => {
                    self.prompt = Some(Prompt::InstallFfmpeg);
                }
                Update::NeedsOllamaInstall => {
                    self.prompt = Some(Prompt::InstallOllama);
                }
                Update::NeedsModel(model) => {
                    self.prompt = Some(Prompt::PullModel(model));
                }
                // Whatever was waiting on Ollama is in `deferred` and starts as
                // soon as the setup job sends `Finished`.
                Update::SetupComplete(message) => self.set_status(message, Tone::Success),
                Update::ElevenLabsVoicesFailed(message) => {
                    // Not a rejection — the key may be perfectly good and the
                    // network not — so the key stays and the failure stays by
                    // the picker. A check the user asked for by entering a key
                    // is still owed an answer, out loud.
                    if std::mem::take(&mut self.checking_key) {
                        self.set_status(t!("status.key_unchecked", error = message), Tone::Error);
                    }
                    self.elevenlabs_voices = VoiceList {
                        voices: Vec::new(),
                        error: Some(message),
                        loaded: true,
                    };
                }
                Update::ApiKeyRejected => self.forget_rejected_key(),
                Update::Error(message) => {
                    crate::log::line(format!("failed: {message}"));
                    self.set_status(message, Tone::Error);
                    // Don't retry the step that was waiting on a failed one; it
                    // would only re-raise the same prompt.
                    self.deferred = None;
                }
                Update::Finished => {
                    self.busy = None;
                    self.tick_at = None;
                    if let Some(job) = self.deferred.take() {
                        self.start(ctx, job);
                    }
                }
            }
        }

        // Playback has no completion signal, so poll it.
        if let Some(playback) = &mut self.playback
            && playback.is_finished()
        {
            let was_a_file = self.playing_audio_file;
            self.playback = None;
            self.playing_audio_file = false;
            if matches!(self.status, Some((_, Tone::Info))) {
                if was_a_file {
                    // No chime on the end of a recording: it arrives over the
                    // last word, and the file running out is not news.
                    self.set_status_quietly(t!("status.finished_playing"), Tone::Success);
                } else {
                    self.set_status(t!("status.finished_reading"), Tone::Success);
                }
            }
        }
    }

    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> = ctx.input(|input| {
            input
                .raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .collect()
        });
        let Some(path) = dropped.into_iter().next() else {
            return;
        };
        // An audio file is not something the reader can open, so it goes to the
        // player — whichever pane it was dropped on — and the player is brought
        // into view so the drop visibly landed somewhere.
        if is_playable(&path) {
            self.pane = Pane::Player;
            self.load_audio_file(path);
            return;
        }
        self.open_file(ctx, path);
    }

    /// Reads and consumes the global shortcuts. Consuming rather than observing
    /// means a shortcut never also reaches the control underneath it.
    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        // The dialog owns the keyboard while it is open, so the shortcuts
        // behind it stay inert and Escape unambiguously means "close this".
        if self.show_api_key || self.prompt.is_some() {
            return;
        }

        // Escape only counts as "stop" when there is something to stop, so it
        // stays available to close an open dropdown the rest of the time.
        let something_to_stop = self.is_playing() || self.busy.is_some();
        let (mut open, mut apply, mut stop) = (false, false, false);
        let (mut key, mut log) = (false, false);
        let (mut toggle, mut back) = (false, false);
        let mut go = None;
        ctx.input_mut(|i| {
            open = i.consume_key(Modifiers::COMMAND, Key::O);
            apply = i.consume_key(Modifiers::COMMAND, Key::Enter);
            stop = i.consume_key(Modifiers::COMMAND, Key::Period)
                || (something_to_stop && i.consume_key(Modifiers::NONE, Key::Escape));
            key = i.consume_key(Modifiers::COMMAND, Key::K);
            log = i.consume_key(Modifiers::COMMAND, Key::L);
            // The transport. Deliberately not ⌘Space or ⌘←: the first never
            // reaches an app on macOS because Spotlight takes it, and the
            // second is "beginning of line" in every text field on the
            // platform — including the dictionary's, two panes away.
            toggle = i.consume_key(Modifiers::COMMAND, Key::P);
            back = i.consume_key(Modifiers::COMMAND, Key::R);

            // One number per pane, in the order they are listed, plus the two
            // conventional ways in: F1 for help and ⌘, for settings.
            for (number, pane) in [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5]
                .into_iter()
                .zip(Pane::ALL)
            {
                if i.consume_key(Modifiers::COMMAND, number) {
                    go = Some(pane);
                }
            }
            if i.consume_key(Modifiers::NONE, Key::F1) {
                go = Some(Pane::Shortcuts);
            }
            if i.consume_key(Modifiers::COMMAND, Key::Comma) {
                go = Some(Pane::Settings);
            }
            if i.consume_key(Modifiers::COMMAND, Key::D) {
                go = Some(Pane::Dictionary);
            }
        });

        if open {
            self.choose_file_for_pane(ctx);
        }
        if apply {
            self.apply(ctx);
        }
        if stop {
            self.stop_everything();
        }
        // Both work from any pane, and bring the player into view — but without
        // moving the keyboard, which mid-playback would land it somewhere the
        // user did not ask to be.
        if toggle || back {
            self.pane = Pane::Player;
        }
        if toggle {
            self.player_toggle();
        }
        if back {
            self.player_skip_back();
        }
        if key {
            self.open_api_key_dialog();
        }
        if log && !self.log.is_empty() {
            self.show_log = !self.show_log;
        }
        if let Some(pane) = go {
            self.show_pane(pane);
        }
    }

    /// Switches pane and sends the keyboard with it, so a shortcut leaves focus
    /// somewhere useful rather than back where the old pane had it.
    fn show_pane(&mut self, pane: Pane) {
        self.pane = pane;
        self.focus_tab = Some(pane);
    }

    // ------------------------------------------------------------------ ui

    /// A caption drawn above its control. The returned id is handed to
    /// [`egui::Response::labelled_by`] so assistive technology announces the
    /// caption as the control's name.
    fn caption(ui: &mut egui::Ui, text: &str) -> egui::Response {
        ui.add_space(2.0);
        ui.label(RichText::new(text).size(15.0))
    }

    /// Gives the keyboard to `response` if this field was the jump target.
    fn take_focus(&mut self, field: Field, response: &egui::Response) {
        if self.focus == Some(field) {
            response.request_focus();
            response.scroll_to_me(Some(egui::Align::Center));
            self.focus = None;
        }
    }

    fn file_field(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let caption = Self::caption(ui, &t!("read.file.caption"));

        let button = ui.add_enabled(
            self.busy.is_none(),
            egui::Button::new(t!("read.file.button"))
                .min_size(egui::vec2(FORM_WIDTH, CONTROL_HEIGHT)),
        );
        let button = button.on_hover_text(t!("read.file.hint"));
        let _ = caption.labelled_by(button.id);
        self.take_focus(Field::File, &button);
        if button.clicked() {
            self.choose_file(&ctx);
        }

        // The selection is a sentence rather than a bare filename, so it makes
        // sense on its own when a screen reader reaches it.
        let text = match &self.file {
            Some(path) => {
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                if self.text_note.is_empty() {
                    t!("read.file.chosen", name = name)
                } else {
                    t!("read.file.chosen_note", name = name, note = self.text_note)
                }
            }
            None => t!("read.file.none"),
        };
        let line = ui.add(egui::Label::new(RichText::new(text)).wrap());
        if let Some(path) = &self.file {
            line.on_hover_text(path.display().to_string());
        }
    }

    fn engine_field(&mut self, ui: &mut egui::Ui) {
        let caption = Self::caption(ui, &t!("read.engine.caption"));
        let mut chosen = self.config.engine;

        let combo = egui::ComboBox::from_id_salt("engine")
            .width(FORM_WIDTH)
            .selected_text(self.config.engine.label())
            .show_ui(ui, |ui| {
                for option in EnginePreference::ALL {
                    ui.selectable_value(&mut chosen, option, option.label())
                        .on_hover_text(option.description());
                }
            })
            .response;
        let combo = combo.on_hover_text(t!(
            "read.engine.hint",
            description = self.config.engine.description()
        ));
        let _ = caption.labelled_by(combo.id);
        self.take_focus(Field::Engine, &combo);

        if chosen != self.config.engine {
            self.config.engine = chosen;
            self.cached = None;
            self.config_dirty = true;
            // Choosing ElevenLabs is the moment the app needs a key, so this is
            // where it asks — rather than failing later, at Apply.
            if chosen == EnginePreference::ElevenLabs {
                self.open_api_key_dialog();
            }
        }

        match self.active_engine() {
            ActiveEngine::ElevenLabs => {
                let source = match self.key_source {
                    KeySource::Env => t!("read.engine.key_from_env"),
                    _ => t!("read.engine.key_stored"),
                };
                let muted = crate::theme::palette(ui.visuals()).muted;
                ui.label(RichText::new(source).color(muted));
            }
            ActiveEngine::System => {
                let muted = crate::theme::palette(ui.visuals()).muted;
                ui.label(RichText::new(system_voice_note()).color(muted));
            }
            ActiveEngine::MissingKey => {
                let colour = crate::theme::palette(ui.visuals()).warn;
                ui.colored_label(colour, t!("read.engine.no_key"));
                if ui
                    .add(
                        egui::Button::new(t!("read.engine.enter_key"))
                            .min_size(egui::vec2(FORM_WIDTH, CONTROL_HEIGHT)),
                    )
                    .on_hover_text(shortcut_text("key", "keys"))
                    .clicked()
                {
                    self.open_api_key_dialog();
                }
            }
            ActiveEngine::Unsupported => {
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    tts::system::unsupported_message(),
                );
            }
        }
    }

    fn voice_field(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let caption = Self::caption(ui, &t!("read.voice.caption"));

        match self.config.engine {
            EnginePreference::ElevenLabs => {
                if self.api_key.is_some() && !self.elevenlabs_voices.loaded && self.busy.is_none() {
                    self.load_elevenlabs_voices(&ctx);
                }
                let enabled = self.api_key.is_some();
                let selected = self.elevenlabs_selection();

                let combo = ui
                    .add_enabled_ui(enabled, |ui| {
                        egui::ComboBox::from_id_salt("elevenlabs_voice")
                            .width(FORM_WIDTH)
                            .selected_text(selected)
                            .show_ui(ui, |ui| {
                                for voice in &self.elevenlabs_voices.voices {
                                    let picked = self.config.elevenlabs_voice_id == voice.id;
                                    if ui.selectable_label(picked, voice.display()).clicked()
                                        && !picked
                                    {
                                        self.config.elevenlabs_voice_id = voice.id.clone();
                                        self.config.elevenlabs_voice_name = voice.name.clone();
                                        self.cached = None;
                                        self.config_dirty = true;
                                    }
                                }
                            })
                            .response
                    })
                    .inner;
                let combo = combo.on_hover_text(t!("read.voice.hint_elevenlabs"));
                let _ = caption.labelled_by(combo.id);
                self.take_focus(Field::Voice, &combo);

                if let Some(error) = self.elevenlabs_voices.error.clone() {
                    ui.colored_label(
                        ui.visuals().error_fg_color,
                        t!("read.voice.failed", error = error),
                    );
                    if ui
                        .add(
                            egui::Button::new(t!("read.voice.retry"))
                                .min_size(egui::vec2(FORM_WIDTH, CONTROL_HEIGHT)),
                        )
                        .clicked()
                    {
                        self.elevenlabs_voices.loaded = false;
                    }
                }
            }
            EnginePreference::System => {
                let selected = self
                    .system_voices
                    .voices
                    .iter()
                    .find(|v| v.id == self.config.system_voice)
                    .map(Voice::display)
                    .unwrap_or_else(|| t!("read.voice.default"));

                let combo = egui::ComboBox::from_id_salt("system_voice")
                    .width(FORM_WIDTH)
                    .selected_text(selected)
                    .show_ui(ui, |ui| {
                        // Keeps a way back to the computer's own chosen voice
                        // after picking a specific one.
                        if ui
                            .selectable_label(
                                self.config.system_voice.is_empty(),
                                t!("read.voice.default"),
                            )
                            .clicked()
                        {
                            self.config.system_voice.clear();
                            self.cached = None;
                            self.config_dirty = true;
                        }
                        ui.separator();
                        for voice in &self.system_voices.voices {
                            let picked = self.config.system_voice == voice.id;
                            if ui.selectable_label(picked, voice.display()).clicked() && !picked {
                                self.config.system_voice = voice.id.clone();
                                self.cached = None;
                                self.config_dirty = true;
                            }
                        }
                    })
                    .response;
                let combo = combo.on_hover_text(t!("read.voice.hint_system"));
                let _ = caption.labelled_by(combo.id);
                self.take_focus(Field::Voice, &combo);

                if let Some(error) = &self.system_voices.error {
                    ui.colored_label(ui.visuals().error_fg_color, error.clone());
                }

                // The value goes in the caption rather than in a box beside the
                // rail, which is what lets the rail itself be the same width as
                // every other control — and reads better aloud, since the
                // caption is what a screen reader announces as the name.
                let speed = Self::caption(
                    ui,
                    &t!("read.voice.rate.caption", rate = self.config.system_rate),
                );
                ui.spacing_mut().slider_width = FORM_WIDTH;
                let slider = ui.add_sized(
                    [FORM_WIDTH, CONTROL_HEIGHT],
                    egui::Slider::new(&mut self.config.system_rate, 90..=350)
                        .show_value(false)
                        .clamping(egui::SliderClamping::Always),
                );
                let slider = slider.on_hover_text(t!("read.voice.rate.hint"));
                let _ = speed.labelled_by(slider.id);
                if slider.changed() {
                    self.cached = None;
                    self.config_dirty = true;
                }
            }
        }
    }

    /// What the ElevenLabs voice dropdown should say right now.
    fn elevenlabs_selection(&self) -> String {
        if self.api_key.is_none() {
            return t!("read.voice.needs_key");
        }
        self.elevenlabs_voices
            .voices
            .iter()
            .find(|v| v.id == self.config.elevenlabs_voice_id)
            .map(Voice::display)
            .or_else(|| {
                Some(self.config.elevenlabs_voice_name.clone()).filter(|name| !name.is_empty())
            })
            .unwrap_or_else(|| {
                if self.elevenlabs_voices.error.is_some() {
                    t!("read.voice.unavailable")
                } else {
                    t!("read.voice.loading")
                }
            })
    }

    /// Whether a Word document's formatting is read out along with its words.
    ///
    /// Shown for every file rather than only for a `.docx`, so it does not
    /// appear and vanish as files are opened — a control that moves is a
    /// control nobody can find twice, and Tab order that changes underneath
    /// someone is worse still. It is disabled, with a reason, when the open
    /// file has no formatting to read.
    fn formatting_field(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let caption = Self::caption(ui, &t!("read.formatting.caption"));
        let mut chosen = self.config.formatting;
        // Nothing else this app opens carries formatting: a `.txt` has none, a
        // CSV is already announced as a table, and an image comes back as
        // whatever the vision model wrote.
        let applies = self.file_kind.is_none_or(|kind| kind == FileKind::Docx);

        let combo = ui
            .add_enabled_ui(applies, |ui| {
                egui::ComboBox::from_id_salt("formatting")
                    .width(FORM_WIDTH)
                    .selected_text(self.config.formatting.label())
                    .show_ui(ui, |ui| {
                        for option in Formatting::ALL {
                            ui.selectable_value(&mut chosen, option, option.label())
                                .on_hover_text(option.description());
                        }
                    })
                    .response
            })
            .inner;
        let combo = combo.on_hover_text(if applies {
            self.config.formatting.description()
        } else {
            t!(
                "read.formatting.not_applicable",
                kind = self
                    .file_kind
                    .map(FileKind::label)
                    .unwrap_or_else(|| t!("filekind.unknown"))
            )
        });
        let _ = caption.labelled_by(combo.id);
        self.take_focus(Field::Formatting, &combo);

        if chosen != self.config.formatting {
            self.config.formatting = chosen;
            self.config_dirty = true;
            self.cached = None;
            // The text on screen was extracted under the old setting, so the
            // choice would otherwise do nothing until the file was opened
            // again — which looks exactly like a broken control.
            if self.file_kind == Some(FileKind::Docx)
                && self.busy.is_none()
                && let Some(path) = self.file.clone()
            {
                self.start(
                    &ctx,
                    Job::ReadDocument {
                        path,
                        formatting: chosen,
                    },
                );
            }
        }
    }

    fn action_field(&mut self, ui: &mut egui::Ui) {
        let caption = Self::caption(ui, &t!("read.action.caption"));
        let mut chosen = self.config.action;

        let combo = egui::ComboBox::from_id_salt("action")
            .width(FORM_WIDTH)
            .selected_text(self.config.action.label())
            .show_ui(ui, |ui| {
                for option in Action::ALL {
                    ui.selectable_value(&mut chosen, option, option.label())
                        .on_hover_text(option.description());
                }
            })
            .response;
        let combo = combo.on_hover_text(t!(
            "read.action.hint",
            description = self.config.action.description()
        ));
        let _ = caption.labelled_by(combo.id);
        self.take_focus(Field::Action, &combo);

        if chosen != self.config.action {
            self.config.action = chosen;
            self.config_dirty = true;
        }

        // Only meaningful for a save, so it appears only then rather than
        // sitting there greyed out and unexplained.
        if self.config.action == Action::SaveAudio {
            let caption = Self::caption(ui, &t!("read.format.caption"));
            let mut format = self.config.save_format;
            let combo = egui::ComboBox::from_id_salt("save_format")
                .width(FORM_WIDTH)
                .selected_text(format.label())
                .show_ui(ui, |ui| {
                    for option in AudioFormat::ALL {
                        ui.selectable_value(&mut format, option, option.label());
                    }
                })
                .response;
            let _ = caption.labelled_by(combo.id);
            if format != self.config.save_format {
                self.config.save_format = format;
                self.config_dirty = true;
            }
        }
    }

    fn apply_button(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.add_space(6.0);

        // While speech is playing the same button stops it: one full-width
        // control in one place, whatever state the app is in. A file playing in
        // the audio player is not this pane's business, and has its own Stop.
        if self.is_playing() && !self.playing_audio_file {
            let stop = ui.add(
                egui::Button::new(RichText::new(t!("read.stop")).size(17.0))
                    .min_size(egui::vec2(FORM_WIDTH, CONTROL_HEIGHT + 6.0)),
            );
            let stop = stop.on_hover_text(t!("read.stop.hint"));
            self.take_focus(Field::Apply, &stop);
            if stop.clicked() {
                self.stop_playback();
                self.set_status(t!("status.stopped_reading"), Tone::Info);
            }
            return;
        }

        let ready = self.busy.is_none() && !self.text.trim().is_empty();
        let button = ui.add_enabled(
            ready,
            egui::Button::new(RichText::new(t!("read.apply")).size(17.0))
                .min_size(egui::vec2(FORM_WIDTH, CONTROL_HEIGHT + 6.0)),
        );
        let button = button.on_hover_text(t!(
            "read.apply.hint",
            description = self.config.action.description()
        ));
        self.take_focus(Field::Apply, &button);
        if button.clicked() {
            self.apply(&ctx);
        }
    }

    fn status_row(&mut self, ui: &mut egui::Ui) {
        if let Some(busy) = &self.busy {
            let label = busy.label.clone();
            let cancellable = busy.cancellable;
            let cancel = Arc::clone(&busy.cancel);
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(&label);
            });
            if let Some(progress) = busy.progress {
                let bar = ui.add(
                    egui::ProgressBar::new(progress)
                        .desired_width(FORM_WIDTH)
                        .desired_height(PROGRESS_HEIGHT),
                );
                percentage_across(ui, bar.rect, progress);
            }
            if cancellable
                && ui
                    .add(
                        egui::Button::new(t!("status.cancel"))
                            .min_size(egui::vec2(FORM_WIDTH, CONTROL_HEIGHT)),
                    )
                    .on_hover_text(t!("read.stop.hint"))
                    .clicked()
            {
                cancel.store(true, Ordering::Relaxed);
            }
            return;
        }

        match &self.status {
            Some((message, tone)) => {
                let palette = crate::theme::palette(ui.visuals());
                let colour = match tone {
                    Tone::Info => ui.visuals().text_color(),
                    Tone::Success => palette.ok,
                    Tone::Error => palette.bad,
                };
                ui.add(
                    egui::Label::new(
                        RichText::new(format!("{}{message}", tone.prefix())).color(colour),
                    )
                    .wrap(),
                );
            }
            None => {
                let muted = crate::theme::palette(ui.visuals()).muted;
                ui.add(egui::Label::new(RichText::new(t!("read.ready_hint")).color(muted)).wrap());
            }
        }
    }

    fn log_pane(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        ui.horizontal(|ui| {
            ui.label(RichText::new(t!("log.heading")));
            if ui
                .button(t!("log.hide"))
                .on_hover_text(t!("log.hide.hint"))
                .clicked()
            {
                self.show_log = false;
            }
        });
        egui::ScrollArea::vertical()
            .max_height(150.0)
            .stick_to_bottom(true)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for line in &self.log {
                    ui.label(RichText::new(line).monospace().size(12.0));
                }
            });
    }

    // ----------------------------------------------------------- the tab rail

    /// The list of panes down the left. Each one is a real focusable control,
    /// and the arrow keys walk the list once it has focus — which is how a
    /// tab list is expected to behave, and what makes the rail usable without
    /// Tabbing through every pane to reach the last.
    fn tab_rail(&mut self, ui: &mut egui::Ui) {
        const TAB_WIDTH: f32 = 176.0;

        ui.add_space(10.0);
        let mut focused = None;
        let mut responses = Vec::with_capacity(Pane::ALL.len());

        for (index, pane) in Pane::ALL.into_iter().enumerate() {
            // A selected button rather than a plain one, so the current pane is
            // marked by fill *and* by the pressed state a screen reader reports.
            let response = ui.add(
                egui::Button::new(pane.label())
                    .selected(self.pane == pane)
                    .min_size(egui::vec2(TAB_WIDTH, CONTROL_HEIGHT + 4.0)),
            );
            let response =
                response.on_hover_text(format!("{}  ·  {}", pane.shortcut(), pane.hint()));
            if response.clicked() {
                self.pane = pane;
            }
            if response.has_focus() {
                focused = Some(index);
            }
            if self.focus_tab == Some(pane) {
                response.request_focus();
                self.focus_tab = None;
            }
            responses.push(response);
        }

        let Some(index) = focused else {
            return;
        };
        let step = ui.input_mut(|i| {
            i.consume_key(Modifiers::NONE, Key::ArrowDown) as i32
                - i.consume_key(Modifiers::NONE, Key::ArrowUp) as i32
        });
        if step != 0 {
            let next = (index as i32 + step).rem_euclid(Pane::ALL.len() as i32) as usize;
            self.pane = Pane::ALL[next];
            responses[next].request_focus();
        }
    }

    // --------------------------------------------------------- the one dialog

    /// The API key, asked for over the top of whatever the user was doing —
    /// because that is exactly what choosing ElevenLabs is: a question that
    /// interrupts choosing an engine and then goes away again.
    fn api_key_dialog_window(&mut self, ctx: &egui::Context) {
        if !self.show_api_key {
            return;
        }
        let response = egui::Modal::new(egui::Id::new("api_key")).show(ctx, |ui| {
            ui.set_max_width(520.0);
            self.api_key_dialog(ui);
        });
        // Escape and a click on the backdrop both close, which is what every
        // other dialog on both platforms does.
        if response.should_close() {
            self.close_dialog();
        }
    }

    fn close_dialog(&mut self) {
        self.show_api_key = false;
        self.dialog_opened = false;
        self.key_input.clear();
        // The rejection notice explains why the dialog opened by itself, so it
        // goes when the dialog does. Opening it again later — to change a
        // working key — should not still be reading someone their last mistake.
        self.key_rejected = false;
        self.config_dirty = true;
    }

    fn api_key_dialog(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.heading(t!("key.heading"));
        ui.add_space(6.0);

        match self.key_source {
            KeySource::Env => {
                ui.label(t!("key.from_env", variable = apikey::ENV_VAR));
            }
            KeySource::Stored => {
                ui.label(t!("key.stored"));
                ui.add_space(8.0);
                if ui
                    .add(
                        egui::Button::new(t!("key.remove"))
                            .min_size(egui::vec2(240.0, CONTROL_HEIGHT)),
                    )
                    .clicked()
                {
                    match apikey::clear() {
                        Ok(()) => {
                            self.api_key = None;
                            self.key_source = KeySource::None;
                            self.elevenlabs_voices = VoiceList::default();
                            self.set_status(t!("status.key_removed"), Tone::Success);
                        }
                        Err(error) => self.set_status(format!("{error:#}"), Tone::Error),
                    }
                }
            }
            KeySource::None => {
                // Shown once, at the top, when the last key came back rejected
                // — so the dialog reopening explains itself rather than looking
                // like it never closed.
                if self.key_rejected {
                    ui.colored_label(crate::theme::palette(ui.visuals()).bad, t!("key.rejected"));
                    ui.add_space(8.0);
                }
                ui.add(
                    egui::Label::new(t!("key.ask", storage = apikey::STORAGE_DESCRIPTION)).wrap(),
                );
                ui.add_space(8.0);

                // A button rather than a bare link, so it is the same size and
                // in the same Tab order as everything else here — and it says
                // where it goes, since "click here" is no use read aloud.
                if ui
                    .add(
                        egui::Button::new(t!("key.get"))
                            .min_size(egui::vec2(FORM_WIDTH, CONTROL_HEIGHT)),
                    )
                    .on_hover_text(t!("key.get.hint", url = ELEVENLABS_KEYS_URL))
                    .clicked()
                {
                    ctx.open_url(egui::OpenUrl::same_tab(ELEVENLABS_KEYS_URL));
                }
                ui.add_space(10.0);

                let caption = ui.label(t!("key.caption"));
                let entry = ui.add(
                    egui::TextEdit::singleline(&mut self.key_input)
                        .password(true)
                        .hint_text("sk_…")
                        .desired_width(FORM_WIDTH),
                );
                let _ = caption.labelled_by(entry.id);
                // The dialog exists to take this one value, so the keyboard
                // starts in it — once, on open, so Tab can still leave.
                if std::mem::take(&mut self.dialog_opened) {
                    entry.request_focus();
                }
                let submitted = entry.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));

                ui.add_space(10.0);
                let typed = !self.key_input.trim().is_empty();
                let save = ui.add_enabled(
                    typed,
                    egui::Button::new(t!("key.save")).min_size(egui::vec2(200.0, CONTROL_HEIGHT)),
                );
                if save.clicked() || (submitted && typed) {
                    let key = self.key_input.trim().to_string();
                    match apikey::store(&key) {
                        Ok(()) => {
                            self.api_key = Some(key);
                            self.key_source = KeySource::Stored;
                            self.elevenlabs_voices = VoiceList::default();
                            self.close_dialog();
                            self.focus = Some(Field::Voice);
                            // Deliberately `Info`, which makes no sound: a key
                            // is written to disk long before anyone knows it is
                            // any good, and a success chime here would be
                            // celebrating a key ElevenLabs is about to refuse.
                            // The sound belongs to the answer, below.
                            self.set_status(t!("status.key_saved"), Tone::Info);
                            self.checking_key = true;
                            // Asked for here rather than left to the voice
                            // picker to notice, which only happens when the
                            // Read pane is the one on screen — the key can be
                            // entered from any of them.
                            self.load_elevenlabs_voices(&ctx);
                            return;
                        }
                        Err(error) => self.set_status(format!("{error:#}"), Tone::Error),
                    }
                }
            }
        }

        ui.add_space(12.0);
        ui.separator();
        if ui
            .add(egui::Button::new(t!("key.close")).min_size(egui::vec2(160.0, CONTROL_HEIGHT)))
            .on_hover_text(t!("key.close.hint"))
            .clicked()
        {
            self.close_dialog();
        }
    }

    /// The dictionary editor: one row per rule, each row a pair of fields and a
    /// Remove button, all of them reachable in Tab order.
    fn dictionary_pane(&mut self, ui: &mut egui::Ui) {
        // Every cell is given an exact size rather than a desired one, so the
        // header line stays over the column it names however long the words in
        // the rows get.
        const CELL: f32 = 168.0;
        const WHOLE: f32 = 84.0;
        const REMOVE: f32 = 104.0;
        const GAP: f32 = 10.0;

        ui.heading(t!("dict.heading"));
        ui.add_space(6.0);
        ui.add(egui::Label::new(t!("dict.intro")).wrap());
        ui.add_space(10.0);

        let mut remove = None;
        let mut changed = false;

        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = GAP;
            // Left-aligned in a fixed-width cell, so each heading starts exactly
            // where the field below it starts.
            for (width, text) in [
                (CELL, t!("dict.column.from")),
                (CELL, t!("dict.column.to")),
                (WHOLE, t!("dict.column.whole")),
            ] {
                ui.allocate_ui_with_layout(
                    egui::vec2(width, 18.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| ui.label(text),
                );
            }
        });

        egui::ScrollArea::vertical()
            .max_height(340.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for (index, rule) in self.config.dictionary.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = GAP;
                        let from = ui.add_sized(
                            [CELL, CONTROL_HEIGHT],
                            egui::TextEdit::singleline(&mut rule.from)
                                .hint_text(t!("dict.hint.from")),
                        );
                        let to = ui.add_sized(
                            [CELL, CONTROL_HEIGHT],
                            egui::TextEdit::singleline(&mut rule.to).hint_text(t!("dict.hint.to")),
                        );
                        let whole = ui.add_sized(
                            [WHOLE, CONTROL_HEIGHT],
                            egui::Checkbox::without_text(&mut rule.whole_word),
                        );
                        let whole = whole.on_hover_text(t!("dict.whole.hint"));
                        // Named for a screen reader, which would otherwise hear
                        // a column of identical "Remove" buttons.
                        let named = if rule.from.trim().is_empty() {
                            t!("dict.remove.row", number = index + 1)
                        } else {
                            t!("dict.remove.word", word = rule.from.trim())
                        };
                        if ui
                            .add_sized(
                                [REMOVE, CONTROL_HEIGHT],
                                egui::Button::new(t!("dict.remove")),
                            )
                            .on_hover_text(named)
                            .clicked()
                        {
                            remove = Some(index);
                        }
                        changed |= from.changed() || to.changed() || whole.changed();
                    });
                }
            });

        if self.config.dictionary.is_empty() {
            let muted = crate::theme::palette(ui.visuals()).muted;
            ui.add_space(6.0);
            ui.label(RichText::new(t!("dict.empty")).color(muted));
        }

        ui.add_space(10.0);
        if ui
            .add(egui::Button::new(t!("dict.add")).min_size(egui::vec2(240.0, CONTROL_HEIGHT)))
            .clicked()
        {
            self.config
                .dictionary
                .push(crate::dictionary::Replacement::default());
            changed = true;
        }

        if let Some(index) = remove {
            self.config.dictionary.remove(index);
            changed = true;
        }
        if changed {
            // The audio rendered from the old wording is no longer what the
            // dictionary says, so it cannot be replayed.
            self.cached = None;
            self.config_dirty = true;
        }
    }

    /// The audio player: choose a file, then play, pause, stop or skip back.
    ///
    /// The four transport buttons are always present and always in the same
    /// place, greyed out rather than hidden when they don't apply — a control
    /// that appears and disappears moves everything below it, which is exactly
    /// what a screen magnifier and a screen reader's reading order cannot
    /// afford.
    fn player_pane(&mut self, ui: &mut egui::Ui) {
        const HALF: f32 = (FORM_WIDTH - 10.0) / 2.0;

        ui.heading(t!("player.heading"));
        ui.add_space(6.0);
        ui.add(egui::Label::new(t!("player.intro")).wrap());
        ui.add_space(10.0);

        // Declared out here so the buttons can be acted on after the borrow of
        // `self` inside the layout closure has ended.
        let (mut play, mut pause, mut stop, mut back) = (false, false, false, false);

        ui.vertical(|ui| {
            ui.set_max_width(FORM_WIDTH);

            let caption = Self::caption(ui, &t!("player.file.caption"));
            let choose = ui.add(
                egui::Button::new(t!("player.file.button"))
                    .min_size(egui::vec2(FORM_WIDTH, CONTROL_HEIGHT)),
            );
            let choose = choose.on_hover_text(t!("player.file.hint"));
            let _ = caption.labelled_by(choose.id);
            self.take_focus(Field::AudioFile, &choose);
            if choose.clicked() {
                self.choose_audio_file();
            }

            let chosen = match &self.audio_file {
                Some(path) => t!(
                    "player.file.chosen",
                    name = path.file_name().unwrap_or_default().to_string_lossy()
                ),
                None => t!("player.file.none"),
            };
            let line = ui.add(egui::Label::new(RichText::new(chosen)).wrap());
            if let Some(path) = &self.audio_file {
                line.on_hover_text(path.display().to_string());
            }

            ui.add_space(8.0);

            let has_file = self.audio_file.is_some();
            let active = self.player_is_active();
            let paused = self.player_is_paused();

            // Play resumes a paused file, so it is enabled whenever there is
            // something to start or something to continue.
            ui.horizontal(|ui| {
                let button = ui
                    .add_enabled(
                        has_file && (!active || paused),
                        egui::Button::new(RichText::new(t!("player.play")).size(16.0))
                            .min_size(egui::vec2(HALF, CONTROL_HEIGHT + 6.0)),
                    )
                    .on_hover_text(if paused {
                        t!("player.play.hint_resume")
                    } else {
                        t!("player.play.hint_start")
                    });
                // Choosing a file sends the keyboard here, so the next thing
                // after picking is the one thing you came to do.
                self.take_focus(Field::Play, &button);
                play = button.clicked();

                pause = ui
                    .add_enabled(
                        active && !paused,
                        egui::Button::new(RichText::new(t!("player.pause")).size(16.0))
                            .min_size(egui::vec2(HALF, CONTROL_HEIGHT + 6.0)),
                    )
                    .on_hover_text(t!("player.pause.hint"))
                    .clicked();
            });
            ui.horizontal(|ui| {
                stop = ui
                    .add_enabled(
                        active,
                        egui::Button::new(RichText::new(t!("player.stop")).size(16.0))
                            .min_size(egui::vec2(HALF, CONTROL_HEIGHT + 6.0)),
                    )
                    .on_hover_text(t!("player.stop.hint"))
                    .clicked();
                back = ui
                    .add_enabled(
                        active,
                        egui::Button::new(RichText::new(t!("player.back")).size(16.0))
                            .min_size(egui::vec2(HALF, CONTROL_HEIGHT + 6.0)),
                    )
                    .on_hover_text(t!("player.back.hint"))
                    .clicked();
            });

            ui.add_space(10.0);
            self.player_position(ui);
        });

        if play {
            self.player_play();
        }
        if pause {
            self.player_pause();
        }
        if stop {
            self.player_stop();
        }
        if back {
            self.player_skip_back();
        }
    }

    /// Where the file has got to, in words and — where the length is known —
    /// as a bar. The words come first because they are the part that works
    /// without sight, and they say the same thing the bar shows.
    fn player_position(&mut self, ui: &mut egui::Ui) {
        let Some(playback) = &self.playback else {
            let muted = crate::theme::palette(ui.visuals()).muted;
            let text = if self.audio_file.is_some() {
                t!("player.idle_ready")
            } else {
                t!("player.idle")
            };
            ui.label(RichText::new(text).color(muted));
            return;
        };
        if !self.playing_audio_file {
            let muted = crate::theme::palette(ui.visuals()).muted;
            ui.label(RichText::new(t!("player.reading_instead")).color(muted));
            return;
        }

        let position = playback.position();
        let duration = playback.duration();
        let paused = playback.is_paused();
        let at = audio::spoken_time(position);
        let text = match (duration, paused) {
            (Some(total), true) => t!(
                "player.position.paused_of",
                at = at,
                total = audio::spoken_time(total)
            ),
            (Some(total), false) => t!(
                "player.position.playing_of",
                at = at,
                total = audio::spoken_time(total)
            ),
            (None, true) => t!("player.position.paused", at = at),
            (None, false) => t!("player.position.playing", at = at),
        };
        ui.label(RichText::new(text).size(15.0));

        // The time left, on its own line and in the largest type on the pane,
        // because "how much longer" is the question a listener actually has and
        // working it out from two other numbers is not an answer. Only shown
        // when the length is known: audio stitched from several ElevenLabs
        // responses often reports none, and a countdown from nowhere would be
        // worse than no countdown.
        if let Some(total) = duration {
            let left = audio::spoken_time(audio::time_left(position, total));
            ui.label(
                RichText::new(t!("player.position.left", left = left))
                    .size(17.0)
                    .strong(),
            );
        }

        if let Some(total) = duration.filter(|d| !d.is_zero()) {
            let fraction = (position.as_secs_f32() / total.as_secs_f32()).clamp(0.0, 1.0);
            ui.add(
                egui::ProgressBar::new(fraction)
                    .desired_width(FORM_WIDTH)
                    .desired_height(10.0),
            );
        }
    }

    fn shortcuts_pane(&mut self, ui: &mut egui::Ui) {
        ui.heading(t!("shortcuts.heading"));
        ui.add_space(6.0);
        ui.add(egui::Label::new(t!("shortcuts.intro")).wrap());
        ui.add_space(12.0);

        egui::Grid::new("shortcuts")
            .num_columns(2)
            .spacing([24.0, 10.0])
            .striped(true)
            .show(ui, |ui| {
                for name in SHORTCUTS {
                    // Not monospaced: in a monospace face ⌘O reads as ⌘0.
                    ui.label(shortcut_text(name, "keys"));
                    ui.label(shortcut_text(name, "what"));
                    ui.end_row();
                }
            });
    }

    fn settings_pane(&mut self, ui: &mut egui::Ui) {
        ui.heading(t!("settings.heading"));
        ui.add_space(6.0);

        let mut edited = self.language_setting(ui);

        ui.add_space(12.0);
        ui.separator();
        let caption = ui.label(t!("settings.model.caption"));
        let model = ui.add(
            egui::TextEdit::singleline(&mut self.config.elevenlabs_model_id)
                .desired_width(FORM_WIDTH),
        );
        let _ = caption.labelled_by(model.id);
        edited |= model.changed();
        model.on_hover_text(t!("settings.model.hint"));

        ui.add_space(12.0);
        ui.separator();
        let sounds = ui
            .checkbox(&mut self.config.sound_effects, t!("settings.sounds"))
            .on_hover_text(t!("settings.sounds.hint"));
        if sounds.changed() {
            edited = true;
            // The confirmation of turning them on is the sound itself, which
            // is also the only way to find out they are audible at all.
            if self.config.sound_effects {
                self.play_cue(Tone::Success);
            }
        }

        // Indented under the setting it depends on, and disabled with it: with
        // sounds off there is nothing for this to be a choice about.
        ui.indent("tick_setting", |ui| {
            let tick = ui
                .add_enabled(
                    self.config.sound_effects,
                    egui::Checkbox::new(&mut self.config.progress_tick, t!("settings.tick")),
                )
                .on_hover_text(t!("settings.tick.hint"));
            if tick.changed() {
                edited = true;
                // Same reasoning as the sound above: the answer to "what does
                // that sound like?" is the sound.
                if self.config.progress_tick {
                    self.sound(TICK_SOUND, ClaimsTheGap::No);
                }
            }
        });

        if cfg!(target_os = "windows") {
            ui.add_space(12.0);
            ui.separator();
            ui.add(egui::Label::new(t!("settings.context_menu.intro")).wrap());
            ui.add_space(6.0);

            let muted = crate::theme::palette(ui.visuals()).muted;
            let status = if self.context_menu_installed {
                match crate::context_menu::install_path() {
                    Some(path) => t!("settings.context_menu.installed_at", path = path.display()),
                    None => t!("settings.context_menu.installed"),
                }
            } else {
                t!("settings.context_menu.not_installed")
            };
            ui.add(egui::Label::new(RichText::new(status).color(muted)).wrap());
            ui.add_space(6.0);

            let label = if self.context_menu_installed {
                t!("settings.context_menu.disable")
            } else {
                t!("settings.context_menu.enable")
            };
            if ui
                .add(egui::Button::new(label).min_size(egui::vec2(FORM_WIDTH, CONTROL_HEIGHT)))
                .clicked()
            {
                let outcome = if self.context_menu_installed {
                    crate::context_menu::uninstall().map(|()| t!("settings.context_menu.disabled"))
                } else {
                    crate::context_menu::install()
                        .map(|path| t!("settings.context_menu.enabled", path = path.display()))
                };
                match outcome {
                    Ok(message) => {
                        self.context_menu_installed = !self.context_menu_installed;
                        self.set_status(message, Tone::Success);
                    }
                    Err(error) => self.set_status(format!("{error:#}"), Tone::Error),
                }
            }
        }

        ui.add_space(12.0);
        ui.separator();
        ui.add(egui::Label::new(t!("settings.vision.intro")).wrap());
        ui.add_space(6.0);

        // A dropdown rather than a text field, because the name has to match a
        // model Ollama can actually run — and, since Ollama drops support for
        // older ones, a typo and a retired model look identical from here.
        // Anything not on the list still works: it just has to be typed into
        // the field the "Other" entry reveals.
        let listed = crate::config::VISION_MODELS
            .iter()
            .find(|(id, _)| *id == self.config.ollama_model);
        let caption = ui.label(t!("settings.vision.caption"));
        let mut chosen = self.config.ollama_model.clone();
        let vision = egui::ComboBox::from_id_salt("vision_model")
            .width(FORM_WIDTH)
            .selected_text(match listed {
                Some((id, _)) => (*id).to_string(),
                None => t!(
                    "settings.vision.other_selected",
                    model = self.config.ollama_model
                ),
            })
            .show_ui(ui, |ui| {
                for (id, description) in crate::config::VISION_MODELS {
                    ui.selectable_value(&mut chosen, (*id).to_string(), *id)
                        .on_hover_text(crate::config::vision_model_description(description));
                }
                if ui
                    .selectable_label(listed.is_none(), t!("settings.vision.other"))
                    .on_hover_text(t!("settings.vision.other.hint"))
                    .clicked()
                    && listed.is_some()
                {
                    chosen = String::new();
                }
            })
            .response;
        let _ = caption.labelled_by(vision.id);
        if chosen != self.config.ollama_model {
            self.config.ollama_model = chosen;
            edited = true;
        }

        // The description of whichever model is selected, so the size and the
        // trade-off are visible without opening the dropdown and hovering.
        let muted = crate::theme::palette(ui.visuals()).muted;
        match listed {
            Some((_, description)) => {
                let description = crate::config::vision_model_description(description);
                ui.add(egui::Label::new(RichText::new(description).color(muted)).wrap());
            }
            None => {
                let caption = ui.label(t!("settings.vision.name.caption"));
                let typed = ui.add(
                    egui::TextEdit::singleline(&mut self.config.ollama_model)
                        .hint_text(t!("settings.vision.name.hint"))
                        .desired_width(FORM_WIDTH),
                );
                let _ = caption.labelled_by(typed.id);
                edited |= typed.changed();
                if crate::config::is_retired(&self.config.ollama_model) {
                    ui.colored_label(
                        crate::theme::palette(ui.visuals()).bad,
                        t!("settings.vision.retired"),
                    );
                }
            }
        }

        ui.add_space(6.0);
        let caption = ui.label(t!("settings.vision.prompt.caption"));
        let prompt = ui.add(
            egui::TextEdit::multiline(&mut self.config.ollama_prompt)
                .desired_rows(4)
                .desired_width(FORM_WIDTH),
        );
        let _ = caption.labelled_by(prompt.id);
        edited |= prompt.changed();

        // A pane has no "Done" button to save on, so every keystroke marks the
        // config dirty; it is written once at the end of the frame regardless.
        if edited {
            self.config_dirty = true;
        }
        if ui.button(t!("settings.vision.prompt.reset")).clicked() {
            self.config.ollama_prompt = crate::config::default_vision_prompt();
            self.config_dirty = true;
        }

        if self.video_settings(ui) {
            self.config_dirty = true;
        }

        ui.add_space(12.0);
        ui.separator();
        ui.add(egui::Label::new(t!("settings.reset.intro")).wrap());
        ui.add_space(8.0);
        if ui
            .add(
                egui::Button::new(t!("settings.reset.button"))
                    .min_size(egui::vec2(FORM_WIDTH, CONTROL_HEIGHT)),
            )
            .clicked()
        {
            self.confirm_reset = true;
        }

        ui.add_space(12.0);
        ui.separator();
        ui.add(egui::Label::new(t!("settings.diagnostics.intro")).wrap());
        ui.add_space(8.0);
        // Copied to the clipboard rather than revealed in a file manager: the
        // people this app is built for should not have to go and find a file
        // on disk to report a problem with it.
        if ui
            .add(
                egui::Button::new(t!("settings.diagnostics.button"))
                    .min_size(egui::vec2(FORM_WIDTH, CONTROL_HEIGHT)),
            )
            .clicked()
        {
            let diagnostics = crate::log::contents();
            let lines = diagnostics.lines().count();
            ui.ctx().copy_text(diagnostics);
            self.set_status(tn!("settings.diagnostics.copied", lines), Tone::Success);
        }

        ui.add_space(12.0);
        ui.separator();
        let muted = crate::theme::palette(ui.visuals()).muted;
        if let Some(path) = Config::path() {
            ui.add(
                egui::Label::new(
                    RichText::new(t!("settings.file.config", path = path.display())).color(muted),
                )
                .wrap(),
            );
        }
        if let Some(path) = crate::log::path() {
            ui.add(
                egui::Label::new(
                    RichText::new(t!("settings.file.log", path = path.display())).color(muted),
                )
                .wrap(),
            );
        }
    }

    /// The language picker, and the folder a translator works in.
    ///
    /// First on the page, deliberately. Someone who has opened the app in a
    /// language they cannot read needs one control, and the only thing they can
    /// rely on to find it is where it sits — not what it says.
    ///
    /// Returns whether anything changed.
    fn language_setting(&mut self, ui: &mut egui::Ui) -> bool {
        let mut edited = false;
        let caption = ui.label(t!("settings.language.caption"));

        // Each language is named in its own language, since somebody looking
        // for theirs is looking for the word they call it by, not ours.
        let available = i18n::available();
        let selected = if self.config.language == AUTO_LANGUAGE {
            t!("settings.language.system")
        } else {
            available
                .iter()
                .find(|(code, _)| *code == self.config.language)
                .map(|(_, name)| name.clone())
                .unwrap_or_else(|| self.config.language.clone())
        };

        let mut chosen = self.config.language.clone();
        let combo = egui::ComboBox::from_id_salt("language")
            .width(FORM_WIDTH)
            .selected_text(selected)
            .show_ui(ui, |ui| {
                ui.selectable_value(
                    &mut chosen,
                    AUTO_LANGUAGE.to_string(),
                    t!("settings.language.system"),
                );
                ui.separator();
                for (code, name) in &available {
                    ui.selectable_value(&mut chosen, code.clone(), name);
                }
            })
            .response;
        let combo = combo.on_hover_text(t!("settings.language.hint"));
        let _ = caption.labelled_by(combo.id);

        if chosen != self.config.language {
            self.config.language = chosen;
            edited = true;
            self.switch_language();
        }

        // Whatever the current file could not be read as, said plainly: a
        // translator's first draft always has a stray quote in it somewhere,
        // and hunting for it without a line number is miserable.
        let problems = i18n::current_problems();
        if !problems.is_empty() {
            let bad = crate::theme::palette(ui.visuals()).bad;
            ui.add_space(4.0);
            ui.add(
                egui::Label::new(
                    RichText::new(tn!("settings.language.problem_count", problems.len()))
                        .color(bad),
                )
                .wrap(),
            );
            for problem in problems.iter().take(10) {
                ui.add(
                    egui::Label::new(
                        RichText::new(t!(
                            "settings.language.problem",
                            line = problem.line,
                            what = problem.what
                        ))
                        .color(bad),
                    )
                    .wrap(),
                );
            }
        }

        ui.add_space(6.0);
        ui.add(egui::Label::new(t!("settings.language.help")).wrap());
        ui.add_space(6.0);

        if let Some(dir) = i18n::languages_dir() {
            let muted = crate::theme::palette(ui.visuals()).muted;
            ui.add(
                egui::Label::new(
                    RichText::new(t!("settings.language.folder", path = dir.display()))
                        .color(muted),
                )
                .wrap(),
            );
            ui.add_space(6.0);
            if ui
                .add(
                    egui::Button::new(t!("settings.language.open_folder"))
                        .min_size(egui::vec2(FORM_WIDTH, CONTROL_HEIGHT)),
                )
                .clicked()
            {
                // Created on the way, so the button never opens nothing —
                // "put a file in this folder" is no use if the folder is only
                // made once a file is already in it.
                if let Err(error) = std::fs::create_dir_all(&dir) {
                    crate::log::line(format!(
                        "languages: could not create {} — {error}",
                        dir.display()
                    ));
                }
                ui.ctx()
                    .open_url(egui::OpenUrl::same_tab(format!("file://{}", dir.display())));
            }
        }

        if ui
            .add(
                egui::Button::new(t!("settings.language.reload"))
                    .min_size(egui::vec2(FORM_WIDTH, CONTROL_HEIGHT)),
            )
            .clicked()
        {
            i18n::reload();
            let name = self.language_name();
            self.set_status(t!("settings.language.reloaded", name = name), Tone::Success);
        }

        edited
    }

    /// Puts the newly chosen language into use, and moves the prompts with it.
    fn switch_language(&mut self) {
        i18n::apply_setting(&self.config.language);
        let kept = self.retranslate_prompts();
        let name = self.language_name();
        let message = if kept {
            t!("settings.language.prompts_kept", name = name)
        } else {
            t!("settings.language.changed", name = name)
        };
        self.set_status(message, Tone::Success);
    }

    /// The language in use, named in itself.
    fn language_name(&self) -> String {
        let code = i18n::current_code();
        i18n::available()
            .into_iter()
            .find(|(candidate, _)| *candidate == code)
            .map(|(_, name)| name)
            .unwrap_or(code)
    }

    /// Moves the three model prompts to the new language, leaving anything the
    /// user has written themselves exactly as they wrote it.
    ///
    /// Returns true if at least one prompt was left alone, which is worth
    /// saying out loud: otherwise a French interface quietly goes on asking the
    /// model in English and nothing on screen explains why.
    fn retranslate_prompts(&mut self) -> bool {
        let mut kept = false;
        for (key, current, fresh) in [
            (
                "prompt.vision",
                &mut self.config.ollama_prompt,
                crate::config::default_vision_prompt(),
            ),
            (
                "prompt.frame",
                &mut self.config.video_frame_prompt,
                crate::config::default_frame_prompt(),
            ),
            (
                "prompt.narration",
                &mut self.config.video_narration_prompt,
                crate::config::default_narration_prompt(),
            ),
        ] {
            if i18n::is_untouched_prompt(key, current) {
                *current = fresh;
            } else {
                kept = true;
            }
        }
        self.config_dirty = true;
        kept
    }

    /// The video half of the Settings pane. Returns whether anything changed.
    ///
    /// Its own function because the settings pane was already long, and because
    /// these controls are unlike the rest of the app's: everything else here
    /// changes how something sounds, and these change how long the user waits.
    /// The numbers are all worded in those terms.
    fn video_settings(&mut self, ui: &mut egui::Ui) -> bool {
        let mut edited = false;

        ui.add_space(12.0);
        ui.separator();
        ui.add(egui::Label::new(t!("settings.video.intro")).wrap());
        ui.add_space(6.0);

        let narrate = ui
            .checkbox(&mut self.config.video_narrate, t!("settings.video.narrate"))
            .on_hover_text(t!("settings.video.narrate.hint"));
        edited |= narrate.changed();

        if self.config.video_narrate {
            let caption = ui.label(t!("settings.video.narrator.caption"));
            let narrator = ui.add(
                egui::TextEdit::singleline(&mut self.config.narration_model)
                    .hint_text(t!(
                        "settings.video.narrator.placeholder",
                        model = self.config.ollama_model
                    ))
                    .desired_width(FORM_WIDTH),
            );
            let narrator = narrator.on_hover_text(t!("settings.video.narrator.hint"));
            let _ = caption.labelled_by(narrator.id);
            edited |= narrator.changed();
        }

        ui.add_space(6.0);
        let caption = ui.label(t!("settings.video.scene.caption"));
        ui.spacing_mut().slider_width = FORM_WIDTH;
        let scene = ui.add_sized(
            [FORM_WIDTH, CONTROL_HEIGHT],
            egui::Slider::new(&mut self.config.video_scene_threshold, 0.05..=1.0)
                .show_value(false)
                .clamping(egui::SliderClamping::Always),
        );
        let scene = scene.on_hover_text(t!("settings.video.scene.hint"));
        let _ = caption.labelled_by(scene.id);
        edited |= scene.changed();
        ui.label(
            RichText::new(describe_sensitivity(self.config.video_scene_threshold))
                .color(crate::theme::palette(ui.visuals()).muted),
        );

        ui.add_space(6.0);
        let caption = ui.label(t!("settings.video.interval.caption"));
        let interval = ui.add_sized(
            [FORM_WIDTH, CONTROL_HEIGHT],
            egui::Slider::new(&mut self.config.video_interval_secs, 5..=300)
                .show_value(false)
                .clamping(egui::SliderClamping::Always),
        );
        let interval = interval.on_hover_text(t!("settings.video.interval.hint"));
        let _ = caption.labelled_by(interval.id);
        edited |= interval.changed();
        ui.label(
            RichText::new(t!(
                "settings.video.interval.value",
                time =
                    audio::spoken_time(Duration::from_secs(self.config.video_interval_secs as u64))
            ))
            .color(crate::theme::palette(ui.visuals()).muted),
        );

        ui.add_space(6.0);
        let caption = ui.label(t!("settings.video.max.caption"));
        let cap = ui.add_sized(
            [FORM_WIDTH, CONTROL_HEIGHT],
            egui::Slider::new(
                &mut self.config.video_max_frames,
                1..=crate::config::MAX_FRAMES_LIMIT,
            )
            .show_value(false)
            .clamping(egui::SliderClamping::Always),
        );
        let cap = cap.on_hover_text(t!("settings.video.max.hint"));
        let _ = caption.labelled_by(cap.id);
        edited |= cap.changed();
        ui.label(
            RichText::new(t!(
                "settings.video.max.value",
                frames = self.config.video_max_frames,
                time = audio::spoken_time(Duration::from_secs(
                    self.config.video_max_frames as u64 * SECONDS_PER_FRAME_ESTIMATE
                ))
            ))
            .color(crate::theme::palette(ui.visuals()).muted),
        );

        ui.add_space(6.0);
        let caption = ui.label(t!("settings.video.frame_prompt.caption"));
        let frame_prompt = ui.add(
            egui::TextEdit::multiline(&mut self.config.video_frame_prompt)
                .desired_rows(3)
                .desired_width(FORM_WIDTH),
        );
        let _ = caption.labelled_by(frame_prompt.id);
        edited |= frame_prompt.changed();

        if self.config.video_narrate {
            ui.add_space(6.0);
            let caption = ui.label(t!("settings.video.narration_prompt.caption"));
            let narration_prompt = ui.add(
                egui::TextEdit::multiline(&mut self.config.video_narration_prompt)
                    .desired_rows(3)
                    .desired_width(FORM_WIDTH),
            );
            let _ = caption.labelled_by(narration_prompt.id);
            edited |= narration_prompt.changed();
        }

        if ui.button(t!("settings.video.prompt.reset")).clicked() {
            self.config.video_frame_prompt = crate::config::default_frame_prompt();
            self.config.video_narration_prompt = crate::config::default_narration_prompt();
            self.config_dirty = true;
        }

        edited
    }

    /// Confirms before putting the settings back to their defaults.
    ///
    /// Asking first because the damage is invisible: a reset takes back a
    /// deliberately chosen voice, speaking rate and vision model all at once,
    /// and none of it announces itself — the next thing the user hears is a
    /// different voice reading at a different speed, with nothing to say why.
    fn reset_dialog(&mut self, ctx: &egui::Context) {
        if !self.confirm_reset {
            return;
        }
        let mut decision: Option<bool> = None;

        egui::Modal::new(egui::Id::new("confirm_reset")).show(ctx, |ui| {
            ui.set_max_width(560.0);
            ui.heading(t!("reset.heading"));
            ui.add_space(6.0);
            ui.add(egui::Label::new(t!("reset.what")).wrap());
            ui.add_space(6.0);
            // Said plainly, because "reset" is exactly the word that makes
            // someone worry about the list of replacements they built up.
            ui.add(egui::Label::new(t!("reset.kept")).wrap());
            ui.add_space(10.0);

            if ui
                .add(
                    egui::Button::new(t!("reset.confirm"))
                        .min_size(egui::vec2(240.0, CONTROL_HEIGHT)),
                )
                .clicked()
            {
                decision = Some(true);
            }
            if ui
                .add(
                    egui::Button::new(t!("reset.cancel"))
                        .min_size(egui::vec2(240.0, CONTROL_HEIGHT)),
                )
                .clicked()
            {
                decision = Some(false);
            }
        });

        let Some(confirmed) = decision else {
            return;
        };
        self.confirm_reset = false;
        if !confirmed {
            self.set_status(t!("status.settings_unchanged"), Tone::Info);
            return;
        }

        self.config.reset_to_defaults();
        // The engine and voice have both just changed, so anything rendered
        // under the old ones is no longer what Apply would produce.
        self.cached = None;
        self.config_dirty = true;
        crate::log::line("settings reset to defaults");
        self.set_status(t!("status.settings_reset"), Tone::Success);
    }

    fn prompt_dialog(&mut self, ctx: &egui::Context) {
        let Some(prompt) = &self.prompt else {
            return;
        };
        let busy = self.busy.is_some();
        let mut decision: Option<Option<Job>> = None;

        // The dialog is about whatever the user just opened, which is the only
        // reason any of it is being asked.
        let heading = if self.file_kind == Some(FileKind::Video) {
            t!("install.heading.video")
        } else {
            t!("install.heading.image")
        };

        egui::Modal::new(egui::Id::new("ollama_prompt")).show(ctx, |ui| {
            ui.set_max_width(560.0);
            ui.heading(heading);
            ui.add_space(6.0);
            match prompt {
                Prompt::InstallOllama => {
                    ui.add(egui::Label::new(t!("install.ollama.what")).wrap());
                    ui.add_space(8.0);
                    if let Some(installer) = crate::ollama::install_command() {
                        ui.label(
                            RichText::new(t!("install.runs", command = installer)).monospace(),
                        );
                        ui.add_space(8.0);
                        if ui
                            .add_enabled(
                                !busy,
                                egui::Button::new(t!("install.ollama.button"))
                                    .min_size(egui::vec2(240.0, CONTROL_HEIGHT)),
                            )
                            .clicked()
                        {
                            decision = Some(Some(Job::InstallOllama));
                        }
                    } else {
                        ui.add(egui::Label::new(crate::ollama::manual_install_advice()).wrap());
                        ui.hyperlink_to(
                            t!("install.ollama.download"),
                            "https://ollama.com/download",
                        );
                        if cfg!(target_os = "macos") {
                            ui.add_space(8.0);
                            ui.label(
                                RichText::new(t!(
                                    "install.homebrew.runs",
                                    command = crate::homebrew::INSTALL_COMMAND
                                ))
                                .monospace(),
                            );
                            ui.add_space(8.0);
                            if ui
                                .add_enabled(
                                    !busy,
                                    egui::Button::new(t!("install.homebrew.button"))
                                        .min_size(egui::vec2(240.0, CONTROL_HEIGHT)),
                                )
                                .clicked()
                            {
                                decision = Some(Some(Job::InstallHomebrew));
                            }
                        }
                        ui.add_space(8.0);
                    }
                    if ui
                        .add(
                            egui::Button::new(t!("install.not_now"))
                                .min_size(egui::vec2(240.0, CONTROL_HEIGHT)),
                        )
                        .clicked()
                    {
                        decision = Some(None);
                    }
                }
                Prompt::InstallFfmpeg => {
                    ui.add(egui::Label::new(t!("install.ffmpeg.what")).wrap());
                    ui.add_space(8.0);
                    if let Some(installer) = crate::ffmpeg::install_command() {
                        ui.label(
                            RichText::new(t!("install.runs", command = installer)).monospace(),
                        );
                        ui.add_space(8.0);
                        if ui
                            .add_enabled(
                                !busy,
                                egui::Button::new(t!("install.ffmpeg.button"))
                                    .min_size(egui::vec2(240.0, CONTROL_HEIGHT)),
                            )
                            .clicked()
                        {
                            decision = Some(Some(Job::InstallFfmpeg));
                        }
                    } else {
                        ui.add(egui::Label::new(crate::ffmpeg::manual_install_advice()).wrap());
                        ui.hyperlink_to(t!("install.ffmpeg.download"), crate::ffmpeg::DOWNLOAD_URL);
                        if cfg!(target_os = "macos") {
                            ui.add_space(8.0);
                            ui.label(
                                RichText::new(t!(
                                    "install.homebrew.runs",
                                    command = crate::homebrew::INSTALL_COMMAND
                                ))
                                .monospace(),
                            );
                            ui.add_space(8.0);
                            if ui
                                .add_enabled(
                                    !busy,
                                    egui::Button::new(t!("install.homebrew.button"))
                                        .min_size(egui::vec2(240.0, CONTROL_HEIGHT)),
                                )
                                .clicked()
                            {
                                decision = Some(Some(Job::InstallHomebrew));
                            }
                        }
                        ui.add_space(8.0);
                    }
                    if ui
                        .add(
                            egui::Button::new(t!("install.not_now"))
                                .min_size(egui::vec2(240.0, CONTROL_HEIGHT)),
                        )
                        .clicked()
                    {
                        decision = Some(None);
                    }
                }
                Prompt::PullModel(model) => {
                    ui.add(egui::Label::new(t!("install.model.what", model = model)).wrap());
                    ui.add_space(8.0);
                    if ui
                        .add_enabled(
                            !busy,
                            egui::Button::new(t!("install.model.button", model = model))
                                .min_size(egui::vec2(240.0, CONTROL_HEIGHT)),
                        )
                        .clicked()
                    {
                        decision = Some(Some(Job::PullModel(model.clone())));
                    }
                    if ui
                        .add(
                            egui::Button::new(t!("install.not_now"))
                                .min_size(egui::vec2(240.0, CONTROL_HEIGHT)),
                        )
                        .clicked()
                    {
                        decision = Some(None);
                    }
                }
            }
        });

        let Some(choice) = decision else {
            return;
        };
        self.prompt = None;
        match choice {
            Some(job) => {
                // Once setup finishes, pick the file back up where we left off.
                match (self.file.clone(), self.file_kind) {
                    (Some(path), Some(FileKind::Image)) => {
                        self.deferred = Some(Job::ReadImage {
                            path,
                            config: Box::new(self.config.clone()),
                        });
                    }
                    (Some(path), Some(FileKind::Video)) => {
                        self.deferred = Some(Job::ReadVideo {
                            path,
                            config: Box::new(self.config.clone()),
                        });
                    }
                    _ => {}
                }
                self.start(ctx, job);
            }
            None => {
                let what = if self.file_kind == Some(FileKind::Video) {
                    t!("status.video_skipped")
                } else {
                    t!("status.image_skipped")
                };
                self.deferred = None;
                self.set_status(what, Tone::Info);
            }
        }
    }

    /// Shown once, right after the startup check finds a newer release: its
    /// changelog, with a way to grab it or wave it off for this session.
    fn update_dialog(&mut self, ctx: &egui::Context) {
        let Some(available) = &self.update_available else {
            return;
        };
        let mut decision: Option<bool> = None;

        egui::Modal::new(egui::Id::new("update_available")).show(ctx, |ui| {
            ui.set_max_width(560.0);
            ui.heading(t!("update.heading", version = available.version));
            ui.add_space(6.0);
            egui::ScrollArea::vertical()
                .max_height(280.0)
                .show(ui, |ui| {
                    ui.add(egui::Label::new(&available.notes).wrap());
                });
            ui.add_space(10.0);

            if ui
                .add(
                    egui::Button::new(t!("update.download"))
                        .min_size(egui::vec2(240.0, CONTROL_HEIGHT)),
                )
                .clicked()
            {
                decision = Some(true);
            }
            if ui
                .add(
                    egui::Button::new(t!("update.not_now"))
                        .min_size(egui::vec2(240.0, CONTROL_HEIGHT)),
                )
                .clicked()
            {
                decision = Some(false);
            }
        });

        let Some(download) = decision else {
            return;
        };
        if download {
            ctx.open_url(egui::OpenUrl::same_tab(available.download_url.clone()));
        }
        self.update_available = None;
    }
}

/// True for a file the audio player can open, which is what decides where a
/// dropped file goes.
fn is_playable(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            audio::PLAYABLE_EXTENSIONS
                .iter()
                .any(|known| ext.eq_ignore_ascii_case(known))
        })
}

/// Which platform's voices the system engine offers. Two entries rather than
/// one with a `{platform}` in it: a translator should not have to guess whether
/// their language inflects around the name of an operating system.
fn system_voice_note() -> String {
    if cfg!(target_os = "windows") {
        t!("read.engine.system_note_windows")
    } else {
        t!("read.engine.system_note_macos")
    }
}

impl eframe::App for SpeechApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.drain_updates(&ctx);
        self.handle_dropped_files(&ctx);
        self.handle_shortcuts(&ctx);

        // System speech runs in a separate process, so it would keep talking
        // after the window closed if we didn't stop it here.
        if ctx.input(|i| i.viewport().close_requested()) {
            self.stop_playback();
            self.config_dirty = true;
        }

        // Keep the frame loop alive while something is moving, so progress and
        // playback state stay current without spinning when the app is idle.
        if self.busy.is_some() || self.is_playing() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        // Which is also the clock the tick runs on.
        self.tick_while_busy();

        egui::Panel::top(egui::Id::new("header")).show(ui, |ui| {
            ui.add_space(8.0);
            ui.heading(t!("app.title"));
            ui.add_space(8.0);
        });

        // The panes, listed down the left. Fixed width: a rail that resizes is
        // a rail whose labels move, and these are the app's landmarks.
        egui::Panel::left(egui::Id::new("panes"))
            .resizable(false)
            .exact_size(196.0)
            .show(ui, |ui| self.tab_rail(ui));

        // The status line is shared by every pane, because what the app is
        // doing does not stop being true when you go and edit the dictionary.
        egui::Panel::bottom(egui::Id::new("status")).show(ui, |ui| {
            ui.add_space(8.0);
            self.status_row(ui);
            if self.show_log && !self.log.is_empty() {
                self.log_pane(ui);
            }
            ui.add_space(8.0);
        });

        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| match self.pane {
                    // One column, one width. The form never reflows, so a
                    // magnified view can be parked on the left edge and stay
                    // useful all the way down.
                    Pane::Read => {
                        ui.vertical(|ui| {
                            ui.set_max_width(FORM_WIDTH);
                            self.file_field(ui);
                            self.formatting_field(ui);
                            self.engine_field(ui);
                            self.voice_field(ui);
                            self.action_field(ui);
                            self.apply_button(ui);
                        });
                    }
                    Pane::Player => self.player_pane(ui),
                    Pane::Dictionary => self.dictionary_pane(ui),
                    Pane::Settings => self.settings_pane(ui),
                    Pane::Shortcuts => self.shortcuts_pane(ui),
                });
        });

        self.api_key_dialog_window(&ctx);
        self.reset_dialog(&ctx);
        self.prompt_dialog(&ctx);
        self.update_dialog(&ctx);

        if self.config_dirty {
            self.config_dirty = false;
            self.save_config();
        }

        // A file dragged over the window gets a visible hint.
        if ctx.input(|i| !i.raw.hovered_files.is_empty()) {
            egui::Area::new(egui::Id::new("drop_hint"))
                .order(egui::Order::Foreground)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(&ctx, |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.heading(t!("app.drop_hint"));
                    });
                });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three of them, by the name they are credited under.
    const CUES: [(&str, &[u8]); 3] = [
        ("success", SUCCESS_SOUND),
        ("error", ERROR_SOUND),
        ("progress", PROGRESS_SOUND),
    ];

    /// The cues are decoded at the moment they are played, and a cue that
    /// fails to decode is deliberately swallowed into the log rather than
    /// reported — so a bad file here would be silent in the most literal way,
    /// and nobody would find out from using the app.
    #[test]
    fn every_cue_decodes_to_audible_sound() {
        for (name, bytes) in CUES {
            let decoder = rodio::Decoder::builder()
                .with_data(std::io::Cursor::new(bytes))
                .with_byte_len(bytes.len() as u64)
                .build()
                .unwrap_or_else(|error| panic!("the {name} cue should decode: {error}"));

            let samples: Vec<f32> =
                rodio::Source::take_duration(decoder, std::time::Duration::from_secs(5)).collect();
            let peak = samples.iter().fold(0.0f32, |peak, s| peak.max(s.abs()));

            assert!(!samples.is_empty(), "the {name} cue decoded to nothing");
            assert!(peak > 0.1, "the {name} cue is silent or nearly so: {peak}");
        }
    }

    /// A cue is feedback, so it has to be over before it becomes an
    /// interruption — and it must not still be sounding when the next action
    /// reports its own outcome.
    #[test]
    fn no_cue_outstays_its_welcome() {
        for (name, bytes) in CUES {
            let decoder = rodio::Decoder::builder()
                .with_data(std::io::Cursor::new(bytes))
                .with_byte_len(bytes.len() as u64)
                .build()
                .unwrap();
            let length = rodio::Source::total_duration(&decoder)
                .unwrap_or_else(|| panic!("the {name} cue should report a duration"));
            assert!(
                length < std::time::Duration::from_secs(3),
                "the {name} cue runs for {length:?}"
            );
        }
    }

    /// Running commentary — "reading part 2 of 5" — is set with `Info`, and a
    /// document read in parts would chime through its own narration if that
    /// tone had a sound. Starting a job does make one, but it hangs off
    /// [`SpeechApp::start`] rather than off a tone, for that reason.
    #[test]
    fn only_finishing_and_failing_make_a_sound() {
        assert!(Tone::Info.sound().is_none());
        assert!(Tone::Success.sound().is_some());
        assert!(Tone::Error.sound().is_some());
        assert_ne!(Tone::Success.sound(), Tone::Error.sound());
    }

    /// Cues that a listener has to tell apart the moment they sound, so no two
    /// of them — the tick included — may be the same recording.
    #[test]
    fn no_two_cues_are_the_same_sound() {
        let all: Vec<(&str, &[u8])> = CUES
            .iter()
            .copied()
            .chain(std::iter::once(("tick", TICK_SOUND)))
            .collect();
        for (i, (name, bytes)) in all.iter().enumerate() {
            for (other, other_bytes) in &all[i + 1..] {
                assert_ne!(bytes, other_bytes, "{name} and {other} are the same cue");
            }
        }
    }

    /// The tick is the one sound here that reports nothing, and it may play a
    /// hundred times in a single job. Both of its properties are therefore the
    /// opposite of the other three's, and both are worth holding onto: quiet
    /// enough to sit under the app, and short enough to be over long before the
    /// next one is due.
    #[test]
    fn the_tick_is_quieter_and_shorter_than_the_cues_it_runs_between() {
        let measure = |bytes: &'static [u8]| {
            let decoder = rodio::Decoder::builder()
                .with_data(std::io::Cursor::new(bytes))
                .with_byte_len(bytes.len() as u64)
                .build()
                .expect("the tick should decode");
            let length =
                rodio::Source::total_duration(&decoder).expect("the tick should report a duration");
            let samples: Vec<f32> = decoder.collect();
            let peak = samples.iter().fold(0.0f32, |peak, s| peak.max(s.abs()));
            (length, peak)
        };

        let (length, peak) = measure(TICK_SOUND);
        assert!(peak > 0.01, "the tick is inaudible: {peak}");
        assert!(
            length < Duration::from_millis(500),
            "the tick runs {length:?}"
        );
        // Well inside the gap between two of them, or they would overlap.
        assert!(length * 10 < TICK_EVERY);

        for (name, cue) in CUES {
            let (cue_length, cue_peak) = measure(cue);
            assert!(
                peak < cue_peak / 2.0,
                "the tick ({peak}) is not clearly quieter than the {name} cue ({cue_peak})"
            );
            assert!(
                length < cue_length,
                "the tick ({length:?}) is not shorter than the {name} cue ({cue_length:?})"
            );
        }
    }

    /// Two sounds inside the gap collide, so the second is dropped.
    #[test]
    fn a_sound_landing_on_top_of_another_is_dropped() {
        assert!(cue_is_clear(None), "the first sound of all must be heard");
        assert!(!cue_is_clear(Some(Duration::ZERO)));
        assert!(!cue_is_clear(Some(CUE_GAP / 2)));
        assert!(cue_is_clear(Some(CUE_GAP)));
        assert!(cue_is_clear(Some(CUE_GAP * 2)));
    }

    /// The tick yields to the cues and never the other way round.
    ///
    /// Ticking every fifteen seconds puts a tick within a quarter-second of the
    /// success chime about one job in sixty. If this inverted, that job would
    /// lose the sound that says "finished" — replaced by one that says nothing
    /// — and it would happen rarely enough to look like imagination.
    #[test]
    fn the_tick_never_makes_a_cue_wait() {
        let earlier = Instant::now();
        let later = earlier + Duration::from_secs(1);

        // An announcement takes the floor, so the next sound waits for it.
        assert_eq!(
            floor_after(Some(earlier), later, ClaimsTheGap::Yes),
            Some(later)
        );
        assert_eq!(floor_after(None, later, ClaimsTheGap::Yes), Some(later));

        // A tick leaves it exactly as it found it, whether or not one was held.
        assert_eq!(
            floor_after(Some(earlier), later, ClaimsTheGap::No),
            Some(earlier),
            "a tick pushed the gap forward and would swallow the next cue"
        );
        assert_eq!(floor_after(None, later, ClaimsTheGap::No), None);
    }

    /// The interval is the difference between reassurance and nagging, and the
    /// tick has to be over before the next one starts however it is edited.
    #[test]
    fn the_tick_interval_is_a_sane_one() {
        assert!(TICK_EVERY >= Duration::from_secs(5), "too often to bear");
        assert!(
            TICK_EVERY <= Duration::from_secs(60),
            "long enough that a working job sounds like a stuck one"
        );
    }
}
