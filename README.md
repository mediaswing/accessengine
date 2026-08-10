# Speech Output Engine

A desktop app that reads your documents out loud, and saves the result as an
audio file. It is built around being usable by people who cannot see the screen,
cannot use a mouse, or find small low-contrast text hard going.

Choose a file, choose a voice, choose what to do with it, press **Apply**. That
is the whole app. Speech comes from the voices already built into macOS and
Windows, so it works with no account and no setup; add an ElevenLabs API key and
you get their voices instead. Images work too: hand it a photo or a screenshot
and a vision model running on your own machine reads the text out of it.

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
  finishing and failing each make their own sound.
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
- **Two speech engines.** The voices built into your operating system, or
  ElevenLabs. Choosing ElevenLabs asks for an API key there and then.
- **Reads aloud, or saves to WAV or MP3.** One dropdown, not two buttons.
- **Plays audio files back.** The **Audio Player** pane takes any WAV or MP3 —
  one this app saved, or an audiobook chapter from anywhere else — with play,
  pause, stop and a ten-second rewind for the sentence you missed. Drop a file
  anywhere on the window and it lands there.
- **A sound when something finishes or fails.** A short chime for success, a
  lower tone for a problem, so you know which happened without watching the
  status line. Turn it off under **Settings**.
- **A dictionary of word replacements** — see below.
- **Reads images** (`.jpg`, `.png`, and `.heic`/`.heif` on macOS) through a
  local [Ollama](https://ollama.com) vision model. Nothing is uploaded anywhere.
- **Offers to install Ollama** — with Homebrew on macOS, winget on Windows — the
  first time you open an image without it. It never installs anything without
  asking, and it shows you the command it would run.

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
| <kbd>⌘1</kbd> … <kbd>⌘5</kbd> | Go to Read, Audio Player, Dictionary, Settings or Shortcuts |
| <kbd>⌘P</kbd> | Audio Player: play, or pause if already playing |
| <kbd>⌘R</kbd> | Audio Player: skip back ten seconds |
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
them as JPEG or PNG first.

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

The suite covers the `.docx` parser, the dictionary matcher, text chunking,
WAV/MP3 encoding, the words-per-minute to SAPI rate conversion, and — on macOS
and Windows — renders a real sentence through the system voice and checks the
audio that comes back is neither empty nor silent.

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
- **Input is bounded.** Images are capped at 64 MB, plain text at 64 MB, and a
  `.docx` body at 128 MB *after decompression* — a zip bomb is a few hundred
  kilobytes on disk, and an app that dies on one is an app that fails the person
  relying on it to read their post.
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
| `src/extract/` | `.txt`, `.docx` and image → text |
| `src/tts/` | The ElevenLabs and system-voice engines, and text chunking |
| `src/audio.rs` | PCM, WAV/MP3 encoding, playback and the transport |
| `src/ollama.rs` | Detecting, installing and calling Ollama |
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
Font Licence 1.0; see `assets/fonts/`. The two sound effects are CC0 recordings
from freesound.org, edited for length and loudness; see `assets/sounds/` for
what they are and who made them.
