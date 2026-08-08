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

use crate::audio::{AudioFormat, Playback};
use crate::config::{Action, Config, DEFAULT_VISION_PROMPT, EnginePreference};
use crate::extract::{DOC_EXTENSIONS, FileKind, IMAGE_EXTENSIONS, TEXT_EXTENSIONS};
use crate::jobs::{self, Cancel, Job, Update};
use crate::keychain::{self, KeySource};
use crate::theme::{CONTROL_HEIGHT, FORM_WIDTH};
use crate::tts::{self, Voice};
use egui::{Key, Modifiers, RichText};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

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
    fn prefix(self) -> &'static str {
        match self {
            Self::Info => "",
            Self::Success => "Done: ",
            Self::Error => "Problem: ",
        }
    }
}

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
    PullModel(String),
}

/// The panes listed down the left of the window. The form is one of them, so
/// everything the app can do is a place you can go to rather than a dialog that
/// appears over what you were doing — which also means every one of them has a
/// stable position in the Tab order.
#[derive(PartialEq, Eq, Clone, Copy)]
enum Pane {
    Read,
    Dictionary,
    Settings,
    Shortcuts,
}

impl Pane {
    const ALL: [Self; 4] = [
        Self::Read,
        Self::Dictionary,
        Self::Settings,
        Self::Shortcuts,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Read => "📄  Read a File",
            Self::Dictionary => "📖  Dictionary",
            Self::Settings => "⚙  Settings",
            Self::Shortcuts => "？  Shortcuts",
        }
    }

    fn hint(self) -> &'static str {
        match self {
            Self::Read => "Choose a file, a voice, and what to do with it",
            Self::Dictionary => "Words to swap before the document is spoken",
            Self::Settings => "The ElevenLabs model, and how images are read",
            Self::Shortcuts => "Every keyboard shortcut in the app",
        }
    }

    /// The shortcut that opens it, in [`shortcut_text`] notation.
    fn shortcut(self) -> &'static str {
        match self {
            Self::Read => "{C}1",
            Self::Dictionary => "{C}2",
            Self::Settings => "{C}3",
            Self::Shortcuts => "{C}4",
        }
    }
}

/// A control the keyboard can be sent to directly.
#[derive(PartialEq, Eq, Clone, Copy)]
enum Field {
    File,
    Engine,
    Voice,
    Action,
    Apply,
}

/// Voice list loading state, kept per engine.
#[derive(Default)]
struct VoiceList {
    voices: Vec<Voice>,
    error: Option<String>,
    loaded: bool,
}

/// Every keyboard shortcut, in the order the help dialog lists them. Defined
/// once so the dialog, the tooltips and the README cannot drift apart.
pub const SHORTCUTS: &[(&str, &str)] = &[
    ("{C}O", "Choose a file"),
    ("{C}Return", "Apply — run the chosen action"),
    ("{C}. or Esc", "Stop reading, or cancel what is running"),
    (
        "{C}1 … {C}4",
        "Go to Read, Dictionary, Settings or Shortcuts",
    ),
    ("↑ ↓", "Move along the list of panes, once it has focus"),
    ("Tab / Shift+Tab", "Move between controls"),
    ("Space or Return", "Operate the focused control"),
    ("←  →", "Change the value in an open dropdown or a slider"),
    ("{C}K", "ElevenLabs API key"),
    ("{C}L", "Show or hide the activity log"),
    ("F1", "The Shortcuts pane"),
];

/// Renders `{C}` as the modifier this platform actually uses.
fn shortcut_text(raw: &str) -> String {
    raw.replace(
        "{C}",
        if cfg!(target_os = "macos") {
            "⌘"
        } else {
            "Ctrl+"
        },
    )
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
    playback: Option<Playback>,
    cached: Option<CachedRender>,

    status: Option<(String, Tone)>,
    log: Vec<String>,
    show_log: bool,

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
    /// A control to hand the keyboard to on the next frame.
    focus: Option<Field>,
    /// A pane whose tab should take the keyboard on the next frame, after the
    /// arrow keys moved along the rail.
    focus_tab: Option<Pane>,
    /// Set by any control that changes `config`; written out at end of frame
    /// so a slider drag doesn't rewrite the file on every pixel.
    config_dirty: bool,

    tx: Sender<Update>,
    rx: Receiver<Update>,
}

impl SpeechApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        crate::theme::apply(&cc.egui_ctx);

        let (tx, rx) = channel();
        let (api_key, key_source) = keychain::load();
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
            cached: None,
            status: None,
            log: Vec::new(),
            show_log: false,
            prompt: None,
            deferred: None,
            pane: Pane::Read,
            show_api_key: false,
            dialog_opened: false,
            key_input: String::new(),
            // The keyboard starts on the first control, so a user who never
            // touches the mouse does not have to Tab in from nowhere.
            focus: Some(Field::File),
            focus_tab: None,
            config_dirty: false,
            tx,
            rx,
        };
        app.load_system_voices();

        // A path on the command line opens straight away, which is what
        // "Open With" produces on both platforms.
        if let Some(path) = std::env::args_os().nth(1).map(PathBuf::from) {
            let ctx = cc.egui_ctx.clone();
            app.open_file(&ctx, path);
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
    }

    fn save_config(&mut self) {
        if let Err(error) = self.config.save() {
            self.set_status(format!("Could not save settings: {error:#}"), Tone::Error);
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

    fn choose_file(&mut self, ctx: &egui::Context) {
        if self.busy.is_some() {
            return;
        }
        if let Some(path) = rfd::FileDialog::new()
            .set_title("Choose a document or image to read")
            .add_filter(
                "All supported files",
                &[TEXT_EXTENSIONS, DOC_EXTENSIONS, IMAGE_EXTENSIONS].concat(),
            )
            .add_filter("Text files", TEXT_EXTENSIONS)
            .add_filter("Word documents", DOC_EXTENSIONS)
            .add_filter("Images", IMAGE_EXTENSIONS)
            .pick_file()
        {
            self.open_file(ctx, path);
        }
    }

    fn open_file(&mut self, ctx: &egui::Context, path: PathBuf) {
        let Some(kind) = FileKind::from_path(&path) else {
            self.set_status(
                format!(
                    "{} is not a file type this app can read",
                    path.file_name().unwrap_or_default().to_string_lossy()
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
            _ => Job::ReadDocument(path),
        };
        self.start(ctx, job);
    }

    /// The Apply button: run whichever action the dropdown is showing.
    fn apply(&mut self, ctx: &egui::Context) {
        if self.busy.is_some() {
            return;
        }
        if self.file.is_none() {
            self.set_status("Choose a file first.", Tone::Error);
            self.focus = Some(Field::File);
            return;
        }
        if self.text.trim().is_empty() {
            self.set_status(
                "There is no text to read yet — the file is still being read, or it was empty.",
                Tone::Error,
            );
            return;
        }
        match self.active_engine() {
            ActiveEngine::MissingKey => {
                self.set_status(
                    "Add an ElevenLabs API key, or choose System voices.",
                    Tone::Error,
                );
                self.open_api_key_dialog();
                return;
            }
            ActiveEngine::Unsupported => {
                self.set_status(tts::system::UNSUPPORTED_MESSAGE, Tone::Error);
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
            0 => "Reading aloud…".to_string(),
            1 => "Reading aloud… 1 word replaced by the dictionary.".to_string(),
            n => format!("Reading aloud… {n} words replaced by the dictionary."),
        };

        match engine {
            // The system engines speak straight to the output device, so there
            // is nothing to synthesise and nothing to wait for.
            jobs::Engine::System { voice, rate } => match tts::system::speak(&text, &voice, rate) {
                Ok(child) => {
                    self.playback = Some(Playback::Process(child));
                    self.set_status(reading, Tone::Info);
                }
                Err(error) => self.set_status(format!("{error:#}"), Tone::Error),
            },
            jobs::Engine::ElevenLabs { .. } => {
                if self.config.elevenlabs_voice_id.is_empty() {
                    self.set_status("Choose an ElevenLabs voice first.", Tone::Error);
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
                self.set_status("Reading aloud…", Tone::Info);
            }
            Err(error) => self.set_status(format!("{error:#}"), Tone::Error),
        }
    }

    fn stop_playback(&mut self) {
        if let Some(mut playback) = self.playback.take() {
            playback.stop();
        }
    }

    /// Escape and ⌘. mean "whatever is happening, stop".
    fn stop_everything(&mut self) {
        if self.is_playing() {
            self.stop_playback();
            self.set_status("Stopped reading.", Tone::Info);
            return;
        }
        if let Some(busy) = &self.busy
            && busy.cancellable
        {
            busy.cancel.store(true, Ordering::Relaxed);
            self.set_status("Cancelling…", Tone::Info);
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
                .unwrap_or_else(|| "speech".to_string()),
            format.extension()
        );

        let mut dialog = rfd::FileDialog::new()
            .set_title("Save the audio file as")
            .add_filter(format.label(), &[format.extension()])
            .set_file_name(&suggested);
        if let Some(dir) = self.config.last_save_dir.as_ref().filter(|d| d.is_dir()) {
            dialog = dialog.set_directory(dir);
        }
        let Some(mut path) = dialog.save_file() else {
            self.set_status("Nothing saved.", Tone::Info);
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

    // -------------------------------------------------------------- updates

    fn drain_updates(&mut self, ctx: &egui::Context) {
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
                    self.show_log = true;
                    self.log.push(line);
                    // Keep the pane bounded; a model pull emits a lot of lines.
                    if self.log.len() > 500 {
                        self.log.drain(..self.log.len() - 500);
                    }
                }
                Update::TextReady { text, note } => {
                    self.text = text;
                    self.text_note = note;
                    self.cached = None;
                    self.set_status("Ready. Press Apply to read it.", Tone::Success);
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
                }
                Update::Mp3Ready(mp3) => {
                    self.cached = Some(CachedRender {
                        fingerprint: self.fingerprint(),
                        mp3: Arc::clone(&mp3),
                    });
                    self.begin_playback(mp3);
                }
                Update::Saved(path) => {
                    self.set_status(format!("Saved to {}", path.display()), Tone::Success);
                }
                Update::NeedsOllamaInstall => {
                    self.prompt = Some(Prompt::InstallOllama);
                }
                Update::NeedsModel(model) => {
                    self.prompt = Some(Prompt::PullModel(model));
                }
                // Whatever was waiting on Ollama is in `deferred` and starts as
                // soon as the setup job sends `Finished`.
                Update::SetupComplete => self.set_status("Ollama is ready.", Tone::Success),
                Update::ElevenLabsVoicesFailed(message) => {
                    self.elevenlabs_voices = VoiceList {
                        voices: Vec::new(),
                        error: Some(message),
                        loaded: true,
                    };
                }
                Update::Error(message) => {
                    self.set_status(message, Tone::Error);
                    // Don't retry the step that was waiting on a failed one; it
                    // would only re-raise the same prompt.
                    self.deferred = None;
                }
                Update::Finished => {
                    self.busy = None;
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
            self.playback = None;
            if matches!(self.status, Some((_, Tone::Info))) {
                self.set_status("Finished reading.", Tone::Success);
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
        if let Some(path) = dropped.into_iter().next() {
            self.open_file(ctx, path);
        }
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
        let mut go = None;
        ctx.input_mut(|i| {
            open = i.consume_key(Modifiers::COMMAND, Key::O);
            apply = i.consume_key(Modifiers::COMMAND, Key::Enter);
            stop = i.consume_key(Modifiers::COMMAND, Key::Period)
                || (something_to_stop && i.consume_key(Modifiers::NONE, Key::Escape));
            key = i.consume_key(Modifiers::COMMAND, Key::K);
            log = i.consume_key(Modifiers::COMMAND, Key::L);

            // One number per pane, in the order they are listed, plus the two
            // conventional ways in: F1 for help and ⌘, for settings.
            for (number, pane) in [Key::Num1, Key::Num2, Key::Num3, Key::Num4]
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
            self.choose_file(ctx);
        }
        if apply {
            self.apply(ctx);
        }
        if stop {
            self.stop_everything();
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
        let caption = Self::caption(ui, "Document or image");

        let button = ui.add_enabled(
            self.busy.is_none(),
            egui::Button::new("📂  Choose File…").min_size(egui::vec2(FORM_WIDTH, CONTROL_HEIGHT)),
        );
        let button = button.on_hover_text(format!(
            "{}  ·  Text, Word documents and images. You can also drop a file on this window.",
            shortcut_text("{C}O")
        ));
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
                    format!("Chosen: {name}")
                } else {
                    format!("Chosen: {name} — {}", self.text_note)
                }
            }
            None => "No file chosen yet.".to_string(),
        };
        let line = ui.add(egui::Label::new(RichText::new(text)).wrap());
        if let Some(path) = &self.file {
            line.on_hover_text(path.display().to_string());
        }
    }

    fn engine_field(&mut self, ui: &mut egui::Ui) {
        let caption = Self::caption(ui, "Speech engine");
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
        let combo = combo.on_hover_text(format!(
            "{}  ·  {}",
            shortcut_text("{C}2"),
            self.config.engine.description()
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
                    KeySource::Env => "API key from the environment",
                    _ => "API key stored securely on this computer",
                };
                let muted = crate::theme::palette(ui.visuals()).muted;
                ui.label(RichText::new(source).color(muted));
            }
            ActiveEngine::System => {
                let muted = crate::theme::palette(ui.visuals()).muted;
                ui.label(RichText::new(SYSTEM_VOICE_NOTE).color(muted));
            }
            ActiveEngine::MissingKey => {
                let colour = crate::theme::palette(ui.visuals()).warn;
                ui.colored_label(colour, "No API key set — ElevenLabs cannot be used yet.");
                if ui
                    .add(
                        egui::Button::new("Enter API Key…")
                            .min_size(egui::vec2(FORM_WIDTH, CONTROL_HEIGHT)),
                    )
                    .on_hover_text(shortcut_text("{C}K"))
                    .clicked()
                {
                    self.open_api_key_dialog();
                }
            }
            ActiveEngine::Unsupported => {
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    tts::system::UNSUPPORTED_MESSAGE,
                );
            }
        }
    }

    fn voice_field(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let caption = Self::caption(ui, "Voice");

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
                let combo = combo.on_hover_text(format!(
                    "{}  ·  The ElevenLabs voice the document is read in",
                    shortcut_text("{C}3")
                ));
                let _ = caption.labelled_by(combo.id);
                self.take_focus(Field::Voice, &combo);

                if let Some(error) = self.elevenlabs_voices.error.clone() {
                    ui.colored_label(ui.visuals().error_fg_color, format!("Voices: {error}"));
                    if ui
                        .add(
                            egui::Button::new("Try Loading Voices Again")
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
                    .unwrap_or_else(|| DEFAULT_VOICE_LABEL.to_string());

                let combo = egui::ComboBox::from_id_salt("system_voice")
                    .width(FORM_WIDTH)
                    .selected_text(selected)
                    .show_ui(ui, |ui| {
                        // Keeps a way back to the computer's own chosen voice
                        // after picking a specific one.
                        if ui
                            .selectable_label(
                                self.config.system_voice.is_empty(),
                                DEFAULT_VOICE_LABEL,
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
                let combo = combo.on_hover_text(format!(
                    "{}  ·  The installed voice the document is read in",
                    shortcut_text("{C}3")
                ));
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
                    &format!(
                        "Speaking speed — {} words per minute",
                        self.config.system_rate
                    ),
                );
                ui.spacing_mut().slider_width = FORM_WIDTH;
                let slider = ui.add_sized(
                    [FORM_WIDTH, CONTROL_HEIGHT],
                    egui::Slider::new(&mut self.config.system_rate, 90..=350)
                        .show_value(false)
                        .clamping(egui::SliderClamping::Always),
                );
                let slider = slider.on_hover_text(
                    "Left and right arrows adjust this by one word per minute when it has focus.",
                );
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
            return "Add an API key to load voices".to_string();
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
                    "Voices unavailable".to_string()
                } else {
                    "Loading voices…".to_string()
                }
            })
    }

    fn action_field(&mut self, ui: &mut egui::Ui) {
        let caption = Self::caption(ui, "What to do");
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
        let combo = combo.on_hover_text(format!(
            "{}  ·  {}",
            shortcut_text("{C}4"),
            self.config.action.description()
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
            let caption = Self::caption(ui, "Audio format");
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
        // control in one place, whatever state the app is in.
        if self.is_playing() {
            let stop = ui.add(
                egui::Button::new(RichText::new("⏹  Stop Reading").size(17.0))
                    .min_size(egui::vec2(FORM_WIDTH, CONTROL_HEIGHT + 6.0)),
            );
            let stop = stop.on_hover_text(shortcut_text("{C}. or Esc"));
            self.take_focus(Field::Apply, &stop);
            if stop.clicked() {
                self.stop_playback();
                self.set_status("Stopped reading.", Tone::Info);
            }
            return;
        }

        let ready = self.busy.is_none() && !self.text.trim().is_empty();
        let button = ui.add_enabled(
            ready,
            egui::Button::new(RichText::new("Apply").size(17.0))
                .min_size(egui::vec2(FORM_WIDTH, CONTROL_HEIGHT + 6.0)),
        );
        let button = button.on_hover_text(format!(
            "{}  ·  {}",
            shortcut_text("{C}Return"),
            self.config.action.description()
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
                ui.add(
                    egui::ProgressBar::new(progress)
                        .desired_width(FORM_WIDTH)
                        .desired_height(10.0)
                        .show_percentage(),
                );
            }
            if cancellable
                && ui
                    .add(
                        egui::Button::new("Cancel")
                            .min_size(egui::vec2(FORM_WIDTH, CONTROL_HEIGHT)),
                    )
                    .on_hover_text(shortcut_text("{C}. or Esc"))
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
                ui.add(egui::Label::new(RichText::new(READY_HINT).color(muted)).wrap());
            }
        }
    }

    fn log_pane(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        ui.horizontal(|ui| {
            ui.label(RichText::new("Activity log"));
            if ui
                .button("Hide")
                .on_hover_text(shortcut_text("{C}L"))
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
            let response = response.on_hover_text(format!(
                "{}  ·  {}",
                shortcut_text(pane.shortcut()),
                pane.hint()
            ));
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
        self.config_dirty = true;
    }

    fn api_key_dialog(&mut self, ui: &mut egui::Ui) {
        ui.heading("ElevenLabs API Key");
        ui.add_space(6.0);

        match self.key_source {
            KeySource::Env => {
                ui.label(format!(
                    "The key in the {} environment variable is being used. \
                     Unset it if you would rather manage the key here.",
                    keychain::ENV_VAR
                ));
            }
            KeySource::Keychain => {
                ui.label("A key is saved securely on this computer, so ElevenLabs voices are ready to use.");
                ui.add_space(8.0);
                if ui
                    .add(
                        egui::Button::new("Remove Saved Key")
                            .min_size(egui::vec2(240.0, CONTROL_HEIGHT)),
                    )
                    .clicked()
                {
                    match keychain::clear() {
                        Ok(()) => {
                            self.api_key = None;
                            self.key_source = KeySource::None;
                            self.elevenlabs_voices = VoiceList::default();
                            self.set_status("API key removed.", Tone::Success);
                        }
                        Err(error) => self.set_status(format!("{error:#}"), Tone::Error),
                    }
                }
            }
            KeySource::None => {
                ui.add(
                    egui::Label::new(format!(
                        "Paste the key from your ElevenLabs account. It is stored {}, \
                         never in a settings file. Without a key, choose System voices instead — \
                         they need no account and work offline.",
                        keychain::STORAGE_DESCRIPTION
                    ))
                    .wrap(),
                );
                ui.add_space(8.0);

                let caption = ui.label("API key");
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
                    egui::Button::new("Save Key").min_size(egui::vec2(200.0, CONTROL_HEIGHT)),
                );
                if save.clicked() || (submitted && typed) {
                    let key = self.key_input.trim().to_string();
                    match keychain::store(&key) {
                        Ok(()) => {
                            self.api_key = Some(key);
                            self.key_source = KeySource::Keychain;
                            self.elevenlabs_voices = VoiceList::default();
                            self.set_status("API key saved.", Tone::Success);
                            self.close_dialog();
                            self.focus = Some(Field::Voice);
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
            .add(egui::Button::new("Close").min_size(egui::vec2(160.0, CONTROL_HEIGHT)))
            .on_hover_text("Esc")
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

        ui.heading("Dictionary");
        ui.add_space(6.0);
        ui.add(
            egui::Label::new(
                "Words listed here are swapped before the document is spoken — to fix how a \
                 name is pronounced, or to replace a word with a gentler one. Matching ignores \
                 capitals, and a word that started a sentence still starts one afterwards. \
                 The file on disk is never changed.",
            )
            .wrap(),
        );
        ui.add_space(10.0);

        let mut remove = None;
        let mut changed = false;

        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = GAP;
            // Left-aligned in a fixed-width cell, so each heading starts exactly
            // where the field below it starts.
            for (width, text) in [
                (CELL, "Say this word"),
                (CELL, "as this"),
                (WHOLE, "Whole word"),
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
                                .hint_text("word in the document"),
                        );
                        let to = ui.add_sized(
                            [CELL, CONTROL_HEIGHT],
                            egui::TextEdit::singleline(&mut rule.to)
                                .hint_text("what to say instead"),
                        );
                        let whole = ui.add_sized(
                            [WHOLE, CONTROL_HEIGHT],
                            egui::Checkbox::without_text(&mut rule.whole_word),
                        );
                        let whole = whole.on_hover_text(
                            "On: only complete words match. Off: the letters match \
                             anywhere, including inside longer words.",
                        );
                        // Named for a screen reader, which would otherwise hear
                        // a column of identical "Remove" buttons.
                        let named = if rule.from.trim().is_empty() {
                            format!("Remove row {}", index + 1)
                        } else {
                            format!("Remove “{}”", rule.from.trim())
                        };
                        if ui
                            .add_sized([REMOVE, CONTROL_HEIGHT], egui::Button::new("Remove"))
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
            ui.label(RichText::new("No replacements yet.").color(muted));
        }

        ui.add_space(10.0);
        if ui
            .add(egui::Button::new("Add Replacement").min_size(egui::vec2(240.0, CONTROL_HEIGHT)))
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

    fn shortcuts_pane(&mut self, ui: &mut egui::Ui) {
        ui.heading("Keyboard Shortcuts");
        ui.add_space(6.0);
        ui.add(
            egui::Label::new(
                "Every part of this app can be operated without a mouse. Tab moves forward \
                 through the controls, Shift+Tab moves back, and the control with focus is \
                 drawn with a heavy outline.",
            )
            .wrap(),
        );
        ui.add_space(12.0);

        egui::Grid::new("shortcuts")
            .num_columns(2)
            .spacing([24.0, 10.0])
            .striped(true)
            .show(ui, |ui| {
                for (keys, what) in SHORTCUTS {
                    // Not monospaced: in a monospace face ⌘O reads as ⌘0.
                    ui.label(shortcut_text(keys));
                    ui.label(*what);
                    ui.end_row();
                }
            });
    }

    fn settings_pane(&mut self, ui: &mut egui::Ui) {
        ui.heading("Settings");
        ui.add_space(6.0);

        let caption = ui.label("ElevenLabs model");
        let model = ui.add(
            egui::TextEdit::singleline(&mut self.config.elevenlabs_model_id)
                .desired_width(FORM_WIDTH),
        );
        let _ = caption.labelled_by(model.id);
        let mut edited = model.changed();
        model.on_hover_text("For example: eleven_multilingual_v2, eleven_turbo_v2_5");

        ui.add_space(12.0);
        ui.separator();
        ui.add(
            egui::Label::new(
                "Images are read by a vision model running on this computer through Ollama. \
                 Nothing is uploaded anywhere.",
            )
            .wrap(),
        );
        ui.add_space(6.0);

        let caption = ui.label("Vision model");
        let vision = ui.add(
            egui::TextEdit::singleline(&mut self.config.ollama_model).desired_width(FORM_WIDTH),
        );
        let _ = caption.labelled_by(vision.id);
        edited |= vision.changed();

        ui.add_space(6.0);
        let caption = ui.label("Prompt sent with the image");
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
        if ui.button("Reset Prompt To Default").clicked() {
            self.config.ollama_prompt = DEFAULT_VISION_PROMPT.to_string();
            self.config_dirty = true;
        }

        ui.add_space(12.0);
        ui.separator();
        if let Some(path) = Config::path() {
            let muted = crate::theme::palette(ui.visuals()).muted;
            ui.add(
                egui::Label::new(
                    RichText::new(format!("Settings file: {}", path.display())).color(muted),
                )
                .wrap(),
            );
        }
    }

    fn prompt_dialog(&mut self, ctx: &egui::Context) {
        let Some(prompt) = &self.prompt else {
            return;
        };
        let busy = self.busy.is_some();
        let mut decision: Option<Option<Job>> = None;

        egui::Modal::new(egui::Id::new("ollama_prompt")).show(ctx, |ui| {
            ui.set_max_width(560.0);
            ui.heading("Reading Images");
            ui.add_space(6.0);
            match prompt {
                Prompt::InstallOllama => {
                    ui.add(
                        egui::Label::new(
                            "Reading text out of an image needs Ollama, which runs a vision \
                         model on this computer. It is not installed yet.",
                        )
                        .wrap(),
                    );
                    ui.add_space(8.0);
                    if let Some(installer) = crate::ollama::install_command() {
                        ui.label(RichText::new(format!("This runs: {installer}")).monospace());
                        ui.add_space(8.0);
                        if ui
                            .add_enabled(
                                !busy,
                                egui::Button::new("Install Ollama")
                                    .min_size(egui::vec2(240.0, CONTROL_HEIGHT)),
                            )
                            .clicked()
                        {
                            decision = Some(Some(Job::InstallOllama));
                        }
                    } else {
                        ui.add(egui::Label::new(crate::ollama::MANUAL_INSTALL_ADVICE).wrap());
                        ui.hyperlink_to("Download Ollama", "https://ollama.com/download");
                        if cfg!(target_os = "macos") {
                            ui.hyperlink_to("Install Homebrew", "https://brew.sh");
                        }
                        ui.add_space(8.0);
                    }
                    if ui
                        .add(
                            egui::Button::new("Not Now")
                                .min_size(egui::vec2(240.0, CONTROL_HEIGHT)),
                        )
                        .clicked()
                    {
                        decision = Some(None);
                    }
                }
                Prompt::PullModel(model) => {
                    ui.add(
                        egui::Label::new(format!(
                            "Ollama is installed, but the vision model “{model}” has not been \
                         downloaded yet. It is typically several gigabytes and only needs \
                         downloading once."
                        ))
                        .wrap(),
                    );
                    ui.add_space(8.0);
                    if ui
                        .add_enabled(
                            !busy,
                            egui::Button::new(format!("Download {model}"))
                                .min_size(egui::vec2(240.0, CONTROL_HEIGHT)),
                        )
                        .clicked()
                    {
                        decision = Some(Some(Job::PullModel(model.clone())));
                    }
                    if ui
                        .add(
                            egui::Button::new("Not Now")
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
                // Once setup finishes, pick the image back up where we left off.
                if let (Some(path), Some(FileKind::Image)) = (self.file.clone(), self.file_kind) {
                    self.deferred = Some(Job::ReadImage {
                        path,
                        config: Box::new(self.config.clone()),
                    });
                }
                self.start(ctx, job);
            }
            None => {
                self.deferred = None;
                self.set_status("Image not read.", Tone::Info);
            }
        }
    }
}

const READY_HINT: &str = "Choose a .txt, .docx or image file, then press Apply. \
                          Press F1 for keyboard shortcuts.";

const DEFAULT_VOICE_LABEL: &str = "This computer's default voice";

#[cfg(target_os = "windows")]
const SYSTEM_VOICE_NOTE: &str = "The voices built into Windows. No account needed.";
#[cfg(not(target_os = "windows"))]
const SYSTEM_VOICE_NOTE: &str = "The voices built into macOS. No account needed.";

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

        egui::Panel::top(egui::Id::new("header")).show(ui, |ui| {
            ui.add_space(8.0);
            ui.heading("Speech Output Engine");
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
                            self.engine_field(ui);
                            self.voice_field(ui);
                            self.action_field(ui);
                            self.apply_button(ui);
                        });
                    }
                    Pane::Dictionary => self.dictionary_pane(ui),
                    Pane::Settings => self.settings_pane(ui),
                    Pane::Shortcuts => self.shortcuts_pane(ui),
                });
        });

        self.api_key_dialog_window(&ctx);
        self.prompt_dialog(&ctx);

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
                        ui.heading("Drop to open");
                    });
                });
        }
    }
}
