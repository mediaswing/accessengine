//! The user interface and the playback state machine that sits behind it.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use egui::{Align, Color32, RichText};

use crate::audio::Sounds;
use crate::config::{self, Config};
use crate::document::{ChunkMode, Document, SUPPORTED_EXTENSIONS};
use crate::export::{self, Export};
use crate::logging;
use crate::speech::elevenlabs::{self, Command as ElCommand, ElevenLabs, RemoteVoice, VoiceRequest};
use crate::speech::system::SystemEngine;
use crate::speech::{EngineKind, PlanItem, PlayState};
use crate::theme;
use crate::update::{UpdateChecker, UpdateInfo};
use crate::vision::{self, ModelInfo, Vision, VisionResult, IMAGE_EXTENSIONS};
use crate::wordlist::{self, BlockPolicy, Hit, WordlistSet};
use crate::{i18n, t, tn, APP_NAME};

/// Settings are written this long after the last change, so dragging a slider
/// does not mean a disk write per frame.
const SETTINGS_SAVE_DELAY: Duration = Duration::from_millis(1500);
/// An export expected to take at least this long is confirmed first. Below it,
/// the save dialog is confirmation enough — nobody needs a second question
/// about something that will be over before they look away.
const CONFIRM_EXPORT_ABOVE: Duration = Duration::from_secs(30);
/// Beyond this many chunks the document view renders a window around the
/// cursor instead of the whole text, to keep layout under a frame budget.
const VIRTUALISE_ABOVE: usize = 4000;
const VIRTUAL_WINDOW: usize = 200;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    General,
    Wordlists,
    Settings,
}

impl Tab {
    const ALL: [Self; 3] = [Self::General, Self::Wordlists, Self::Settings];

    fn title(self) -> String {
        match self {
            Self::General => t!("tab.general"),
            Self::Wordlists => t!("tab.wordlists"),
            Self::Settings => t!("tab.settings"),
        }
    }
}

/// What the preview pane is showing: the document being read, or the picture
/// being described — whichever was opened last.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Preview {
    Document,
    Image,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StatusKind {
    Info,
    Error,
}

struct Status {
    text: String,
    kind: StatusKind,
}

pub struct AccessEngine {
    cfg: Config,
    doc: Document,

    /// What will actually be spoken, after the wordlists have had their say.
    plan: Vec<PlanItem>,
    /// Chunk indices grouped into paragraphs, for laying the document out.
    paragraphs: Vec<Vec<usize>>,
    /// Every change the wordlists made to the current document.
    hits: Vec<Hit>,
    /// Chunks dropped entirely by a `skip sentence` rule.
    skipped_chunks: usize,

    plan_pos: usize,
    state: PlayState,
    /// Set when the plan index changes, to scroll the view to it once.
    follow_cursor: bool,

    /// The system engine, or the reason there isn't one.
    system: Result<SystemEngine, String>,
    eleven: ElevenLabs,
    el_voices: Vec<RemoteVoice>,
    el_synthesising: Option<usize>,
    /// Whether the worker's paused plan still matches ours. Cleared whenever
    /// the plan is rebuilt, so a wordlist change while paused restarts with the
    /// new text instead of resuming the old.
    el_can_resume: bool,

    wordlists: WordlistSet,
    voice_filter: String,

    /// A document being written to an MP3 file, if one is.
    export: Export,
    /// How long a sentence takes, learned from the sentences already sent.
    export_estimate: export::Estimate,
    /// Set while the confirmation for a long export is on screen.
    confirm_export: bool,
    /// An image is waiting to be described as soon as there is a model to do
    /// it with. Set when one is opened; the description then starts by itself.
    pending_describe: bool,
    /// Whether the finished description is on screen.
    show_description: bool,
    /// When the current ElevenLabs sentence was sent, for that estimate.
    synthesis_began: Option<Instant>,

    vision: Vision,
    models: Vec<ModelInfo>,
    /// Whether a model list has been asked for yet this session. Ollama is
    /// asked once, when a description is first wanted, rather than on every
    /// frame: it may not be running, and that answer does not change quickly.
    models_requested: bool,
    image_path: Option<PathBuf>,
    description: String,

    sounds: Sounds,
    tab: Tab,
    /// What the preview pane is showing.
    preview: Preview,
    /// The Settings form's working copy. Nothing typed there reaches the
    /// running app until Apply, so a half-typed Ollama address or a theme
    /// being tried on for size cannot take effect by itself.
    draft: Config,
    /// The log folder as typed. Kept as text rather than read back out of the
    /// draft each frame, so deleting the last character of a path does not
    /// refill the box with the default one.
    log_dir_field: String,
    /// Set when ElevenLabs turns down the key we hold, so the button to enter
    /// one comes back for a key that has expired or been revoked.
    api_key_rejected: bool,
    show_key_dialog: bool,
    /// The key dialog's own copy, so cancelling leaves the saved key alone.
    key_draft: String,
    status: Option<Status>,
    settings_dirty: Option<Instant>,

    /// Present while the background check is in flight; taken once it resolves.
    update_checker: Option<UpdateChecker>,
    /// Set once a newer release is found; drives the update dialog.
    update_info: Option<UpdateInfo>,
    show_update_dialog: bool,
}

impl AccessEngine {
    pub fn new(cc: &eframe::CreationContext<'_>, initial_file: Option<PathBuf>) -> Self {
        egui_extras::install_image_loaders(&cc.egui_ctx);
        theme::apply(&cc.egui_ctx);

        let cfg = Config::load();
        // Before any of the interface is built, so the first frame is already
        // in the right language rather than in English for one repaint.
        i18n::apply_setting(&cfg.language);
        theme::apply_appearance(&cc.egui_ctx, cfg.appearance);
        cc.egui_ctx.set_zoom_factor(cfg.text_scale);

        // The repaint hooks let worker threads and speech callbacks wake the UI
        // instead of the UI polling them at a fixed rate.
        let ctx = cc.egui_ctx.clone();
        let repaint = move || ctx.request_repaint();
        let ctx2 = cc.egui_ctx.clone();

        let system = SystemEngine::new(repaint.clone()).map_err(|e| {
            log::error!("system speech unavailable: {e:#}");
            format!("{e:#}")
        });

        let update_checker = cfg
            .check_for_updates
            .then(|| UpdateChecker::start(repaint.clone()));

        let mut wordlists = WordlistSet {
            policy: cfg.block_policy,
            bleep_text: cfg.bleep_text.clone(),
            lists: Vec::new(),
        };
        if let Some(dir) = config::wordlist_dir() {
            if let Err(e) = wordlist::install_bundled(&dir) {
                log::error!("installing bundled wordlists: {e:#}");
            }
            wordlists.lists = wordlist::discover(&dir, &cfg.disabled_wordlists);
        }

        let mut app = Self {
            doc: Document::default(),
            plan: Vec::new(),
            paragraphs: Vec::new(),
            hits: Vec::new(),
            skipped_chunks: 0,
            plan_pos: 0,
            state: PlayState::Idle,
            follow_cursor: false,
            system,
            eleven: ElevenLabs::new(move || ctx2.request_repaint()),
            el_voices: Vec::new(),
            el_synthesising: None,
            el_can_resume: false,
            wordlists,
            voice_filter: String::new(),
            export: Export::default(),
            export_estimate: export::Estimate::default(),
            confirm_export: false,
            pending_describe: false,
            show_description: false,
            synthesis_began: None,
            vision: Vision::default(),
            models: Vec::new(),
            models_requested: false,
            image_path: None,
            description: String::new(),
            sounds: Sounds::new(cfg.sounds_enabled),
            tab: Tab::General,
            preview: Preview::Document,
            draft: cfg.clone(),
            log_dir_field: cfg
                .log_dir
                .clone()
                .unwrap_or_else(logging::default_log_dir)
                .display()
                .to_string(),
            api_key_rejected: false,
            show_key_dialog: false,
            key_draft: String::new(),
            status: None,
            settings_dirty: None,
            update_checker,
            update_info: None,
            show_update_dialog: false,
            cfg,
        };

        app.apply_voice_settings();
        if let Some(path) = initial_file {
            app.open_path(&path, &cc.egui_ctx);
        }
        app
    }

    // ---------------------------------------------------------------- status

    fn info(&mut self, text: impl Into<String>) {
        let text = text.into();
        log::info!("{text}");
        self.status = Some(Status {
            text,
            kind: StatusKind::Info,
        });
    }

    fn error(&mut self, text: impl Into<String>) {
        let text = text.into();
        log::error!("{text}");
        self.sounds.failure();
        self.status = Some(Status {
            text,
            kind: StatusKind::Error,
        });
    }

    fn mark_settings_dirty(&mut self) {
        self.settings_dirty = Some(Instant::now());
    }

    fn save_settings_now(&mut self) {
        self.settings_dirty = None;
        self.cfg.disabled_wordlists = self
            .wordlists
            .lists
            .iter()
            .filter(|l| !l.enabled)
            .map(|l| l.name.clone())
            .collect();
        if let Err(e) = self.cfg.save() {
            log::error!("saving settings: {e:#}");
        }
    }

    // -------------------------------------------------------------- document

    fn open_file_dialog(&mut self, ctx: &egui::Context) {
        // Everything openable in one filter: someone reaching for "Open file"
        // with an image in mind should not have to know it lives on another
        // tab. The narrower filters stay for anyone who wants them.
        let everything: Vec<&str> = SUPPORTED_EXTENSIONS
            .iter()
            .chain(IMAGE_EXTENSIONS)
            .copied()
            .collect();
        let picked = rfd::FileDialog::new()
            .set_title(t!("pick.open_title"))
            .add_filter(t!("pick.filter_everything"), &everything)
            .add_filter(t!("pick.filter_documents"), SUPPORTED_EXTENSIONS)
            .add_filter(t!("pick.filter_images"), IMAGE_EXTENSIONS)
            .add_filter(t!("pick.filter_all"), &["*"])
            .pick_file();
        if let Some(path) = picked {
            self.open_path(&path, ctx);
        }
    }

    /// Route a file to whichever half of the app knows what to do with it.
    fn open_path(&mut self, path: &std::path::Path, ctx: &egui::Context) {
        if is_image(path) {
            self.load_image(path.to_path_buf(), ctx);
        } else {
            self.load_document(path);
        }
    }

    /// Take an image as the subject of the preview pane: the picture is not
    /// the document, so leaving the reader on screen with nothing visibly
    /// changed would be a dead end.
    fn load_image(&mut self, path: PathBuf, ctx: &egui::Context) {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        self.description.clear();
        self.show_description = false;
        self.image_path = Some(path);
        // The picture is what is open, so it takes the preview pane; the tabs
        // keep whatever the user was doing.
        self.preview = Preview::Image;
        // Opening an image is the whole instruction: describing it is what the
        // user came for, so nothing further should have to be pressed.
        self.pending_describe = true;
        self.list_models_once(ctx);
        self.info(t!("status.opened_image", name = name));
        self.advance_auto_describe(ctx);
    }

    /// Send the current image to the model.
    fn describe_now(&mut self, ctx: &egui::Context) {
        let Some(image) = self.image_path.clone() else {
            return;
        };
        if self.vision.is_busy() {
            return;
        }
        self.show_description = false;
        let ctx = ctx.clone();
        self.vision.describe(
            self.cfg.ollama_url.clone(),
            self.cfg.ollama_model.clone(),
            self.cfg.ollama_prompt.clone(),
            image,
            self.cfg.geotag_images,
            move || ctx.request_repaint(),
        );
    }

    /// Start the waiting description once a model is known.
    ///
    /// Opening an image usually beats the answer from Ollama — which may have
    /// had to be started first — so the request waits here for the model list
    /// rather than failing and asking the user to try again.
    fn advance_auto_describe(&mut self, ctx: &egui::Context) {
        if !self.pending_describe || self.vision.is_busy() {
            return;
        }
        if self.image_path.is_none() {
            self.pending_describe = false;
            return;
        }
        if self.cfg.ollama_model.trim().is_empty() {
            return; // Still waiting to hear what is installed.
        }
        self.pending_describe = false;
        self.describe_now(ctx);
    }

    /// Write the description out as a plain text file.
    fn save_description(&mut self) {
        let stem = self
            .image_path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "description".to_string());
        let Some(path) = rfd::FileDialog::new()
            .set_title(t!("pick.save_description_title"))
            .set_file_name(export::suggested_filename(&stem, "txt"))
            .add_filter(t!("pick.filter_text"), &["txt"])
            .save_file()
        else {
            return;
        };
        let path = export::with_extension(path, "txt");
        match std::fs::write(&path, &self.description) {
            Ok(()) => {
                self.sounds.success();
                self.info(t!(
                    "status.saved",
                    name = path.file_name().unwrap_or(path.as_os_str()).to_string_lossy()
                ));
            }
            Err(e) => self.error(t!("error.save_description", reason = e)),
        }
    }

    /// Read the description as though it were the document, because it now is.
    fn read_description_aloud(&mut self) {
        let text = self.description.clone();
        let title = self
            .image_path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| t!("doc.description_of", name = n.to_string_lossy()))
            .unwrap_or_else(|| t!("doc.image_description"));
        self.set_document_text(&title, text);
        self.play();
    }

    /// Ask Ollama what it has installed.
    fn list_models(&mut self, ctx: &egui::Context) {
        self.models_requested = true;
        let url = self.cfg.ollama_url.clone();
        let ctx = ctx.clone();
        self.vision.list_models(url, move || ctx.request_repaint());
    }

    /// The same, but only the first time it is needed. Without this the model
    /// menu is empty until the user finds the refresh button, which reads as
    /// "no models installed" when the truth is "nobody has asked yet".
    fn list_models_once(&mut self, ctx: &egui::Context) {
        if self.models_requested || self.vision.is_busy() {
            return;
        }
        self.list_models(ctx);
    }

    /// Why saving audio is unavailable right now, or `None` if it is.
    ///
    /// The system voices are missing rather than disabled: the platform speech
    /// engines play to the sound card and offer no way to capture what they
    /// produce, so there is nothing to write.
    fn export_blocker(&self) -> Option<String> {
        if self.export.is_running() {
            return Some(t!("block.already_saving"));
        }
        if self.plan.is_empty() {
            return Some(t!("block.no_file"));
        }
        if self.cfg.engine != EngineKind::ElevenLabs {
            return Some(t!("block.needs_elevenlabs"));
        }
        if self.cfg.effective_api_key().is_empty() {
            return Some(t!("block.needs_key"));
        }
        if self.cfg.elevenlabs_voice_id.is_empty() {
            return Some(t!("block.needs_voice"));
        }
        None
    }

    /// How long saving the whole document would take, on current form.
    fn export_duration(&self) -> Duration {
        self.export_estimate.for_sentences(self.plan.len())
    }

    /// The button was pressed. A short document goes straight to the save
    /// dialog; a long one is worth asking about first, because it is one paid
    /// request per sentence and there is no way to know that from the button.
    fn save_as_mp3(&mut self, ctx: &egui::Context) {
        if let Some(reason) = self.export_blocker() {
            self.error(reason);
            return;
        }
        if self.export_duration() >= CONFIRM_EXPORT_ABOVE {
            self.confirm_export = true;
            return;
        }
        self.choose_mp3_destination(ctx);
    }

    /// Ask for a destination and start writing the document to it.
    fn choose_mp3_destination(&mut self, ctx: &egui::Context) {
        if let Some(reason) = self.export_blocker() {
            self.error(reason);
            return;
        }
        let Some(path) = rfd::FileDialog::new()
            .set_title(t!("pick.save_mp3_title"))
            .set_file_name(export::suggested_filename(&self.doc.title, "mp3"))
            .add_filter(t!("pick.filter_mp3"), &["mp3"])
            .save_file()
        else {
            return;
        };
        let path = export::with_extension(path, "mp3");

        // The plan, not the document: what gets saved is exactly what would be
        // read aloud, wordlists and all.
        let texts: Vec<String> = self.plan.iter().map(|p| p.text.clone()).collect();
        let sentences = texts.len();
        let ctx = ctx.clone();
        self.export.start(
            path.clone(),
            texts,
            self.voice_request(),
            move || ctx.request_repaint(),
        );
        if self.export.is_running() {
            self.info(tn!(
                "status.saving_mp3",
                sentences,
                name = path.file_name().unwrap_or(path.as_os_str()).to_string_lossy()
            ));
        } else {
            self.error(t!("error.export_start"));
        }
    }

    fn load_document(&mut self, path: &std::path::Path) {
        self.stop();
        self.preview = Preview::Document;
        match Document::from_path(path, self.cfg.chunk_mode) {
            Ok(doc) => {
                let words = doc.word_count();
                let title = doc.title.clone();
                self.doc = doc;
                self.rebuild_plan();
                self.info(t!(
                    "status.opened_document",
                    title = title,
                    words = words,
                    chunks = self.doc.chunks.len()
                ));
            }
            Err(e) => self.error(t!(
                "error.open_document",
                path = path.display(),
                reason = format!("{e:#}")
            )),
        }
    }

    fn set_document_text(&mut self, title: &str, text: String) {
        self.stop();
        self.preview = Preview::Document;
        self.doc = Document::from_text(title, text, self.cfg.chunk_mode);
        self.rebuild_plan();
        self.info(t!("status.loaded", title = title));
    }

    /// Rebuild the plan when the document may be playing. The wordlists decide
    /// what is safe to say out loud, so a change to them has to reach the
    /// listener immediately: the ElevenLabs worker holds its own copy of the
    /// whole plan and would otherwise read the old text to the end.
    fn rebuild_plan_live(&mut self) {
        let anchor = self.plan.get(self.plan_pos).map(|p| p.chunk_index);
        let was_playing = self.state == PlayState::Playing;
        self.rebuild_plan();
        if !was_playing {
            return;
        }
        // Resume at the first chunk still spoken at or after where we were; the
        // one we were on may itself have just been filtered out.
        match anchor.and_then(|chunk| self.plan.iter().position(|p| p.chunk_index >= chunk)) {
            Some(position) => self.start_from(position),
            None => self.stop(),
        }
    }

    /// Run the wordlists over the document and work out what will be spoken.
    fn rebuild_plan(&mut self) {
        // Whatever the worker is holding is now out of date.
        self.el_can_resume = false;
        self.plan.clear();
        self.hits.clear();
        self.skipped_chunks = 0;

        let filtering = self.cfg.wordlists_enabled;
        for (index, chunk) in self.doc.chunks.iter().enumerate() {
            let text = if filtering {
                let applied = self.wordlists.apply(&chunk.display);
                self.hits.extend(applied.hits);
                if applied.skipped {
                    self.skipped_chunks += 1;
                    continue;
                }
                applied.text
            } else {
                chunk.display.clone()
            };

            if text.trim().is_empty() {
                continue;
            }
            self.plan.push(PlanItem {
                chunk_index: index,
                text,
            });
        }

        self.paragraphs = group_paragraphs(&self.doc, self.cfg.chunk_mode);
        self.plan_pos = self.plan_pos.min(self.plan.len().saturating_sub(1));
        log::debug!(
            "plan rebuilt: {} chunks -> {} spoken, {} skipped, {} substitutions",
            self.doc.chunks.len(),
            self.plan.len(),
            self.skipped_chunks,
            self.hits.len()
        );
    }

    /// The document chunk currently being spoken, if any.
    fn active_chunk(&self) -> Option<usize> {
        if !self.state.is_active() {
            return None;
        }
        self.plan.get(self.plan_pos).map(|p| p.chunk_index)
    }

    // -------------------------------------------------------------- playback

    fn apply_voice_settings(&mut self) {
        let (rate, pitch, volume) = (self.cfg.rate, self.cfg.pitch, self.cfg.volume);
        if let Ok(system) = &mut self.system {
            system.apply_settings(rate, pitch, volume);
        }
        if self.cfg.engine == EngineKind::ElevenLabs {
            self.eleven.send(ElCommand::SetGain(volume));
        }
    }

    fn voice_request(&self) -> VoiceRequest {
        VoiceRequest {
            api_key: self.cfg.effective_api_key(),
            voice_id: self.cfg.elevenlabs_voice_id.clone(),
            model: self.cfg.elevenlabs_model.clone(),
            stability: self.cfg.elevenlabs_stability,
            similarity: self.cfg.elevenlabs_similarity,
        }
    }

    fn play(&mut self) {
        if self.plan.is_empty() {
            self.error(t!("error.nothing_to_read"));
            return;
        }
        // Resuming ElevenLabs is a real resume; the system engine has no pause,
        // so it restarts the current sentence instead.
        if self.state == PlayState::Paused
            && self.cfg.engine == EngineKind::ElevenLabs
            && self.el_can_resume
        {
            self.eleven.send(ElCommand::Resume);
            self.state = PlayState::Playing;
            return;
        }
        self.start_from(self.plan_pos);
    }

    fn start_from(&mut self, position: usize) {
        if position >= self.plan.len() {
            self.stop();
            return;
        }
        self.plan_pos = position;
        self.follow_cursor = true;
        self.apply_voice_settings();

        match self.cfg.engine {
            EngineKind::System => {
                let Ok(system) = &mut self.system else {
                    let reason = self.system.as_ref().err().cloned().unwrap_or_default();
                    self.error(t!("error.no_system_voice", reason = reason));
                    return;
                };
                let result = if system.tracks_progress() {
                    system.speak(&self.plan[position].text)
                } else {
                    // Cannot follow along, so hand over the whole document and
                    // let the OS queue it.
                    let texts: Vec<String> =
                        self.plan[position..].iter().map(|p| p.text.clone()).collect();
                    system.speak_all(&texts)
                };
                match result {
                    Ok(()) => self.state = PlayState::Playing,
                    Err(e) => self.error(t!("error.could_not_speak", reason = format!("{e:#}"))),
                }
            }
            EngineKind::ElevenLabs => {
                if self.cfg.effective_api_key().is_empty() {
                    self.error(t!("error.needs_key_or_system"));
                    return;
                }
                if self.cfg.elevenlabs_voice_id.is_empty() {
                    self.error(t!("error.needs_voice"));
                    return;
                }
                let texts: Vec<String> = self.plan.iter().map(|p| p.text.clone()).collect();
                self.eleven.send(ElCommand::Play {
                    texts,
                    start: position,
                    request: self.voice_request(),
                    gain: self.cfg.volume,
                    preview: false,
                });
                self.state = PlayState::Playing;
            }
        }
    }

    fn pause(&mut self) {
        match self.cfg.engine {
            EngineKind::System => {
                if let Ok(system) = &mut self.system {
                    system.stop();
                }
            }
            EngineKind::ElevenLabs => {
                self.eleven.send(ElCommand::Pause);
                self.el_can_resume = true;
            }
        }
        self.state = PlayState::Paused;
    }

    fn stop(&mut self) {
        if let Ok(system) = &mut self.system {
            system.stop();
        }
        self.eleven.send(ElCommand::Stop);
        self.el_can_resume = false;
        self.state = PlayState::Idle;
        self.plan_pos = 0;
        self.el_synthesising = None;
    }

    fn skip(&mut self, delta: isize) {
        if self.plan.is_empty() {
            return;
        }
        let target = (self.plan_pos as isize + delta).clamp(0, self.plan.len() as isize - 1);
        let target = target as usize;
        if self.state == PlayState::Playing {
            self.start_from(target);
        } else {
            self.plan_pos = target;
            self.follow_cursor = true;
        }
    }

    /// Start reading at whichever plan item covers this document chunk.
    fn play_from_chunk(&mut self, chunk_index: usize) {
        let position = self
            .plan
            .iter()
            .position(|p| p.chunk_index >= chunk_index)
            .unwrap_or(0);
        self.start_from(position);
    }

    /// Advance the engines. Runs every frame, including while hidden, so that
    /// playback keeps going when the window is not visible.
    fn pump(&mut self, ctx: &egui::Context) {
        // System speech: has the current sentence finished?
        let finished = match &mut self.system {
            Ok(system) if self.state == PlayState::Playing => system.poll_finished(),
            _ => false,
        };
        if finished && self.cfg.engine == EngineKind::System {
            let next = self.plan_pos + 1;
            if next >= self.plan.len() {
                self.state = PlayState::Idle;
                self.plan_pos = 0;
                self.sounds.success();
                self.info(t!("status.finished"));
            } else {
                self.start_from(next);
            }
        }

        // ElevenLabs worker events.
        while let Some(event) = self.eleven.try_recv() {
            match event {
                elevenlabs::Event::Started(index) => {
                    self.el_synthesising = None;
                    // Something was synthesised, so whatever key we hold works.
                    self.api_key_rejected = false;
                    if let Some(began) = self.synthesis_began.take() {
                        self.export_estimate.record(began.elapsed());
                    }
                    if self.cfg.engine == EngineKind::ElevenLabs {
                        self.plan_pos = index.min(self.plan.len().saturating_sub(1));
                        self.follow_cursor = true;
                        if self.state == PlayState::Idle {
                            self.state = PlayState::Playing;
                        }
                    }
                }
                elevenlabs::Event::Synthesising(index) => {
                    self.el_synthesising = Some(index);
                    // A sentence the app is actually waiting on: the same round
                    // trip an export makes, so it is worth timing.
                    self.synthesis_began = Some(Instant::now());
                }
                elevenlabs::Event::Finished => {
                    self.el_synthesising = None;
                    if self.cfg.engine == EngineKind::ElevenLabs {
                        self.state = PlayState::Idle;
                        self.plan_pos = 0;
                        self.sounds.success();
                self.info(t!("status.finished"));
                    }
                }
                elevenlabs::Event::Stopped => {
                    self.el_synthesising = None;
                }
                elevenlabs::Event::Error(message) => {
                    self.el_synthesising = None;
                    if self.cfg.engine == EngineKind::ElevenLabs {
                        self.state = PlayState::Idle;
                    }
                    // A key that has expired or been revoked brings the button
                    // for entering one back, on both tabs that offer it.
                    self.api_key_rejected = elevenlabs::is_key_rejection(&message);
                    self.error(message);
                }
                elevenlabs::Event::Voices(voices) => {
                    self.api_key_rejected = false;
                    self.sounds.success();
                    self.info(tn!("status.voices_found", voices.len()));
                    self.el_voices = voices;
                }
            }
        }

        // Saving to MP3.
        if let Some(event) = self.export.poll() {
            match event {
                export::Event::Finished {
                    path,
                    bytes,
                    elapsed,
                } => {
                    let sentences = self.plan.len().max(1) as u32;
                    self.export_estimate.record(elapsed / sentences);
                    self.sounds.success();
                    self.info(t!(
                        "status.saved_mp3",
                        name = path.file_name().unwrap_or(path.as_os_str()).to_string_lossy(),
                        size = format!("{:.1}", bytes as f64 / 1_048_576.0),
                        duration = export::approximate_duration(elapsed)
                    ));
                }
                export::Event::Cancelled => self.info(t!("status.export_cancelled")),
                export::Event::Failed(message) => {
                    if elevenlabs::is_key_rejection(&message) {
                        self.api_key_rejected = true;
                    }
                    self.error(t!("error.export_failed", reason = message));
                }
                // Progress is folded into the export itself.
                export::Event::Progress(_) => {}
            }
        }

        // Local vision model.
        if let Some(result) = self.vision.poll() {
            match result {
                VisionResult::Models(models) => {
                    let vision_count = models.iter().filter(|m| m.vision_capable).count();
                    // The default model: the first installed one that can read
                    // images, chosen so that opening a picture is enough on a
                    // machine where Ollama is set up at all.
                    if self.cfg.ollama_model.is_empty() {
                        if let Some(first) = models.iter().find(|m| m.vision_capable) {
                            self.cfg.ollama_model = first.name.clone();
                            self.mark_settings_dirty();
                        }
                    }
                    self.models = models;

                    if self.pending_describe && self.cfg.ollama_model.is_empty() {
                        // An answer arrived and there is nothing in it that can
                        // look at a picture. Say so once, rather than leaving an
                        // image waiting for a model that is never coming.
                        self.pending_describe = false;
                        self.error(t!("error.no_vision_model"));
                    } else {
                        self.info(tn!(
                            "status.models_found",
                            self.models.len(),
                            vision = vision_count
                        ));
                        self.advance_auto_describe(ctx);
                    }
                }
                VisionResult::Description(text) => {
                    self.sounds.success();
                    self.info(t!("status.image_described"));
                    self.description = text;
                    // The description is the answer to the question the user
                    // asked by opening the picture, so it comes to them.
                    self.show_description = true;
                }
                VisionResult::Error(message) => {
                    self.pending_describe = false;
                    self.error(message);
                }
            }
        }

        if self
            .settings_dirty
            .is_some_and(|t| t.elapsed() > SETTINGS_SAVE_DELAY)
        {
            self.save_settings_now();
        }

        if let Some(checker) = &mut self.update_checker {
            if let Some(found) = checker.poll() {
                self.update_checker = None;
                if let Some(info) = found {
                    let already_skipped = info.version == self.cfg.skipped_update_version;
                    log::info!("update available: v{}", info.version);
                    self.update_info = Some(info);
                    self.show_update_dialog = !already_skipped;
                }
            }
        }
    }
}

/// Group chunk indices into paragraphs so the document reads as prose rather
/// than as one sentence per line.
fn group_paragraphs(doc: &Document, mode: ChunkMode) -> Vec<Vec<usize>> {
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut current: Vec<usize> = Vec::new();
    let mut previous_end = 0usize;

    for (index, chunk) in doc.chunks.iter().enumerate() {
        // A blank line in the gap since the last chunk starts a new paragraph.
        let gap = doc.text.get(previous_end..chunk.start).unwrap_or("");
        let paragraph_break = mode == ChunkMode::Paragraph
            || gap.matches('\n').count() >= 2
            || (index > 0 && gap.contains('\n') && gap.trim().is_empty() && gap.len() > 1);

        if paragraph_break && !current.is_empty() {
            groups.push(std::mem::take(&mut current));
        }
        current.push(index);
        previous_end = chunk.end;
    }
    if !current.is_empty() {
        groups.push(current);
    }
    groups
}

impl eframe::App for AccessEngine {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump(ctx);
        // egui has its own Ctrl/Cmd + plus and minus. Someone who uses those
        // should not find the setting has forgotten it by the next launch.
        let zoom = ctx.zoom_factor();
        if (zoom - self.cfg.text_scale).abs() > f32::EPSILON {
            self.cfg.text_scale = zoom;
            // The Settings form follows: a size set with the keyboard is not an
            // unapplied change waiting for a button.
            self.draft.text_scale = zoom;
            self.mark_settings_dirty();
        }
        // While anything is in flight, keep frames coming so progress moves.
        if self.state.is_active() || self.vision.is_busy() || self.export.is_running() {
            ctx.request_repaint_after(Duration::from_millis(80));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.handle_shortcuts(ui.ctx());
        self.update_dialog(ui.ctx());
        self.export_confirm_dialog(ui.ctx());
        self.description_dialog(ui.ctx());
        self.api_key_dialog(ui.ctx());

        egui::Panel::top("tabs")
            .show(ui, |ui| self.tab_bar(ui));
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        // Tabs and their controls live on the left; the document — the thing
        // the user is actually following along with — takes the main area on
        // the right. With the preview switched off there is no second thing to
        // show, so the tabs have the window rather than leaving a gap.
        if self.cfg.show_preview {
            egui::Panel::left("side")
                .resizable(true)
                .default_size(400.0)
                .size_range(320.0..=640.0)
                .show(ui, |ui| self.side_panel(ui));
            egui::CentralPanel::default().show(ui, |ui| self.document_view(ui));
        } else {
            egui::CentralPanel::default().show(ui, |ui| self.side_panel(ui));
        }
    }

    fn save(&mut self, _storage: &mut dyn eframe::Storage) {
        self.save_settings_now();
    }
}

// ------------------------------------------------------------------- the UI

impl AccessEngine {
    /// The transport shortcuts.
    ///
    /// Two tiers, because the unmodified keys belong to whatever has keyboard
    /// focus: Space presses a focused button and the arrows move a focused
    /// slider, so those only act globally when nothing is focused. The chorded
    /// forms always work, which is what keeps the app operable for someone
    /// navigating by keyboard — previously a single Tab press silently killed
    /// every shortcut in here, including Ctrl/Cmd + O.
    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        if ctx.text_edit_focused() {
            return; // The user is typing.
        }
        // Anything focused claims the unmodified keys for itself.
        let unclaimed = !ctx.egui_wants_keyboard_input();

        let (open, stop, play, next, prev) = ctx.input(|i| {
            let command = i.modifiers.command;
            (
                command && i.key_pressed(egui::Key::O),
                i.key_pressed(egui::Key::Escape),
                (command && i.key_pressed(egui::Key::P))
                    || (unclaimed && i.key_pressed(egui::Key::Space)),
                (command && i.key_pressed(egui::Key::ArrowRight))
                    || (unclaimed && i.key_pressed(egui::Key::ArrowRight)),
                (command && i.key_pressed(egui::Key::ArrowLeft))
                    || (unclaimed && i.key_pressed(egui::Key::ArrowLeft)),
            )
        });
        if open {
            self.open_file_dialog(ctx);
        }
        if play {
            match self.state {
                PlayState::Playing => self.pause(),
                _ => self.play(),
            }
        }
        if stop && self.state.is_active() {
            self.stop();
        }
        if next {
            self.skip(1);
        }
        if prev {
            self.skip(-1);
        }
    }

    /// Every shortcut, for the list in Diagnostics. Kept beside the handler so
    /// the two cannot drift apart.
    fn shortcuts() -> [(String, String); 6] {
        [
            (t!("shortcut.open.keys"), t!("shortcut.open.what")),
            (t!("shortcut.play.keys"), t!("shortcut.play.what")),
            (t!("shortcut.stop.keys"), t!("shortcut.stop.what")),
            (t!("shortcut.skip.keys"), t!("shortcut.skip.what")),
            (t!("shortcut.tab.keys"), t!("shortcut.tab.what")),
            (t!("shortcut.enter.keys"), t!("shortcut.enter.what")),
        ]
    }

    /// A newer release was found on GitHub. Only offers a link to the release
    /// page — see `src/update.rs` for why this never downloads anything.
    fn update_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_update_dialog {
            return;
        }
        let Some(info) = self.update_info.clone() else {
            self.show_update_dialog = false;
            return;
        };

        let mut open = true;
        let response = egui::Modal::new(egui::Id::new("update-available")).show(ctx, |ui| {
            ui.set_max_width(420.0);
            ui.heading(t!("dialog.update.title", version = info.version));
            ui.add_space(4.0);
            ui.label(t!(
                "dialog.update.running",
                version = env!("CARGO_PKG_VERSION")
            ));

            if !info.notes.trim().is_empty() {
                ui.add_space(8.0);
                egui::ScrollArea::vertical()
                    .max_height(160.0)
                    .show(ui, |ui| {
                        ui.label(RichText::new(info.notes.trim()).small());
                    });
            }

            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if button(ui, &t!("dialog.update.open")).clicked() {
                    logging::open_url(&info.url);
                    open = false;
                }
                if ui.button(t!("dialog.update.later")).clicked() {
                    open = false;
                }
                if ui.button(t!("dialog.update.skip")).clicked() {
                    self.cfg.skipped_update_version = info.version.clone();
                    self.mark_settings_dirty();
                    open = false;
                }
            });
        });

        if response.should_close() || !open {
            self.show_update_dialog = false;
        }
    }

    /// Asked before a long export, because the cost of one is not visible from
    /// the button: a request per sentence, against a paid account, for as long
    /// as it takes. The figure is the app's own measured rate where it has one.
    fn export_confirm_dialog(&mut self, ctx: &egui::Context) {
        if !self.confirm_export {
            return;
        }
        // Anything that made the export impossible while the question was on
        // screen answers it: withdraw rather than ask about something that can
        // no longer happen.
        if self.export_blocker().is_some() {
            self.confirm_export = false;
            return;
        }

        let sentences = self.plan.len();
        let estimate = self.export_duration();
        let mut decision: Option<bool> = None;

        let response = egui::Modal::new(egui::Id::new("confirm-export")).show(ctx, |ui| {
            ui.set_max_width(430.0);
            ui.heading(t!("dialog.export.title"));
            ui.add_space(6.0);
            ui.label(tn!("dialog.export.body", sentences));
            ui.add_space(6.0);
            let duration = export::approximate_duration(estimate);
            ui.label(
                RichText::new(if self.export_estimate.is_measured() {
                    t!("dialog.export.measured", duration = duration)
                } else {
                    t!("dialog.export.guess", duration = duration)
                })
                .strong(),
            );
            ui.add_space(4.0);
            ui.label(RichText::new(t!("dialog.export.note")).weak().small());

            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if ui.button(t!("dialog.export.confirm")).clicked() {
                    decision = Some(true);
                }
                if ui.button(t!("common.cancel")).clicked() {
                    decision = Some(false);
                }
            });
        });

        // Escape, or a click outside, means no.
        if response.should_close() {
            decision.get_or_insert(false);
        }
        match decision {
            Some(true) => {
                self.confirm_export = false;
                self.choose_mp3_destination(ctx);
            }
            Some(false) => self.confirm_export = false,
            None => {}
        }
    }

    /// The finished description, and the two things anyone actually wants to
    /// do with one.
    ///
    /// It arrives as a dialog rather than as text on a tab with buttons under
    /// it: the description is the answer to a question the user asked by
    /// opening the picture, and an answer should present itself.
    fn description_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_description || self.description.is_empty() {
            return;
        }
        let title = self
            .image_path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| t!("doc.description_of", name = n.to_string_lossy()))
            .unwrap_or_else(|| t!("doc.image_description"));
        let description = self.description.clone();

        let mut read = false;
        let mut save = false;
        let mut close = false;

        let response = egui::Modal::new(egui::Id::new("image-description")).show(ctx, |ui| {
            ui.set_max_width(520.0);
            ui.heading(title);
            ui.add_space(8.0);
            egui::ScrollArea::vertical()
                .id_salt("description-text")
                .max_height(280.0)
                .show(ui, |ui| {
                    // Selectable, so it can still be copied by hand now that
                    // there is no Copy button under a text box.
                    ui.add(egui::Label::new(RichText::new(&description).size(15.0)).selectable(true));
                });

            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if button(ui, &t!("dialog.description.read")).clicked() {
                    read = true;
                }
                if button(ui, &t!("dialog.description.save")).clicked() {
                    save = true;
                }
                if ui.button(t!("common.close")).clicked() {
                    close = true;
                }
            });
        });

        if response.should_close() {
            close = true;
        }
        if read {
            self.show_description = false;
            self.read_description_aloud();
        } else if save {
            self.save_description();
        } else if close {
            self.show_description = false;
        }
    }

    /// The tab strip: General / Wordlists / Settings. Lives at the very top of
    /// the window rather than above just the side panel, since it is the
    /// primary way to navigate the app.
    fn tab_bar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        // Three buttons, a third of the width each, so the strip reads as one
        // control rather than as three words of different lengths.
        ui.horizontal(|ui| {
            let gaps = ui.spacing().item_spacing.x * (Tab::ALL.len() - 1) as f32;
            let width = ((ui.available_width() - gaps) / Tab::ALL.len() as f32).max(60.0);
            for tab in Tab::ALL {
                let title = tab.title();
                let button = egui::Button::selectable(self.tab == tab, centred(&title))
                    .corner_radius(6.0)
                    .min_size(egui::vec2(width, 34.0));
                if ui.add(button).clicked() {
                    self.tab = tab;
                }
            }
        });
        ui.add_space(8.0);
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(3.0);
        ui.horizontal(|ui| {
            match &self.status {
                Some(status) => {
                    // The word carries the meaning and the colour reinforces
                    // it. Colour alone would say nothing to a screen reader, or
                    // to anyone who cannot tell this green from this red.
                    let palette = theme::palette(ui.visuals());
                    let (colour, text) = match status.kind {
                        StatusKind::Info => (palette.ok, status.text.clone()),
                        StatusKind::Error => (
                            palette.bad,
                            t!("status.error_prefix", message = status.text),
                        ),
                    };
                    announce(ui.label(RichText::new(text).color(colour)));
                }
                None => {
                    ui.label(RichText::new(t!("status.ready", app = APP_NAME)).weak());
                }
            }
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                if let Some(message) = self.vision.busy_message.clone() {
                    ui.add(egui::Spinner::new().size(12.0));
                    ui.label(RichText::new(message).weak());
                }
                if self.export.is_running() {
                    let (done, total) = self.export.progress;
                    if ui.small_button(t!("common.cancel")).clicked() {
                        self.export.cancel();
                        self.info(t!("status.stopping"));
                    }
                    ui.label(
                        RichText::new(t!("status.saving_progress", done = done + 1, total = total))
                            .weak(),
                    );
                    ui.add(egui::Spinner::new().size(12.0));
                }
            });
        });
        ui.add_space(3.0);
    }

    fn side_panel(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        egui::ScrollArea::vertical()
            .id_salt("side-scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| match self.tab {
                Tab::General => self.general_tab(ui),
                Tab::Wordlists => self.wordlists_tab(ui),
                Tab::Settings => self.settings_tab(ui),
            });
    }

    // ---------------------------------------------------------- general tab

    /// Open something, say what should happen to it, and press Apply. Every
    /// other choice lives on Settings; what changes from one document to the
    /// next lives here.
    fn general_tab(&mut self, ui: &mut egui::Ui) {
        pane_header(ui, &t!("general.title"), &t!("general.subtitle"));

        if wide_button(ui, &t!("general.open"))
            .on_hover_text(t!("general.open_hint"))
            .clicked()
        {
            self.open_file_dialog(ui.ctx());
        }
        ui.add_space(6.0);
        self.open_file_line(ui);

        let outputs = labelled_options(&config::Output::ALL, config::Output::label);
        if let Some(output) =
            setting_choice(ui, "output", &t!("general.do_this"), self.cfg.output, &outputs)
        {
            self.cfg.output = output;
            self.mark_settings_dirty();
        }

        match self.cfg.engine {
            EngineKind::System => self.system_voice_picker(ui),
            EngineKind::ElevenLabs => self.elevenlabs_voice_picker(ui),
        }

        ui.add_space(14.0);
        let blocker = self.apply_blocker();
        let apply = wide_button_enabled(ui, blocker.is_none(), &t!("common.apply"));
        let apply = match blocker {
            Some(reason) => apply.on_disabled_hover_text(reason),
            None => apply.on_hover_text(match self.cfg.output {
                config::Output::ReadAloud => t!("general.apply_read_hint"),
                config::Output::SaveAudio => t!("general.apply_save_hint"),
            }),
        };
        if apply.clicked() {
            self.apply_general(ui.ctx());
        }
    }

    /// What is open, in a line: the answer to "will Apply act on what I think
    /// it will".
    fn open_file_line(&mut self, ui: &mut egui::Ui) {
        match (self.preview, self.image_path.clone()) {
            (Preview::Image, Some(path)) => {
                ui.label(
                    RichText::new(
                        path.file_name()
                            .unwrap_or(path.as_os_str())
                            .to_string_lossy()
                            .into_owned(),
                    )
                    .strong(),
                );
                let state = if self.description.is_empty() {
                    t!("general.image_pending")
                } else {
                    t!("general.image_described")
                };
                ui.label(RichText::new(state).weak().small());
            }
            _ if !self.doc.title.is_empty() => {
                let title = ui.label(RichText::new(&self.doc.title).strong());
                if let Some(path) = &self.doc.path {
                    title.on_hover_text(path.display().to_string());
                }
                ui.label(
                    RichText::new(t!(
                        "general.word_counts",
                        words = self.doc.word_count(),
                        spoken = self.plan.len()
                    ))
                    .weak()
                    .small(),
                );
            }
            _ => {
                ui.label(RichText::new(t!("general.nothing_open")).weak());
            }
        }
    }

    /// Why Apply cannot do anything yet, or `None` if it can.
    fn apply_blocker(&self) -> Option<String> {
        match self.cfg.output {
            config::Output::ReadAloud => {
                if self.plan.is_empty() && self.description.is_empty() {
                    Some(t!("block.no_document"))
                } else {
                    None
                }
            }
            config::Output::SaveAudio => self.export_blocker(),
        }
    }

    /// Carry out what the two dropdowns above the button say.
    fn apply_general(&mut self, ctx: &egui::Context) {
        self.apply_voice_settings();
        match self.cfg.output {
            config::Output::ReadAloud => {
                // A described image with no document is the description: it is
                // what the user opened the picture for, and the only text
                // there is to read.
                if self.plan.is_empty() && !self.description.is_empty() {
                    self.read_description_aloud();
                    return;
                }
                if self.state.is_active() {
                    self.stop();
                }
                self.play();
            }
            config::Output::SaveAudio => self.save_as_mp3(ctx),
        }
    }

    // --------------------------------------------------------- the API key

    /// Whether the button for entering a key belongs on screen: the cloud
    /// engine is chosen and there is either no key at all, or one ElevenLabs
    /// has since turned down.
    fn needs_api_key_for(&self, engine: EngineKind) -> bool {
        engine == EngineKind::ElevenLabs
            && !self.cfg.api_key_from_env()
            && (self.cfg.effective_api_key().is_empty() || self.api_key_rejected)
    }

    fn api_key_button(&mut self, ui: &mut egui::Ui) {
        let text = if self.api_key_rejected {
            t!("key.button_refused")
        } else {
            t!("key.button")
        };
        if wide_button(ui, &text).clicked() {
            self.key_draft = self.cfg.elevenlabs_api_key.clone();
            self.show_key_dialog = true;
        }
    }

    /// Entering the key, on its own, rather than as a field among the voice
    /// settings: it is the one thing standing between the user and the engine
    /// they just chose.
    fn api_key_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_key_dialog {
            return;
        }
        let mut save = false;
        let mut close = false;

        let response = egui::Modal::new(egui::Id::new("api-key")).show(ctx, |ui| {
            ui.set_max_width(460.0);
            ui.heading(t!("key.title"));
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                ui.label(RichText::new(t!("key.needs_account")).weak().small());
                ui.hyperlink_to(
                    RichText::new(t!("key.sign_up")).small(),
                    "https://elevenlabs.io/sign-up",
                );
                ui.label(RichText::new(t!("key.then")).weak().small());
                ui.hyperlink_to(
                    RichText::new(t!("key.find_key")).small(),
                    "https://elevenlabs.io/app/settings/api-keys",
                );
                ui.label(RichText::new(t!("key.under_settings")).weak().small());
            });

            ui.add_space(8.0);
            let label = field_label(ui, &t!("key.field"));
            ui.add(
                egui::TextEdit::singleline(&mut self.key_draft)
                    .password(true)
                    .hint_text("sk_…")
                    .desired_width(f32::INFINITY),
            )
            .labelled_by(label);

            ui.add_space(6.0);
            let mut remember = self.cfg.save_api_key;
            if ui
                .checkbox(&mut remember, t!("key.remember"))
                .changed()
            {
                self.cfg.save_api_key = remember;
            }
            // On screen rather than in a tooltip: what happens to a credential
            // is not something to hide behind a pointer nobody may be using.
            ui.label(RichText::new(t!("key.warning")).weak().small());

            ui.add_space(12.0);
            match button_row(ui, &[t!("common.cancel"), t!("common.save")]) {
                Some(0) => close = true,
                Some(_) => save = true,
                None => {}
            }
        });

        if response.should_close() {
            close = true;
        }
        if save {
            self.cfg.elevenlabs_api_key = self.key_draft.trim().to_string();
            self.api_key_rejected = false;
            self.mark_settings_dirty();
            self.show_key_dialog = false;
            self.key_draft.clear();
            if self.cfg.effective_api_key().is_empty() {
                self.error(t!("error.no_key"));
            } else {
                self.info(t!("status.key_saved"));
            }
        } else if close {
            self.show_key_dialog = false;
            self.key_draft.clear();
        }
    }

    // ------------------------------------------------------ voice pickers

    /// The voice, on General, because it is the choice that changes from one
    /// document to the next. How the voice is driven — speed, pitch, model —
    /// is a setting, and lives on Settings.
    fn system_voice_picker(&mut self, ui: &mut egui::Ui) {
        let voices = match &self.system {
            Ok(system) => system.voices.clone(),
            Err(reason) => {
                ui.add_space(8.0);
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    t!("error.no_engine_reason", reason = reason),
                );
                #[cfg(all(unix, not(target_os = "macos")))]
                ui.label(RichText::new(t!("voice.linux_hint")).weak().small());
                ui.label(RichText::new(t!("voice.use_elevenlabs")).weak().small());
                return;
            }
        };

        let filter_label = field_label(ui, &t!("voice.filter"));
        ui.add(
            egui::TextEdit::singleline(&mut self.voice_filter)
                .hint_text(t!("voice.filter_hint"))
                .desired_width(f32::INFINITY),
        )
        .labelled_by(filter_label);

        let needle = self.voice_filter.to_lowercase();
        let selected_name = self
            .cfg
            .system_voice_id
            .as_ref()
            .and_then(|id| voices.iter().find(|v| &v.id == id))
            .map(|v| format!("{} ({})", v.name, v.language))
            .unwrap_or_else(|| t!("voice.system_default"));

        let mut chosen: Option<String> = None;
        let voice_label = field_label(ui, &t!("voice.voice"));
        let width = ui.available_width();
        egui::ComboBox::from_id_salt("system-voice")
            .selected_text(selected_name)
            .width(width)
            .show_ui(ui, |ui| {
                for voice in voices.iter().filter(|v| {
                    needle.is_empty()
                        || v.name.to_lowercase().contains(&needle)
                        || v.language.to_lowercase().contains(&needle)
                }) {
                    let label = format!("{} ({})", voice.name, voice.language);
                    let selected = self.cfg.system_voice_id.as_deref() == Some(&voice.id);
                    if ui.selectable_label(selected, label).clicked() {
                        chosen = Some(voice.id.clone());
                    }
                }
            })
            .response
            .labelled_by(voice_label);
        ui.label(
            RichText::new(tn!("voice.local_count", voices.len()))
                .weak()
                .small(),
        );

        if let Some(id) = chosen {
            if let Ok(system) = &mut self.system {
                if let Err(e) = system.set_voice_by_id(&id) {
                    self.error(t!("error.voice_select", reason = format!("{e:#}")));
                } else {
                    self.cfg.system_voice_id = Some(id);
                    self.mark_settings_dirty();
                }
            }
        }
    }

    fn elevenlabs_voice_picker(&mut self, ui: &mut egui::Ui) {
        if self.needs_api_key_for(self.cfg.engine) {
            ui.add_space(8.0);
            ui.label(
                RichText::new(if self.api_key_rejected {
                    t!("voice.key_refused")
                } else {
                    t!("voice.key_needed")
                })
                .weak()
                .small(),
            );
            self.api_key_button(ui);
            return;
        }
        if self.api_key_rejected && self.cfg.api_key_from_env() {
            ui.add_space(8.0);
            ui.colored_label(ui.visuals().error_fg_color, t!("voice.env_key_refused"));
        }

        ui.add_space(8.0);
        if wide_button(ui, &t!("voice.fetch")).clicked() {
            let key = self.cfg.effective_api_key();
            if key.is_empty() {
                self.error(t!("error.enter_key_first"));
            } else {
                self.info(t!("status.fetching_voices"));
                self.eleven.send(ElCommand::FetchVoices { api_key: key });
            }
        }

        let selected = if self.cfg.elevenlabs_voice_name.is_empty() {
            t!("voice.none_selected")
        } else {
            self.cfg.elevenlabs_voice_name.clone()
        };
        let mut chosen: Option<RemoteVoice> = None;
        let voice_label = field_label(ui, &t!("voice.voice"));
        let width = ui.available_width();
        egui::ComboBox::from_id_salt("el-voice")
            .selected_text(selected)
            .width(width)
            .show_ui(ui, |ui| {
                for voice in &self.el_voices {
                    let label = if voice.description.is_empty() {
                        voice.name.clone()
                    } else {
                        format!("{} — {}", voice.name, voice.description)
                    };
                    let is_selected = self.cfg.elevenlabs_voice_id == voice.id;
                    if ui.selectable_label(is_selected, label).clicked() {
                        chosen = Some(voice.clone());
                    }
                }
            })
            .response
            .labelled_by(voice_label);
        ui.label(
            RichText::new(if self.el_voices.is_empty() {
                t!("voice.none_fetched")
            } else {
                tn!("voice.account_count", self.el_voices.len())
            })
            .weak()
            .small(),
        );
        if let Some(voice) = chosen {
            self.cfg.elevenlabs_voice_id = voice.id;
            self.cfg.elevenlabs_voice_name = voice.name;
            self.mark_settings_dirty();
        }
    }

    // -------------------------------------------------------- wordlists tab

    fn wordlists_tab(&mut self, ui: &mut egui::Ui) {
        pane_header(ui, &t!("wordlists.title"), &t!("wordlists.subtitle"));

        if let Some(enabled) = setting_choice(
            ui,
            "wordlists-enabled",
            &t!("wordlists.enabled_caption"),
            self.cfg.wordlists_enabled,
            &[(true, t!("wordlists.on")), (false, t!("wordlists.off"))],
        ) {
            self.cfg.wordlists_enabled = enabled;
            self.rebuild_plan_live();
            self.mark_settings_dirty();
        }

        ui.label(
            RichText::new(t!(
                "wordlists.active_count",
                active = self.wordlists.active_count(),
                total = self.wordlists.lists.len()
            ))
            .weak()
            .small(),
        );

        ui.add_enabled_ui(self.cfg.wordlists_enabled, |ui| {
            let policies = labelled_options(&BlockPolicy::ALL, |p: BlockPolicy| p.label());
            if let Some(policy) = setting_choice(
                ui,
                "block-policy",
                &t!("wordlists.block_caption"),
                self.wordlists.policy,
                &policies,
            ) {
                self.wordlists.policy = policy;
                self.cfg.block_policy = policy;
                self.rebuild_plan_live();
                self.mark_settings_dirty();
            }

            if self.wordlists.policy == BlockPolicy::Bleep {
                let label = field_label(ui, &t!("wordlists.bleep_caption"));
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut self.cfg.bleep_text)
                            .desired_width(f32::INFINITY),
                    )
                    .labelled_by(label)
                    .changed()
                {
                    self.wordlists.bleep_text = self.cfg.bleep_text.clone();
                    self.rebuild_plan_live();
                    self.mark_settings_dirty();
                }
            }

            ui.add_space(14.0);
            ui.label(RichText::new(t!("wordlists.installed")).strong());
            ui.label(RichText::new(t!("wordlists.installed_hint")).weak().small());
            ui.add_space(4.0);
            let mut toggled = false;
            let mut to_open: Option<PathBuf> = None;
            for list in &mut self.wordlists.lists {
                ui.horizontal(|ui| {
                    if ui
                        .checkbox(&mut list.enabled, RichText::new(&list.name).strong())
                        .changed()
                    {
                        toggled = true;
                    }
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        // Every row has an "Edit" button; the name says which.
                        let edit = named(
                            ui.small_button(t!("common.edit")),
                            &t!("wordlists.edit_named", name = list.name),
                        );
                        if edit.on_hover_text(t!("wordlists.edit_hint")).clicked() {
                            to_open = Some(list.path.clone());
                        }
                    });
                });
                let [block, replace, pronounce] = list.counts;
                ui.label(
                    RichText::new(t!(
                        "wordlists.counts",
                        block = block,
                        replace = replace,
                        pronounce = pronounce
                    ))
                    .weak()
                    .small(),
                );
                ui.add_space(4.0);
            }
            if toggled {
                self.rebuild_plan_live();
                self.mark_settings_dirty();
            }
            if let Some(path) = to_open {
                logging::open_path(&path);
            }

            if self.wordlists.lists.is_empty() {
                ui.label(RichText::new(t!("wordlists.none")).weak());
            }

            ui.add_space(10.0);
            if wide_button(ui, &t!("wordlists.install"))
                .on_hover_text(t!("wordlists.install_hint"))
                .clicked()
            {
                self.import_wordlist();
            }
            ui.add_space(6.0);
            match button_row(ui, &[t!("common.open_folder"), t!("common.reload")]) {
                Some(0) => {
                    if let Some(dir) = config::wordlist_dir() {
                        logging::open_path(&dir);
                    }
                }
                Some(_) => self.reload_wordlists(),
                None => {}
            }
        });

        ui.add_space(12.0);
        ui.separator();
        self.changes_review(ui);
    }

    fn reload_wordlists(&mut self) {
        let Some(dir) = config::wordlist_dir() else {
            self.error(t!("error.no_settings_folder"));
            return;
        };
        self.cfg.disabled_wordlists = self
            .wordlists
            .lists
            .iter()
            .filter(|l| !l.enabled)
            .map(|l| l.name.clone())
            .collect();
        self.wordlists.lists = wordlist::discover(&dir, &self.cfg.disabled_wordlists);
        self.rebuild_plan_live();
        self.info(tn!("status.wordlists_reloaded", self.wordlists.lists.len()));
    }

    fn import_wordlist(&mut self) {
        let Some(source) = rfd::FileDialog::new()
            .set_title(t!("pick.add_wordlist_title"))
            .add_filter(t!("pick.filter_wordlists"), &["wordlist", "txt", "list"])
            .pick_file()
        else {
            return;
        };
        let Some(dir) = config::wordlist_dir() else {
            self.error(t!("error.no_settings_folder"));
            return;
        };
        let Some(name) = source.file_name() else {
            return;
        };
        let destination = dir.join(name);
        // Never overwrite: an existing list is one the user may have spent an
        // afternoon on, and a silent replacement leaves nothing to undo.
        if destination.exists() {
            self.error(t!(
                "error.wordlist_exists",
                name = name.to_string_lossy()
            ));
            return;
        }
        if let Err(e) = std::fs::create_dir_all(&dir).and_then(|_| std::fs::copy(&source, &destination).map(|_| ())) {
            self.error(t!("error.wordlist_add", reason = e));
            return;
        }
        self.reload_wordlists();
    }

    /// Show exactly what the wordlists changed, so a teacher can check the
    /// filter did what they expected before playing it to a room.
    fn changes_review(&mut self, ui: &mut egui::Ui) {
        ui.heading(t!("changes.title"));
        if !self.cfg.wordlists_enabled {
            ui.label(RichText::new(t!("changes.off")).weak());
            return;
        }
        if self.doc.is_empty() {
            ui.label(RichText::new(t!("changes.no_document")).weak());
            return;
        }
        if self.hits.is_empty() && self.skipped_chunks == 0 {
            ui.label(RichText::new(t!("changes.none")).weak());
            return;
        }

        // Collapse duplicates: "damn → darn ×7" beats seven identical rows.
        let mut summary: Vec<Change> = Vec::new();
        for hit in &self.hits {
            match summary
                .iter_mut()
                .find(|c| c.original == hit.original && c.replacement == hit.replacement)
            {
                Some(entry) => entry.count += 1,
                None => summary.push(Change {
                    original: hit.original.clone(),
                    replacement: hit.replacement.clone(),
                    count: 1,
                    kind: hit.kind,
                    origin: hit.origin.clone(),
                }),
            }
        }
        summary.sort_by_key(|c| std::cmp::Reverse(c.count));

        if self.skipped_chunks > 0 {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                tn!("changes.skipped", self.skipped_chunks),
            );
        }
        ui.label(
            RichText::new(tn!("changes.total", self.hits.len()))
                .weak()
                .small(),
        );
        ui.add_space(4.0);

        egui::ScrollArea::vertical()
            .id_salt("changes")
            .max_height(220.0)
            .show(ui, |ui| {
                for change in &summary {
                    let explanation = t!(
                        "changes.explanation",
                        kind = change.kind.label(),
                        origin = change.origin
                    );
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        let colour = match change.kind {
                            wordlist::RuleKind::Pronounce => ui.visuals().weak_text_color(),
                            _ => ui.visuals().warn_fg_color,
                        };
                        ui.label(RichText::new(&change.original).strikethrough().color(colour))
                            .on_hover_text(&explanation);
                        // "»" is announced as "right-pointing double angle
                        // quotation mark", which is not what it means here.
                        named_label(ui.label("»"), &t!("changes.becomes"));
                        let shown = if change.replacement.is_empty() {
                            RichText::new(t!("changes.nothing")).italics()
                        } else {
                            RichText::new(&change.replacement).strong()
                        };
                        ui.label(shown).on_hover_text(&explanation);
                        if change.count > 1 {
                            named_label(
                                ui.label(RichText::new(format!("×{}", change.count)).weak().small()),
                                &tn!("changes.times", change.count),
                            );
                        }
                    });
                }
            });
    }

    // --------------------------------------------------------- preview pane

    /// The picture being described, and where that has got to. Shown in the
    /// preview pane, in the place a document would be: an image is what is
    /// open, so it is what the pane shows.
    fn image_preview(&mut self, ui: &mut egui::Ui, path: &std::path::Path) {
        ui.add_space(8.0);
        ui.label(
            RichText::new(
                path.file_name()
                    .unwrap_or(path.as_os_str())
                    .to_string_lossy()
                    .into_owned(),
            )
            .strong()
            .size(16.0),
        );
        ui.add_space(6.0);

        // The preview decodes with the `image` crate, which has no HEIF
        // support, so previewing a HEIC would render a load error beside a
        // description that worked perfectly well. Say so instead.
        let heif = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("heic") || e.eq_ignore_ascii_case("heif"));
        if heif {
            ui.label(RichText::new(t!("image.no_preview")).weak().small());
        } else {
            let picture = ui.add(
                egui::Image::new(file_preview_uri(path))
                    .max_height(420.0)
                    .corner_radius(4.0),
            );
            // The picture's own words, once there are some: the description is
            // what this app exists to produce, and it is the right thing for a
            // screen reader to read where the image is.
            let alt = if self.description.is_empty() {
                t!(
                    "image.alt",
                    name = path.file_name().unwrap_or(path.as_os_str()).to_string_lossy()
                )
            } else {
                self.description.clone()
            };
            picture.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::Image, true, &alt)
            });
        }

        ui.add_space(8.0);
        if let Some(message) = self.vision.busy_message.clone() {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(14.0));
                ui.label(RichText::new(message).weak());
            });
        } else if self.pending_describe {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(14.0));
                ui.label(RichText::new(t!("image.waiting")).weak());
            });
        } else {
            ui.horizontal(|ui| {
                // The dialog was closed. The description still exists and cost
                // real time to produce, so it stays one press away rather than
                // being quietly discarded.
                if !self.description.is_empty()
                    && !self.show_description
                    && ui.button(t!("image.show")).clicked()
                {
                    self.show_description = true;
                }
                // Describing happens by itself when an image is opened. This is
                // for the times the answer was not what someone wanted: change
                // the model or the prompt in Settings, and this asks again.
                let can_repeat = !self.cfg.ollama_model.trim().is_empty();
                if ui
                    .add_enabled(can_repeat, egui::Button::new(t!("image.describe_again")))
                    .on_hover_text(t!("image.describe_again_hint"))
                    .on_disabled_hover_text(t!("image.no_model"))
                    .clicked()
                {
                    self.describe_now(ui.ctx());
                }
            });
        }
    }

    // --------------------------------------------------------- settings tab

    /// Everything that is a setting rather than a choice about this document.
    ///
    /// The form edits a copy: nothing here reaches the running app until Apply,
    /// so a theme can be looked at, an address half-typed, or a whole screenful
    /// of changes abandoned with Reset.
    fn settings_tab(&mut self, ui: &mut egui::Ui) {
        self.list_models_once(ui.ctx());
        pane_header(ui, &t!("settings.title"), &t!("settings.subtitle"));

        self.language_setting(ui);

        if let Some(show) = setting_choice(
            ui,
            "preview-pane",
            &t!("settings.preview_caption"),
            self.draft.show_preview,
            &[
                (true, t!("settings.preview_on")),
                (false, t!("settings.preview_off")),
            ],
        ) {
            self.draft.show_preview = show;
        }

        let engines = labelled_options(&EngineKind::ALL, |e: EngineKind| e.label());
        if let Some(engine) = setting_choice(
            ui,
            "engine",
            &t!("settings.engine"),
            self.draft.engine,
            &engines,
        ) {
            self.draft.engine = engine;
        }
        // The key is not part of the form: it is a credential, it is entered in
        // its own dialog, and it is wanted as soon as the cloud engine is
        // picked here rather than after an Apply.
        if self.needs_api_key_for(self.draft.engine) {
            self.api_key_button(ui);
        }

        if let Some(check) = setting_choice(
            ui,
            "updates",
            &t!("settings.updates"),
            self.draft.check_for_updates,
            &[
                (true, t!("settings.updates_on")),
                (false, t!("settings.updates_off")),
            ],
        ) {
            self.draft.check_for_updates = check;
        }

        if let Some(geotag) = setting_choice(
            ui,
            "geotag",
            &t!("settings.geotag"),
            self.draft.geotag_images,
            &[
                (true, t!("settings.geotag_on")),
                (false, t!("settings.geotag_off")),
            ],
        ) {
            self.draft.geotag_images = geotag;
        }
        ui.label(RichText::new(t!("settings.geotag_note")).weak().small());

        let themes = labelled_options(&config::Appearance::ALL, config::Appearance::label);
        if let Some(appearance) = setting_choice(
            ui,
            "appearance",
            &t!("settings.theme"),
            self.draft.appearance,
            &themes,
        ) {
            self.draft.appearance = appearance;
        }

        ui.add_space(14.0);
        ui.separator();
        self.voice_settings(ui);

        ui.add_space(14.0);
        ui.separator();
        self.reading_settings(ui);

        ui.add_space(14.0);
        ui.separator();
        self.log_settings(ui);

        ui.add_space(14.0);
        ui.separator();
        self.description_settings(ui);

        ui.add_space(16.0);
        ui.separator();
        ui.add_space(8.0);
        match button_row(ui, &[t!("common.reset"), t!("common.apply")]) {
            Some(0) => self.reset_settings(),
            Some(_) => self.apply_settings(ui.ctx()),
            None => {}
        }
        ui.label(
            RichText::new(if self.settings_form_changed() {
                t!("settings.unapplied")
            } else {
                t!("settings.reset_hint")
            })
            .weak()
            .small(),
        );

        ui.add_space(14.0);
        egui::CollapsingHeader::new(t!("settings.diagnostics"))
            .id_salt("diagnostics")
            .show(ui, |ui| self.diagnostics(ui));
    }

    /// The language the interface is written in, and how to add another.
    ///
    /// The picker lists what is loaded rather than what exists: a language
    /// arrives as a file in a folder, so the only way to know what is on offer
    /// is to have read it. Anything in that folder that could not be read is
    /// said here too, because a translator whose file is not in the list above
    /// has nowhere else to find out why.
    fn language_setting(&mut self, ui: &mut egui::Ui) {
        let available = i18n::available();
        let shown = if self.draft.language.trim() == i18n::AUTO {
            t!("settings.language.auto")
        } else {
            available
                .iter()
                .find(|(code, _)| *code == self.draft.language)
                .map(|(_, name)| name.clone())
                // A language that was picked and has since been taken out of
                // the folder: name the code, rather than showing an empty box.
                .unwrap_or_else(|| t!("settings.language.missing", code = self.draft.language))
        };

        let label = field_label(ui, &t!("settings.language"));
        let width = ui.available_width();
        egui::ComboBox::from_id_salt("language")
            .selected_text(shown)
            .width(width)
            .show_ui(ui, |ui| {
                ui.selectable_value(
                    &mut self.draft.language,
                    i18n::AUTO.to_string(),
                    t!("settings.language.auto"),
                );
                for (code, name) in &available {
                    ui.selectable_value(&mut self.draft.language, code.clone(), name);
                }
            })
            .response
            .labelled_by(label);

        if let Some(dir) = i18n::languages_dir() {
            ui.label(
                RichText::new(t!("settings.language.folder", path = dir.display()))
                    .weak()
                    .small(),
            );
            ui.add_space(4.0);
            match button_row(
                ui,
                &[t!("common.open_folder"), t!("settings.language.reload")],
            ) {
                Some(0) => {
                    // The folder is only made when there is something to put in
                    // it, so opening it has to make it first.
                    if let Err(e) = std::fs::create_dir_all(&dir) {
                        self.error(t!("error.language_folder", reason = e));
                    } else {
                        logging::open_path(&dir);
                    }
                }
                Some(_) => {
                    i18n::reload();
                    i18n::apply_setting(&self.cfg.language);
                    self.info(t!("status.language_reloaded", name = i18n::current_name()));
                }
                None => {}
            }
        }

        let files = i18n::folder_problems();
        if !files.is_empty() {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                tn!("settings.language.file_problems", files.len()),
            );
            for problem in &files {
                let name = problem
                    .path
                    .file_name()
                    .unwrap_or(problem.path.as_os_str())
                    .to_string_lossy();
                ui.label(
                    RichText::new(t!(
                        "settings.language.file_problem",
                        file = name,
                        why = file_reason(&problem.reason)
                    ))
                    .weak()
                    .small(),
                );
            }
        }

        // Lines the parser could not use inside the language actually in use.
        // Never anything for English, which a test holds to the format.
        let lines = i18n::current_problems();
        if !lines.is_empty() {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                tn!("settings.language.line_problems", lines.len()),
            );
            for problem in lines.iter().take(10) {
                ui.label(
                    RichText::new(t!(
                        "settings.language.line_problem",
                        line = problem.line,
                        what = problem.what
                    ))
                    .weak()
                    .small(),
                );
            }
        }
    }

    /// How the chosen engine sounds. Which voice is on General; this is the
    /// rest of it.
    fn voice_settings(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.label(RichText::new(t!("settings.voice_heading")).strong());
        match self.draft.engine {
            EngineKind::System => {
                let (supports_rate, supports_pitch, supports_volume, default_rate, default_pitch) =
                    match &self.system {
                        Ok(system) => (
                            system.supports_rate(),
                            system.supports_pitch(),
                            system.supports_volume(),
                            system.default_rate_pos(),
                            system.default_pitch_pos(),
                        ),
                        Err(_) => {
                            ui.label(RichText::new(t!("settings.no_engine")).weak().small());
                            return;
                        }
                    };

                if supports_rate {
                    let mut rate = self.draft.rate.unwrap_or(default_rate);
                    if wide_slider(ui, &t!("settings.speed"), &mut rate, 0.0..=1.0).changed() {
                        self.draft.rate = Some(rate);
                    }
                }
                if supports_pitch {
                    let mut pitch = self.draft.pitch.unwrap_or(default_pitch);
                    if wide_slider(ui, &t!("settings.pitch"), &mut pitch, 0.0..=1.0).changed() {
                        self.draft.pitch = Some(pitch);
                    }
                }
                if supports_volume {
                    let mut volume = self.draft.volume;
                    if wide_slider(ui, &t!("settings.volume"), &mut volume, 0.0..=1.0).changed() {
                        self.draft.volume = volume;
                    }
                }
                ui.add_space(8.0);
                if let Some(index) =
                    button_row(ui, &[t!("settings.reset_voice"), t!("settings.test")])
                {
                    if index == 0 {
                        self.draft.rate = None;
                        self.draft.pitch = None;
                    } else {
                        self.test_system_voice();
                    }
                }
            }
            EngineKind::ElevenLabs => {
                let label = field_label(ui, &t!("settings.model"));
                let mut model = self.draft.elevenlabs_model.clone();
                let width = ui.available_width();
                egui::ComboBox::from_id_salt("el-model")
                    .selected_text(model.clone())
                    .width(width)
                    .show_ui(ui, |ui| {
                        for (id, description) in elevenlabs::MODELS {
                            ui.selectable_value(&mut model, id.to_string(), *description);
                        }
                    })
                    .response
                    .labelled_by(label);
                self.draft.elevenlabs_model = model;

                wide_slider(
                    ui,
                    &t!("settings.stability"),
                    &mut self.draft.elevenlabs_stability,
                    0.0..=1.0,
                );
                ui.label(RichText::new(t!("settings.stability_hint")).weak().small());
                wide_slider(
                    ui,
                    &t!("settings.similarity"),
                    &mut self.draft.elevenlabs_similarity,
                    0.0..=1.0,
                );
                ui.label(RichText::new(t!("settings.similarity_hint")).weak().small());
                wide_slider(ui, &t!("settings.volume"), &mut self.draft.volume, 0.0..=1.0);

                ui.add_space(8.0);
                if wide_button(ui, &t!("settings.test_voice"))
                    .on_hover_text(t!("settings.test_voice_hint"))
                    .clicked()
                {
                    self.test_elevenlabs_voice();
                }
            }
        }
        ui.label(RichText::new(t!("settings.test_note")).weak().small());
    }

    /// Speak a sample with the form's settings rather than the applied ones:
    /// a test that ignored the slider just moved would be no test at all.
    fn test_system_voice(&mut self) {
        let (rate, pitch, volume) = (self.draft.rate, self.draft.pitch, self.draft.volume);
        if let Ok(system) = &mut self.system {
            system.apply_settings(rate, pitch, volume);
            if let Err(e) = system.speak(&t!("settings.sample_text")) {
                self.error(t!("error.could_not_speak", reason = format!("{e:#}")));
            }
        } else {
            self.error(t!("error.no_engine"));
        }
    }

    fn test_elevenlabs_voice(&mut self) {
        if self.cfg.elevenlabs_voice_id.is_empty() {
            self.error(t!("error.choose_voice_general"));
            return;
        }
        // The sample supersedes the document at the worker either way, so stop
        // first and say so, rather than leaving the UI showing a read that is
        // no longer happening.
        if self.state.is_active() {
            self.stop();
        }
        self.info(t!("status.playing_sample"));
        let request = VoiceRequest {
            api_key: self.cfg.effective_api_key(),
            voice_id: self.cfg.elevenlabs_voice_id.clone(),
            model: self.draft.elevenlabs_model.clone(),
            stability: self.draft.elevenlabs_stability,
            similarity: self.draft.elevenlabs_similarity,
        };
        self.eleven.send(ElCommand::Play {
            texts: vec![t!("settings.sample_text")],
            start: 0,
            request,
            gain: self.draft.volume,
            preview: true,
        });
    }

    fn reading_settings(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.label(RichText::new(t!("settings.reading_heading")).strong());

        let modes = labelled_options(&ChunkMode::ALL, |m: ChunkMode| m.label());
        if let Some(mode) = setting_choice(
            ui,
            "chunk-mode",
            &t!("settings.chunk"),
            self.draft.chunk_mode,
            &modes,
        ) {
            self.draft.chunk_mode = mode;
        }
        ui.label(RichText::new(t!("settings.chunk_hint")).weak().small());

        if let Some(sounds) = setting_choice(
            ui,
            "sounds",
            &t!("settings.sounds"),
            self.draft.sounds_enabled,
            &[
                (true, t!("settings.sounds_on")),
                (false, t!("settings.sounds_off")),
            ],
        ) {
            self.draft.sounds_enabled = sounds;
        }

        let (smallest, largest) = config::TEXT_SCALE_RANGE;
        let mut scale = self.draft.text_scale;
        if wide_slider(ui, &t!("settings.text_size"), &mut scale, smallest..=largest).changed() {
            self.draft.text_scale = scale;
        }
        ui.label(RichText::new(t!("settings.text_size_hint")).weak().small());
    }

    fn log_settings(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.label(RichText::new(t!("settings.logs_heading")).strong());

        let label = field_label(ui, &t!("settings.log_folder"));
        if ui
            .add(
                egui::TextEdit::singleline(&mut self.log_dir_field)
                    .hint_text(logging::default_log_dir().display().to_string())
                    .desired_width(f32::INFINITY),
            )
            .labelled_by(label)
            .changed()
        {
            // The platform's own folder is stored as "no choice made", so that
            // a machine whose data directory moves follows it.
            let typed = self.log_dir_field.trim();
            self.draft.log_dir = (!typed.is_empty()
                && std::path::Path::new(typed) != logging::default_log_dir())
            .then(|| PathBuf::from(typed));
        }

        // Asked for each frame rather than remembered from startup: a session
        // left running overnight has moved on to the next day's file.
        ui.label(
            RichText::new(t!(
                "settings.log_today",
                path = logging::log_path().display()
            ))
            .monospace()
            .small(),
        );

        ui.add_space(6.0);
        match button_row(
            ui,
            &[
                t!("common.choose"),
                t!("common.open_folder"),
                t!("common.clear"),
            ],
        ) {
            Some(0) => {
                if let Some(dir) = rfd::FileDialog::new()
                    .set_title(t!("pick.log_folder_title"))
                    .pick_folder()
                {
                    self.log_dir_field = dir.display().to_string();
                    self.draft.log_dir = (dir != logging::default_log_dir()).then_some(dir);
                }
            }
            Some(1) => logging::open_path(&logging::log_dir()),
            Some(_) => self.clear_logs(),
            None => {}
        }
        ui.label(RichText::new(t!("settings.logs_note")).weak().small());
    }

    /// Delete the logs. Not part of the form: it is an action on files, and
    /// there is nothing about it to undo with Reset.
    fn clear_logs(&mut self) {
        match logging::clear_logs() {
            Ok(0) => self.info(t!("status.no_logs")),
            Ok(count) => self.info(tn!("status.logs_cleared", count)),
            Err(e) => self.error(t!("error.clear_logs", reason = e)),
        }
    }

    fn description_settings(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.label(RichText::new(t!("settings.vision_heading")).strong());

        // Editing this changes nothing until Apply: the model list below is
        // still the one fetched from the address in use.
        let label = field_label(ui, &t!("settings.ollama_url"));
        ui.add(
            egui::TextEdit::singleline(&mut self.draft.ollama_url).desired_width(f32::INFINITY),
        )
        .labelled_by(label);
        // The promise holds only while Ollama is on this machine, and the
        // address above is editable, so it is stated conditionally.
        if vision::is_local(&self.draft.ollama_url) {
            ui.label(RichText::new(t!("settings.ollama_local")).weak().small());
        } else {
            let url = self.draft.ollama_url.trim().to_string();
            ui.colored_label(
                theme::palette(ui.visuals()).warn,
                if self.draft.ollama_url.trim_start().starts_with("https://") {
                    t!("settings.ollama_remote", url = url)
                } else {
                    t!("settings.ollama_remote_plain", url = url)
                },
            );
        }

        let label = field_label(ui, &t!("settings.model"));
        let mut model = self.draft.ollama_model.clone();
        let shown = if model.is_empty() {
            t!("settings.model_none")
        } else {
            model.clone()
        };
        let width = ui.available_width();
        egui::ComboBox::from_id_salt("ollama-model")
            .selected_text(shown)
            .width(width)
            .show_ui(ui, |ui| {
                for info in &self.models {
                    let name = if info.vision_capable {
                        t!("settings.model_vision", name = info.name)
                    } else {
                        info.name.clone()
                    };
                    ui.selectable_value(&mut model, info.name.clone(), name);
                }
            })
            .response
            .labelled_by(label);
        self.draft.ollama_model = model;

        if self.models.is_empty() {
            let hint = if self.vision.is_busy() {
                t!("settings.models_looking")
            } else {
                t!("settings.models_none")
            };
            ui.label(RichText::new(hint).weak().small());
        } else if !self.draft.ollama_model.is_empty()
            && !self
                .models
                .iter()
                .any(|m| m.name == self.draft.ollama_model && m.vision_capable)
        {
            ui.colored_label(ui.visuals().warn_fg_color, t!("settings.model_maybe_blind"));
        }
        ui.add_space(6.0);
        if wide_button(ui, &t!("settings.find_models"))
            .on_hover_text(t!("settings.find_models_hint"))
            .clicked()
        {
            self.list_models(ui.ctx());
        }

        let label = field_label(ui, &t!("settings.prompt"));
        ui.add(
            egui::TextEdit::multiline(&mut self.draft.ollama_prompt)
                .desired_rows(5)
                .desired_width(f32::INFINITY),
        )
        .labelled_by(label);
        ui.add_space(6.0);
        if wide_button(ui, &t!("settings.prompt_reset")).clicked() {
            self.draft.ollama_prompt = config::DEFAULT_VISION_PROMPT.to_string();
        }
    }

    // ------------------------------------------------------- the form itself

    /// Whether the form holds anything not yet applied.
    fn settings_form_changed(&self) -> bool {
        // Both sides start from the defaults and take only the fields the form
        // owns, so this compares those and nothing else — a voice picked on
        // General cannot make the form look unsaved.
        let mut applied = Config::default();
        transfer_settings(&self.cfg, &mut applied);
        let mut edited = Config::default();
        transfer_settings(&self.draft, &mut edited);
        serde_json::to_string(&applied).ok() != serde_json::to_string(&edited).ok()
    }

    /// Put the form back to the settings in use.
    fn reset_settings(&mut self) {
        transfer_settings(&self.cfg, &mut self.draft);
        self.log_dir_field = self
            .cfg
            .log_dir
            .clone()
            .unwrap_or_else(logging::default_log_dir)
            .display()
            .to_string();
        self.info(t!("status.settings_reset"));
    }

    /// Commit the form, and make each change take effect now rather than at
    /// the next launch.
    fn apply_settings(&mut self, ctx: &egui::Context) {
        let engine_changed = self.draft.engine != self.cfg.engine;
        let language_changed = self.draft.language != self.cfg.language;
        let chunk_changed = self.draft.chunk_mode != self.cfg.chunk_mode;
        let log_dir_changed = self.draft.log_dir != self.cfg.log_dir;

        transfer_settings(&self.draft, &mut self.cfg);

        if language_changed {
            i18n::apply_setting(&self.cfg.language);
        }
        theme::apply_appearance(ctx, self.cfg.appearance);
        ctx.set_zoom_factor(self.cfg.text_scale);
        self.sounds.enabled = self.cfg.sounds_enabled;
        if engine_changed {
            self.stop();
        }
        self.apply_voice_settings();
        self.eleven.send(ElCommand::SetGain(self.cfg.volume));
        if chunk_changed {
            self.stop();
            self.doc.rechunk(self.cfg.chunk_mode);
            self.rebuild_plan();
        }

        let mut message = t!("status.settings_applied");
        if log_dir_changed {
            match logging::set_dir(self.cfg.log_dir.clone()) {
                Ok(dir) => {
                    message = t!("status.settings_applied_log", path = dir.display());
                }
                Err(e) => {
                    // A log folder that cannot be written to is worse than the
                    // default one, so the rest of the settings still land and
                    // this one goes back.
                    self.cfg.log_dir = None;
                    self.draft.log_dir = None;
                    self.log_dir_field = logging::default_log_dir().display().to_string();
                    self.error(t!("error.log_folder", reason = e));
                    self.save_settings_now();
                    return;
                }
            }
        }
        self.save_settings_now();
        self.info(message);
    }

    // ------------------------------------------------------------ diagnostics

    /// How the app is set up, and what it has been doing. Folded away under
    /// the settings it explains: it is for the times something is wrong.
    fn diagnostics(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new(t!("diag.keyboard")).strong());
        egui::Grid::new("shortcuts")
            .num_columns(2)
            .spacing([16.0, 4.0])
            .show(ui, |ui| {
                for (keys, what) in Self::shortcuts() {
                    ui.label(RichText::new(keys).monospace().small());
                    ui.label(RichText::new(what).small());
                    ui.end_row();
                }
            });
        ui.label(RichText::new(t!("diag.keyboard_note")).weak().small());

        ui.add_space(10.0);
        ui.label(RichText::new(t!("diag.engine")).strong());
        match &self.system {
            Ok(system) => {
                ui.label(
                    RichText::new(t!(
                        "diag.system",
                        voices = system.voices.len(),
                        progress = yes_no(system.tracks_progress()),
                        rate = yes_no(system.supports_rate()),
                        pitch = yes_no(system.supports_pitch()),
                        volume = yes_no(system.supports_volume()),
                    ))
                    .small(),
                );
            }
            Err(reason) => {
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    t!("diag.system_error", reason = reason),
                );
            }
        }
        ui.label(
            RichText::new(t!(
                "diag.document",
                chunks = self.doc.chunks.len(),
                spoken = self.plan.len(),
                skipped = self.skipped_chunks,
                changes = self.hits.len()
            ))
            .small(),
        );

        if let Some(dir) = config::config_dir() {
            ui.add_space(6.0);
            ui.label(RichText::new(t!("diag.settings_folder")).strong());
            ui.label(RichText::new(dir.display().to_string()).monospace().small());
        }

        ui.add_space(10.0);
        ui.label(RichText::new(t!("diag.updates")).strong());
        let checking = self.update_checker.is_some();
        if wide_button_enabled(ui, !checking, &t!("diag.check_now")).clicked() {
            let ctx = ui.ctx().clone();
            self.update_checker = Some(UpdateChecker::start(move || ctx.request_repaint()));
        }
        ui.horizontal(|ui| {
            if checking {
                ui.add(egui::Spinner::new().size(12.0));
                ui.label(RichText::new(t!("diag.checking")).weak());
            } else if let Some(info) = &self.update_info {
                ui.label(t!("diag.update_available", version = info.version));
                if !self.show_update_dialog && ui.small_button(t!("diag.show_dialog")).clicked() {
                    self.show_update_dialog = true;
                }
            } else {
                ui.label(
                    RichText::new(t!("diag.running", version = env!("CARGO_PKG_VERSION"))).weak(),
                );
            }
        });

        ui.add_space(10.0);
        ui.label(RichText::new(t!("diag.log_lines")).strong());
        let lines = logging::tail(200);
        egui::ScrollArea::vertical()
            .id_salt("log-tail")
            .max_height(280.0)
            .stick_to_bottom(true)
            .show(ui, |ui| {
                if lines.is_empty() {
                    ui.label(RichText::new(t!("diag.no_log")).weak());
                }
                for line in &lines {
                    let colour = if line.contains(" ERROR ") {
                        Some(ui.visuals().error_fg_color)
                    } else if line.contains(" WARN ") {
                        Some(ui.visuals().warn_fg_color)
                    } else {
                        None
                    };
                    let mut text = RichText::new(line).monospace().small();
                    if let Some(colour) = colour {
                        text = text.color(colour);
                    }
                    ui.label(text);
                }
            });
    }

    // -------------------------------------------------------- document view

    fn document_view(&mut self, ui: &mut egui::Ui) {
        if self.preview == Preview::Image {
            if let Some(path) = self.image_path.clone() {
                egui::ScrollArea::vertical()
                    .id_salt("image-preview")
                    .auto_shrink([false, false])
                    .show(ui, |ui| self.image_preview(ui, &path));
                return;
            }
        }
        if self.doc.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.label(
                    RichText::new(t!("doc.empty")).size(16.0)
                    .weak(),
                );
            });
            return;
        }

        let active = self.active_chunk();
        let cursor_chunk = self.plan.get(self.plan_pos).map(|p| p.chunk_index);
        let highlight = if ui.visuals().dark_mode {
            Color32::from_rgb(60, 82, 120)
        } else {
            Color32::from_rgb(255, 235, 160)
        };

        // Chunks that the wordlists drop entirely: shown struck through so it
        // is obvious what the listener will not hear.
        let spoken: std::collections::HashSet<usize> =
            self.plan.iter().map(|p| p.chunk_index).collect();

        let virtualise = self.doc.chunks.len() > VIRTUALISE_ABOVE;
        let focus_group = cursor_chunk
            .and_then(|c| self.paragraphs.iter().position(|g| g.contains(&c)))
            .unwrap_or(0);

        let mut clicked: Option<usize> = None;
        // Arrow keys pressed while the reading cursor has focus. Handled here
        // rather than in `handle_shortcuts`, which stands aside for whatever
        // holds focus and so would never see them.
        let mut step: isize = 0;
        let follow = self.follow_cursor;

        egui::ScrollArea::vertical()
            .id_salt("document")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_space(8.0);
                if virtualise {
                    ui.label(
                        RichText::new(t!(
                            "doc.virtualised",
                            position = self.plan_pos + 1,
                            total = self.plan.len()
                        ))
                        .weak()
                        .small(),
                    );
                    ui.add_space(6.0);
                }

                for (group_index, group) in self.paragraphs.iter().enumerate() {
                    if virtualise && group_index.abs_diff(focus_group) > VIRTUAL_WINDOW {
                        continue;
                    }
                    ui.horizontal_wrapped(|ui| {
                        // Sentences must butt up against each other to read as
                        // prose, so the spacing comes from the text itself.
                        ui.spacing_mut().item_spacing.x = 0.0;
                        for (position, &chunk_index) in group.iter().enumerate() {
                            let chunk = &self.doc.chunks[chunk_index];
                            let last = position + 1 == group.len();
                            let text = if last {
                                chunk.display.clone()
                            } else {
                                format!("{} ", chunk.display)
                            };

                            let mut rich = RichText::new(text).size(15.5);
                            if Some(chunk_index) == active {
                                rich = rich.background_color(highlight);
                            } else if Some(chunk_index) == cursor_chunk {
                                rich = rich.underline();
                            }
                            if !spoken.contains(&chunk_index) {
                                rich = rich.strikethrough().weak();
                            }

                            // Only the sentence at the reading cursor is a tab
                            // stop. Every sentence being focusable put a few
                            // hundred stops between this pane and the rest of
                            // the window; one stop, with the arrows moving
                            // within the text, is how a document behaves.
                            let is_cursor = Some(chunk_index) == cursor_chunk;
                            let sense = if is_cursor {
                                egui::Sense::click()
                            } else {
                                egui::Sense::CLICK
                            };
                            let response = ui
                                .add(egui::Label::new(rich).sense(sense))
                                .on_hover_cursor(egui::CursorIcon::PointingHand);
                            if response.clicked() {
                                clicked = Some(chunk_index);
                            }
                            if is_cursor {
                                if response.has_focus() {
                                    step += ui.input(|i| {
                                        i.num_presses(egui::Key::ArrowRight) as isize
                                            - i.num_presses(egui::Key::ArrowLeft) as isize
                                    });
                                }
                                if follow {
                                    response.scroll_to_me(Some(Align::Center));
                                }
                            }
                        }
                    });
                    ui.add_space(10.0);
                }
                ui.add_space(24.0);
            });

        self.follow_cursor = false;
        if step != 0 {
            self.skip(step);
        }
        if let Some(chunk_index) = clicked {
            self.play_from_chunk(chunk_index);
        }
    }
}

/// One row of the changes panel: identical substitutions collapsed together.
struct Change {
    original: String,
    replacement: String,
    count: usize,
    kind: wordlist::RuleKind,
    origin: String,
}

/// Label for a button, padded either side so the text sits in the middle
/// rather than against the left edge: left-aligned words in a button much wider
/// than they are read as the start of a list rather than as the button's name.
fn centred<'a>(text: &'a str) -> (egui::Atom<'a>, egui::Atom<'a>, egui::Atom<'a>) {
    (egui::Atom::grow(), text.into(), egui::Atom::grow())
}

/// Copy the fields the Settings form owns from one config to another.
///
/// Only these: the voice on General and the wordlist rules on Wordlists are
/// edited live, and pressing Apply must not quietly undo one of those.
fn transfer_settings(from: &Config, to: &mut Config) {
    to.language = from.language.clone();
    to.show_preview = from.show_preview;
    to.engine = from.engine;
    to.check_for_updates = from.check_for_updates;
    to.geotag_images = from.geotag_images;
    to.appearance = from.appearance;
    to.text_scale = from.text_scale;
    to.chunk_mode = from.chunk_mode;
    to.sounds_enabled = from.sounds_enabled;
    to.rate = from.rate;
    to.pitch = from.pitch;
    to.volume = from.volume;
    to.elevenlabs_model = from.elevenlabs_model.clone();
    to.elevenlabs_stability = from.elevenlabs_stability;
    to.elevenlabs_similarity = from.elevenlabs_similarity;
    to.ollama_url = from.ollama_url.clone();
    to.ollama_model = from.ollama_model.clone();
    to.ollama_prompt = from.ollama_prompt.clone();
    to.log_dir = from.log_dir.clone();
}

/// A caption above a control, tied to it, with the space that separates one
/// field from the next.
///
/// Above rather than beside: a column of controls that each start after a
/// caption of a different length is a ragged edge, and the controls here are
/// meant to fill the pane.
fn field_label(ui: &mut egui::Ui, text: &str) -> egui::Id {
    ui.add_space(8.0);
    ui.label(RichText::new(text).strong()).id
}

/// A button as wide as the pane, with its label in the middle.
fn wide_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    wide_button_enabled(ui, true, text)
}

fn wide_button_enabled(ui: &mut egui::Ui, enabled: bool, text: &str) -> egui::Response {
    let width = ui.available_width();
    let response = ui.add_enabled(
        enabled,
        egui::Button::new(centred(text)).min_size(egui::vec2(width, 30.0)),
    );
    named(response, spoken(text))
}

/// A button whose label may carry a symbol in front of it.
fn button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    named(ui.button(text), spoken(text))
}

/// The words of a label, without the symbol drawn in front of them.
///
/// "📂  Open a file…" announces itself to a screen reader as "open file folder
/// Open a file": the emoji is decoration, and it is read out first, ahead of
/// the words that say what the button does. Everything before the first letter
/// or digit is left out of the spoken name, so a listener hears the sentence a
/// sighted user reads and nothing else.
///
/// A label that is *only* a symbol keeps it, since dropping it would leave a
/// button with no name at all — those are the ones [`named`] exists for, and
/// they are given a sentence of their own at the call site.
fn spoken(text: &str) -> &str {
    let words = text.trim_start_matches(|c: char| !c.is_alphanumeric());
    if words.is_empty() {
        text
    } else {
        words
    }
}

/// Buttons sharing one line: equal widths, labels centred, so the row reads as
/// one strip rather than as words of different lengths. Returns the index of
/// whichever was pressed.
fn button_row(ui: &mut egui::Ui, labels: &[String]) -> Option<usize> {
    let mut pressed = None;
    ui.horizontal(|ui| {
        let gaps = ui.spacing().item_spacing.x * labels.len().saturating_sub(1) as f32;
        let width = ((ui.available_width() - gaps) / labels.len().max(1) as f32).max(48.0);
        for (index, label) in labels.iter().enumerate() {
            let widget = egui::Button::new(centred(label)).min_size(egui::vec2(width, 30.0));
            if named(ui.add(widget), spoken(label)).clicked() {
                pressed = Some(index);
            }
        }
    });
    pressed
}

/// A dropdown filling the pane, under its own caption. Returns the new value
/// only when the user picked a different one.
fn setting_choice<T: Copy + PartialEq>(
    ui: &mut egui::Ui,
    id_salt: &str,
    caption_text: &str,
    current: T,
    options: &[(T, String)],
) -> Option<T> {
    let label = field_label(ui, caption_text);
    let shown = options
        .iter()
        .find(|(value, _)| *value == current)
        .map(|(_, text)| text.as_str())
        .unwrap_or_default();
    let mut chosen = current;
    let width = ui.available_width();
    egui::ComboBox::from_id_salt(id_salt)
        .selected_text(shown)
        .width(width)
        .show_ui(ui, |ui| {
            for (value, text) in options {
                ui.selectable_value(&mut chosen, *value, text);
            }
        })
        .response
        .labelled_by(label);
    (chosen != current).then_some(chosen)
}

/// The options for [`setting_choice`] over an enum that knows its own labels.
fn labelled_options<T: Copy>(all: &[T], label: impl Fn(T) -> String) -> Vec<(T, String)> {
    all.iter().map(|value| (*value, label(*value))).collect()
}

/// A slider filling the pane, under its own caption. egui sizes sliders from a
/// fixed width in the style, which otherwise leaves them short of the edge.
fn wide_slider(
    ui: &mut egui::Ui,
    caption_text: &str,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
) -> egui::Response {
    let label = field_label(ui, caption_text);
    let width = ui.available_width();
    ui.scope(|ui| {
        // Leave room for the value egui draws after the track.
        ui.spacing_mut().slider_width = (width - 72.0).max(80.0);
        ui.add(
            egui::Slider::new(value, range)
                .custom_formatter(|v, _| format!("{:.0}%", v * 100.0)),
        )
    })
    .inner
    .labelled_by(label)
}

/// Mark a label as somewhere a screen reader should read out of its own accord
/// when the words in it change.
///
/// Everything the app has to say about what just happened — a file opened, a
/// reading finished, a key refused — arrives in one line at the foot of the
/// window, and nothing takes focus to say it. For anyone reading the window by
/// ear that line is the whole feedback channel, and without this it is only
/// read when somebody goes looking for it.
///
/// `Polite` rather than `Assertive`: these are progress and outcomes, not
/// warnings that should cut across a sentence already being spoken.
///
/// Does nothing when no assistive technology is attached, which is when
/// `accesskit_node_builder` returns `None`.
fn announce(response: egui::Response) -> egui::Response {
    response.ctx.accesskit_node_builder(response.id, |node| {
        node.set_live(egui::accesskit::Live::Polite);
    });
    response
}

/// The same for a label, whose face is a symbol that means something other
/// than the name of the character it is.
fn named_label(response: egui::Response, name: &str) -> egui::Response {
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Label, response.enabled(), name)
    });
    response
}

/// Give a control an accessible name of its own, without changing what is on
/// screen.
///
/// A button whose face is `⏭` or `✕` announces itself as the name of that
/// character — "black right-pointing double triangle" — which is not what it
/// does. The tooltip beside it is the sentence a pointer gets; this is the same
/// sentence for everyone else. Where a caption is already drawn next to the
/// control, prefer `Response::labelled_by`: the words are then on screen too.
fn named(response: egui::Response, name: &str) -> egui::Response {
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, response.enabled(), name)
    });
    response
}

/// A pane heading with a quieter line of context under it.
fn pane_header(ui: &mut egui::Ui, title: &str, subtitle: &str) {
    ui.add_space(6.0);
    ui.heading(title);
    if !subtitle.is_empty() {
        ui.label(RichText::new(subtitle).small().weak());
    }
    ui.add_space(8.0);
}

/// Why a file in the languages folder never became a language.
fn file_reason(reason: &i18n::FileReason) -> String {
    match reason {
        i18n::FileReason::Unreadable => t!("settings.language.unreadable"),
        i18n::FileReason::NoCode => t!("settings.language.no_code"),
        i18n::FileReason::WouldReplaceEnglish => t!("settings.language.is_english"),
    }
}

/// Whether a path looks like something the app can describe rather than read.
fn is_image(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| IMAGE_EXTENSIONS.iter().any(|i| ext.eq_ignore_ascii_case(i)))
}

fn yes_no(value: bool) -> String {
    if value {
        t!("common.yes")
    } else {
        t!("common.no")
    }
}

/// A `file://` URI egui's file loader will actually resolve back to `path`.
///
/// The loader wants forward slashes even on Windows, and needs a `file:///`
/// prefix: a Unix path already starts with `/`, so `file://` + path used to
/// grow the needed third slash there for free, but a Windows path (`C:\...`)
/// does not, and two slashes sends it down the loader's UNC-path branch
/// instead — producing `\\C:/...`, which fails to load.
fn file_preview_uri(path: &std::path::Path) -> String {
    let normalised = path.to_string_lossy().replace('\\', "/");
    format!("file:///{}", normalised.trim_start_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The words a screen reader is given for a button whose face begins with
    /// a symbol. Announcing "open file folder Open a file" reads the
    /// decoration out before the instruction.
    #[test]
    fn a_symbol_in_front_of_a_label_is_not_part_of_its_name() {
        crate::i18n::with_language("en", || {
            assert_eq!(spoken(&t!("general.open")), "Open a file…");
            assert_eq!(spoken(&t!("key.button")), "Enter your ElevenLabs API key…");
            assert_eq!(spoken(&t!("voice.fetch")), "Fetch my voices");
            assert_eq!(spoken(&t!("wordlists.install")), "Install a wordlist…");
            assert_eq!(spoken(&t!("dialog.description.save")), "Save as a text file…");
            // A label with no symbol is left exactly as it is.
            assert_eq!(spoken(&t!("common.cancel")), "Cancel");
            // And one that is nothing but a symbol keeps it, rather than
            // becoming a button with no name at all.
            assert_eq!(spoken("»"), "»");
        });
    }

    /// Re-implements the relevant slice of `egui_extras`'s
    /// `file_loader::convert_uri_to_path` (as of egui 0.36) to check our URIs
    /// round-trip back to the original path rather than into its UNC-path
    /// fallback, which is what motivated `file_preview_uri` in the first
    /// place.
    fn loader_resolves_to(uri: &str) -> PathBuf {
        let s = uri.strip_prefix("file://").expect("uri has the file:// scheme");
        if cfg!(target_os = "windows") {
            if let Some(stripped) = s.strip_prefix('/') {
                return PathBuf::from(stripped);
            }
            return PathBuf::from(format!("\\\\{s}"));
        }
        PathBuf::from(s)
    }

    #[test]
    fn windows_drive_paths_round_trip_through_the_loader() {
        let path = PathBuf::from(r"C:\Users\test\image.png");
        let uri = file_preview_uri(&path);
        assert_eq!(uri, "file:///C:/Users/test/image.png");
        if cfg!(target_os = "windows") {
            assert_eq!(loader_resolves_to(&uri), PathBuf::from("C:/Users/test/image.png"));
        }
    }

    #[test]
    fn unix_absolute_paths_round_trip_through_the_loader() {
        let path = PathBuf::from("/home/test/image.png");
        let uri = file_preview_uri(&path);
        assert_eq!(uri, "file:///home/test/image.png");
        if !cfg!(target_os = "windows") {
            assert_eq!(loader_resolves_to(&uri), path);
        }
    }
}
