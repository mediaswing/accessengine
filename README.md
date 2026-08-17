# Speech Output Engine

A desktop app that reads your documents out loud, and saves the result as an
audio file. It is built around being usable by people who cannot see the screen,
cannot use a mouse, or find small low-contrast text hard going.

Choose a file, choose a voice, choose what to do with it, press **Apply**. That
is the whole app. Speech comes from the voices already built into macOS and
Windows, so it works with no account and no setup; add an ElevenLabs API key and
you get their voices instead. It reads text, Word documents, PDFs and tables.
Images work too: hand it a photo or a screenshot and a vision model running on
your own machine reads the text out of it — and so does video, which is taken
apart into stills and described a frame at a time.

The window is two panes. The list on the left — **Read a File**, **Audio
Player**, **Dictionary**, **Settings**, **Shortcuts** — chooses what the
right-hand pane shows.

## Accessibility

This is the point of the app rather than a feature of it.

- **Everything works from the keyboard.** Every control is reachable with Tab,
  every action has a shortcut, and the whole list is in the app under
  **Shortcuts** (or press <kbd>F1</kbd>). Nothing is mouse-only.
- **Every control is labelled**, and the label is tied to its control, so a
  screen reader announces "Speech engine, combo box, System voices" rather than
  reading a value with no name. Buttons say what they do in words.
- **The focused control is obvious** — a heavy accent-coloured outline, not a
  faint one-pixel ring.
- **Colour is never the only signal.** Status messages are prefixed with
  "Done:" or "Problem:" as well as coloured, and — unless you turn it off —
  starting, finishing and failing each make their own sound.
- **Contrast is designed, not inherited.** Every text colour in `src/theme.rs`
  is written down as a pair with the surface it sits on, and clears 4.5:1 in
  both the light and dark themes. The app follows your system appearance.
- **One column, one width.** Every control in the form is the same width and
  the layout never reflows, so a screen magnifier parked on the left edge stays
  useful all the way down.
- **A real bold face is bundled** (Ubuntu Bold). egui ships only a light weight,
  and at these sizes weight is the biggest single readability difference.

## What it does

- **Reads `.txt` and `.docx`.** Word documents are unpacked and only the
  readable text is kept — no style names, no stray tab stops, and character
  references like `&amp;` and `&#233;` come through as `&` and `é`.
- **Reads `.pdf`.** The page is redrawn and the words are worked back out of
  where the glyphs land, since a PDF stores neither paragraphs nor words — see
  [Reading PDFs](#reading-pdfs).
- **Two speech engines.** The voices built into your operating system, or
  ElevenLabs. Choosing ElevenLabs asks for an API key there and then, with a
  link to where ElevenLabs keeps them, and checks the key as soon as you paste
  it. A key ElevenLabs turns down is removed rather than kept.
- **Reads aloud, or saves to WAV or MP3.** One dropdown, not two buttons.
- **Plays audio files back.** The **Audio player** pane takes any WAV or MP3 —
  one this app saved, or an audiobook chapter from anywhere else — with play,
  pause, stop, a ten-second rewind for the sentence you missed, and a countdown
  of how much is left. Drop a file anywhere on the window and it lands there.
- **A sound when something starts, finishes or fails.** A short tone as the work
  begins, a chime for success, a lower tone for a problem, so you know what
  happened without watching the status line. While the work runs there is a
  quiet tick every fifteen seconds — a long job and a stuck one sound different
  — which has its own switch, since it is the one sound that reports nothing
  new. Neither ever plays over a document being read aloud, and the audio player
  stays quiet throughout, since there the sound is the point. All of it is
  under **Settings**.
- **A dictionary of word replacements** — see below.
- **Reads images** (`.jpg`, `.png`, and `.heic`/`.heif` on macOS) through a
  local [Ollama](https://ollama.com) vision model. Nothing is uploaded anywhere.
- **Describes video** (`.mp4`, `.mov`, `.m4v`, `.avi`, `.mkv`, `.webm`). Stills
  are taken wherever the picture changes enough to be a new shot, plus one every
  half minute so a long unbroken take is not described by its opening frame
  alone. Each still goes to the same vision model, and the answers are then
  written up as one continuous description. Nothing is uploaded anywhere. From
  there it is an ordinary piece of text: read it aloud, or save it as WAV or MP3
  like anything else.
- **Offers to install Ollama and ffmpeg** — with Homebrew on macOS, winget on
  Windows — the first time you open a file that needs one. It never installs
  anything without asking, and it shows you the command it would run.
- **A right-click "Speak to file" entry, on Windows.** Turn it on once in
  **Settings** and any text, Word, PDF or CSV file gets a **Speak to file**
  option in Explorer's right-click menu — no window opens, the audio just
  appears next to the file, using whatever engine and voice you last used.

## The dictionary

Words listed under **Dictionary** are swapped just before the document is
spoken. Two uses:

- **Pronunciation.** A synthesiser that says "Siobhan" wrongly will say
  "Shivawn" correctly, and you need that fix in every document, not once.
- **Substitution.** Swap a word for a gentler one so a document can be read out
  in a room with children in it.

Matching ignores capitals, and a word that started a sentence still starts one
after the swap, so a single rule covers "shit" and "Shit". **Whole word** is on
by default, so a rule for "cat" leaves "catalogue" alone. Rules apply in order,
and the file on disk is never touched — the replacement happens on the way to
the voice.

## Keyboard shortcuts

<kbd>⌘</kbd> on macOS, <kbd>Ctrl</kbd> on Windows.

| Keys | What it does |
| --- | --- |
| <kbd>⌘O</kbd> | Choose a file — a document, or an audio file in the player |
| <kbd>⌘Return</kbd> | Apply — run the chosen action |
| <kbd>⌘.</kbd> or <kbd>Esc</kbd> | Stop reading or playing, or cancel what is running |
| <kbd>⌘1</kbd> … <kbd>⌘5</kbd> | Go to Read, Audio player, Dictionary, Settings or Shortcuts |
| <kbd>⌘P</kbd> | Audio player: play, or pause if already playing |
| <kbd>⌘R</kbd> | Audio player: skip back ten seconds |
| <kbd>↑</kbd> <kbd>↓</kbd> | Move along the list of panes, once it has focus |
| <kbd>Tab</kbd> / <kbd>Shift+Tab</kbd> | Move between controls |
| <kbd>Space</kbd> or <kbd>Return</kbd> | Operate the focused control |
| <kbd>←</kbd> <kbd>→</kbd> | Change the value in an open dropdown or a slider |
| <kbd>⌘K</kbd> | ElevenLabs API key |
| <kbd>⌘L</kbd> | Show or hide the activity log |
| <kbd>F1</kbd> | The Shortcuts pane |

## Requirements

macOS or Windows. The speech engines are `say` on macOS and `System.Speech`
(SAPI 5) through PowerShell on Windows; both ship with the operating system.
HEIC photos are converted with `sips`, which is macOS-only — on Windows, save
them as JPEG or PNG first. Video needs [ffmpeg](https://ffmpeg.org); the app
offers to install it when you open one.

You need [Rust](https://rustup.rs) 1.88 or newer to build. There are no C
toolchain surprises on Windows: the TLS stack is `ring` rather than the default
`aws-lc`, which would want CMake and an assembler.

## Building and running

```sh
cargo run --release
```

You can also pass a file to open on startup, which is what "Open With" does on
both platforms:

```sh
cargo run --release -- ~/Documents/report.docx
```

To run the tests:

```sh
cargo test
```

The suite covers the `.docx` parser, the PDF reader — its object syntax, stream
filters, page tree, font encodings and the page-description interpreter, each
tested separately — the dictionary matcher, text chunking, WAV/MP3 encoding, the
words-per-minute to SAPI rate conversion, and — on macOS and Windows — renders a
real sentence through the system voice and checks the audio that comes back is
neither empty nor silent.

## Using ElevenLabs

Without a key the app just works, using the system voices. To use ElevenLabs,
choose it in the **Speech engine** dropdown and the key dialog appears; you can
also reach it any time with <kbd>⌘K</kbd>. The key is saved in `elevenlabs.key`
in the app's own settings folder — under `~/Library/Application Support` on
macOS, `%APPDATA%` on Windows — as plain text, in a file only your account can
read. It never goes on a command line where another process could read it.

Earlier versions kept it in the macOS login keychain and in a DPAPI blob on
Windows. Both reached their storage by handing the key to another program, and
both had ways to fail that produced a wrong answer without saying so; a key
saved by one of those versions is moved into the file above the first time this
one runs. See [Security](#security) for what the change does and doesn't cost.

If you'd rather not store it at all, set the environment variable instead — it
takes priority over the stored key and nothing is written to disk:

```sh
export ELEVENLABS_API_KEY=sk_…
```

There is no length limit. ElevenLabs caps a single request at 10,000 characters,
so anything longer is split into requests of at most 4,500 — at sentence
boundaries, never mid-word — and the returned audio is joined back into one
continuous recording. This applies equally to reading aloud and to saving, so a
book-length document plays and saves as one file; while it runs, the status line
says which part it is on. Each request carries the neighbouring text as context
so the voice doesn't reset its intonation at the seams. Audio is cached per
(text, voice, model), so listening to something and then saving it doesn't pay
for the same synthesis twice.

## Reading PDFs

A PDF is not a document in the sense the other readers here deal with. A `.docx`
says "this is a paragraph, and these are its words". A PDF says "put this glyph
at this point on the page" — everything a reader needs, including where a word
ends and where a line breaks, was thrown away when the file was made. So reading
one is reconstruction: the page is redrawn, and the words are worked back out of
where the glyphs land.

Two details do most of the work. Character widths are read from the file's own
embedded fonts, which is what tells a real word gap from ordinary letter
spacing — without them a line positioned glyph by glyph comes out as
"M a c B o o k". And the file is scanned for its objects rather than seeking to
them through its index, because a PDF that has been edited almost always has a
stale index, and trusting it reports readable documents as damaged.

**Nothing is uploaded, and nothing needs installing.** Unlike images and video,
a PDF is read entirely inside the app, in a second or two, which is why it is
also offered in the Windows right-click menu.

### When a PDF cannot be read

Three cases give up no text, and the message says which one you have, because
they need quite different things done about them:

- **A scan.** A PDF from a scanner or a phone is a photograph of a page with no
  text in it at all. Nothing can parse words out of one — but the image reader
  can, so save the page as a JPEG or PNG and open that instead.
- **Fonts that number their glyphs instead of naming them.** The file shows
  text, but records nothing about what its letters *are*. This is what a Chinese,
  Japanese or Korean document typeset before `/ToUnicode` became usual looks
  like, and reading it needs character tables that ship with a PDF viewer rather
  than with the file. Opening it in a viewer and copying the text out works.
- **Encryption.** Including the very common kind with no password on it, only a
  restriction on printing or copying — the text is still scrambled. Saving a
  fresh copy from a PDF viewer usually removes it.

A code no font can account for is dropped rather than guessed at. A voice
reading confident nonsense is worse than a quiet one.

### Pages that only partly decode

The awkward case is a document that is mostly fine. A long English report with a
section in Chinese, Japanese or Korean opens, reads correctly for a hundred
pages, and then reaches pages whose fonts do not say what their letters are.
Those pages come back with a third or more of their characters missing — which
looks like ordinary text on screen and is nonsense when spoken.

This is judged **per page**, not across the file, and that distinction is the
whole point: measured over a long document those pages are a rounding error, so
a whole-file check stays silent exactly when it is needed. Per page the two are
nowhere near each other — a page whose fonts decode loses a glyph or two in
several thousand, and a page whose fonts do not loses a third of itself or more.

When it happens the status line says how many pages were affected, and the log
names them and says what to do. The text is still there and still read out;
nothing is hidden and nothing is dropped on your behalf.

## Reading images

The first time you open an image, the app checks for Ollama and offers to set up
whatever is missing:

1. If Ollama isn't installed, it offers to install it — `brew install ollama` on
   macOS, `winget install Ollama.Ollama` on Windows — and shows you the command
   first. If neither package manager is there, it links to the downloads instead
   of guessing.
2. If the vision model isn't downloaded, it offers to pull it and shows the
   progress.
3. Then it starts the server if needed and reads the image.

The default model is `qwen2.5vl:3b` — about 3 GB, and good at reading text.
**Settings** offers a handful of alternatives with their download sizes, and
will take the name of any other Ollama vision model you type in. The prompt is
editable there too; the default asks for a verbatim transcription of any text in
the image, falling back to a short description if there isn't any.

Ollama occasionally retires the runner an older model was built for, at which
point that model stops loading no matter how many times it is downloaded —
`llama3.2-vision`, which earlier versions of this app defaulted to, went that
way. Settings will not let you keep a model in that state: the saved name is
swapped for a working one on upgrade, and the error you get names the model and
points at the dropdown rather than repeating Ollama's own wording.

Small models sometimes answer an elaborate prompt with nothing at all. If that
happens the app retries once with a plain question rather than reporting
failure, and says so in the log — which is what makes a 1.7B model like
`moondream` usable here as well as the larger ones.

## Describing video

A video is read the same way as an image, several dozen times over. ffmpeg takes
stills out of it, each still goes to the vision model, and a model then rewrites
the answers as one continuous description. The first time you open a video the
app offers to install ffmpeg if it is missing, the same way it offers Ollama.

**The cost is per frame, not per video.** On a machine with no graphics card a
single frame can take the better part of a minute, so which frames get taken is
the setting that matters. Three controls under **Settings** decide it:

- **How much of the picture must change for a new frame.** ffmpeg scores how
  different each frame is from the one before it; anything at or above this
  counts as a new shot. Low takes a frame when the camera merely moves, which
  describes more and takes longer. High takes one only at a clear cut.
- **Take a frame anyway after this long.** A slow pan across a landscape never
  trips a cut, and without this it would be described by its opening frame
  alone. Thirty seconds by default.
- **Most frames to describe from one video.** The stop that keeps a long or busy
  video from taking the rest of the day. Forty by default, two hundred at most.

So a ten-minute lecture on one slide costs a handful of frames, and a fast-cut
trailer of the same length costs many — which is the right way round.

The write-up is done by the vision model itself unless you name a text model in
**Settings**, so nothing further has to be downloaded to use this at all. A
dedicated text model usually writes better prose; a 3B vision model doing both
jobs will sometimes lose a frame or two out of the middle. If the write-up fails
or comes back as a stub, you get the frame-by-frame account instead — each
description under the time it appears — rather than an error, and the log says
which you got. That account is also what you get with the write-up turned off,
and it is the more trustworthy of the two: every sentence in it came from a
frame, where a joined-up narration is a model's account of what connects them.

## Right-click "Speak to file" (Windows)

Turning this on in **Settings** adds a **Speak to file** entry to the
right-click menu in Explorer for text, Word, PDF and CSV files — the file kinds
that read in a second or two, with no separate setup. Choosing it reads the
file, speaks it with whatever engine, voice and rate are currently saved, and
writes the audio next to the source (as WAV or MP3, whichever **Settings**
has you saving as) — no window opens. Running it twice on the same file never
overwrites the first result; the second is numbered, like Windows does for a
duplicate download.

Images and video stay in the app itself: they go through Ollama and ffmpeg,
can take anywhere from seconds to the better part of an hour, and the app
would have nothing to show for that time with no window open — no progress
bar, no way to cancel.

The app ships as a single portable `.exe`, usually run straight out of
`Downloads` and often deleted afterwards. So turning this on also copies the
app into its own settings folder and points the right-click entry at *that*
copy, not wherever it happened to be launched from — deleting the original
afterwards doesn't break it. Everything here is written to
`HKEY_CURRENT_USER`, so no admin prompt appears, and nothing outside your own
Windows account is touched. **Settings** shows exactly where the copy lives,
and turning the entry back off removes the right-click menu changes (the
copy itself is left in place).

## Security

The app runs other programs and opens files it did not write, so both are
treated as untrusted.

- **No shell, ever.** Nothing is passed through `sh` or `cmd`, so there is no
  layer that could reinterpret a character in a filename or a document. Every
  program is launched with its arguments as a list.
- **Programs are named by absolute path** — `/usr/bin/say`, `/usr/bin/security`,
  `System32\WindowsPowerShell\v1.0\powershell.exe`. A bare name would let
  `PATH`, or the directory the app was unzipped into, decide which binary runs —
  and one of them is handed your documents to speak, another the old stored key
  to hand back.
- **Documents never go on a command line.** macOS pipes the text to `say` over
  stdin; Windows stages it in a file the PowerShell script deletes as soon as it
  has read it. Anything that *is* interpolated into a generated script — paths,
  voice names — is escaped for a PowerShell single-quoted string, and that
  escaping is unit-tested on every platform.
- **The API key never appears on a command line**, so it is never in the process
  list, and it is kept out of the config file so that the file you would paste
  into a bug report holds no secret. It is stored as plain text in
  `elevenlabs.key` in the app's settings folder, created `0600` on macOS so no
  other account can read it. This is a deliberate step back from the keychain
  and DPAPI that earlier versions used: both worked by handing the key to
  another program, and both could fail in ways that stored the wrong thing and
  reported success. What they actually bought was less than it looks — the
  DPAPI blob decrypts for this same user, and the keychain item was written so
  that `security` could read it back without a prompt, so both were one
  subprocess away for anything already running as you. Reliably having the key
  the user typed was worth more than that margin.
- **Input is bounded.** Images are capped at 64 MB, plain text at 64 MB, a
  `.docx` body at 128 MB *after decompression*, and a PDF at 128 MB on disk with
  a further ceiling on the text taken out of it — a zip bomb is a few hundred
  kilobytes on disk, and an app that dies on one is an app that fails the person
  relying on it to read their post. PDF streams are compressed too, and a
  malformed one can describe an endless page.
- **Scratch files are created exclusively** and, on macOS, readable only by their
  owner, so a name guessed in advance is an error rather than a write through
  someone else's symlink.
- **Nothing is uploaded except to ElevenLabs**, and only when you have chosen it.
  Images are read by a model on your own machine. TLS certificate verification is
  never disabled.

## How it is put together

The binary is called `accessengine`; the app the user sees is called Speech
Output Engine.

| File | What lives there |
| --- | --- |
| `src/app.rs` | Every pane, all UI state, and the keyboard |
| `src/theme.rs` | Fonts, the contrast-checked palette, and the form metrics |
| `src/dictionary.rs` | Word replacement |
| `src/jobs.rs` | Background work and the messages it sends back |
| `src/extract/` | `.txt`, `.docx`, `.csv`, image and video → text |
| `src/extract/pdf/` | `.pdf` → text: objects, filters, pages, fonts, page description |
| `src/tts/` | The ElevenLabs and system-voice engines, and text chunking |
| `src/audio.rs` | PCM, WAV/MP3 encoding, playback and the transport |
| `src/ollama.rs` | Detecting, installing and calling Ollama |
| `src/ffmpeg.rs` | Detecting and installing ffmpeg, and taking frames out of video |
| `src/apikey.rs` | API key storage |
| `src/sysexec.rs` | Locating system programs, and staging files for them |
| `src/config.rs` | Everything else, as JSON |

The UI thread never blocks. Anything slow — a network call, a model download, an
install, rendering a file — becomes a job on its own thread that reports
progress over a channel, which the UI drains once per frame. Jobs know nothing
about egui.

Platform differences are confined to `src/tts/system.rs` and
`src/ollama.rs`, each of which has one module per platform behind a shared
signature, rather than `cfg` scattered through the app.

## Licence

MIT. See [LICENSE](LICENSE). The bundled Ubuntu Bold font is under the Ubuntu
Font Licence 1.0; see `assets/fonts/`. The four sound effects are CC0 recordings
from freesound.org, edited for length and loudness; see `assets/sounds/` for
what they are and who made them.
