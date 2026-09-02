# The Accessibility Engine

A cross-platform desktop reader that opens a text file and reads it aloud —
with the system's own voices, or with ElevenLabs if you have an API key. It can
describe an image using a vision model running on your own machine, and it
applies editable **wordlists** so a document can be made safe to play in a
classroom or an open-plan office, and so names and jargon are pronounced
properly.

Written in Rust with [egui](https://github.com/emilk/egui). Runs on macOS,
Windows and Linux. The binary is `accessengine`.

---

## What it does

**Reads files aloud.** Open a `.txt`, `.md`, `.csv`, `.json`, `.log`, `.rst`,
`.org`, `.ppt` or `.pptx` file. Markdown is stripped down to prose first, so you
hear the words rather than the asterisks — code blocks are skipped, links read
as their text, and images are announced by their alt text. A CSV is read as a
table: each value arrives under the name of its column, so nothing depends on
remembering a header line from four rows back. A presentation is read slide by
slide, each announced by number, and a table on a slide is read the same way as
one in a file. The document is
cut into sentences (or paragraphs, your choice); the one being spoken is
highlighted, and clicking any sentence starts reading from there.

**Two speech engines.**

- *System voices* — whatever the operating system provides. Free, offline, and
  instant. macOS offers around 180 out of the box.
- *ElevenLabs* — optional, needs an API key, and uses your account's quota.
  Each sentence is fetched while the previous one plays, so it does not stutter
  between sentences.

**Describes images.** Point it at a picture and a vision model running locally
under [Ollama](https://ollama.com) writes a description you can read aloud,
append to the document, or copy. If the photo is geotagged, where it was
taken is added to the end of the description — as coordinates, because turning
those into a place name would mean sending them to a geocoding service.
Nothing is uploaded anywhere.

**Reads presentations.** Both PowerPoint formats open: `.pptx` and the
`.ppt` that came before it, along with `.pptm`, `.pps` and `.ppsx`. Each slide
is announced by its number, and a slide with nothing on it says so rather than
being skipped, so following along by ear keeps the same count as the person at
the front of the room. A table on a slide is read as a table, the same way a
CSV is — every figure under the name of its column, because four cells read as
four unrelated words tell a listener nothing. Master slides and layouts are
left out — their placeholder text is "Click to edit Master title style", and
hearing that between every slide would be worse than hearing nothing.

The file is identified by what is inside it rather than by its name, so a
`.pptx` that arrived renamed to `.ppt` still opens. A presentation saved with a
password says so instead of reading out the ciphertext. Speaker notes are not
read.

**Speaks your language.** The interface is looked up by key rather than
written in place, so adding a language is a plain text file dropped in a folder
— see below. English ships in the binary and is the fallback for everything a
translation has not reached yet.

**Wordlists.** The distinctive part — see below.

---

## Wordlists

A wordlist rewrites text *before* it is spoken. Two jobs, one mechanism:

- make a document safe to read out to a room
- fix words the synthesiser says wrongly

They are plain text files you can edit in any editor:

```
# comment

[pronounce]
Gloucester = Gloster
SQL Server = sequel server
C# = C sharp

[replace]
damn = darn

[block]
rudeword
swear*
```

- **`[pronounce]`** — a respelling. Used exactly as written, because
  `SQL = sequel` must not become "SEQUEL": several engines spell all-caps words
  out letter by letter.
- **`[replace]`** — a milder substitute. Follows the case of the original, so
  "Damn it" reads as "Darn it".
- **`[block]`** — handled by the **Blocked words** setting: say a placeholder
  ("beep"), say nothing, or skip the whole sentence.

Matching is case-insensitive and whole-word. Multi-word phrases work and beat
single words, so `SQL Server` wins over `SQL`. A `*` at either end is a
wildcard: `swear*` catches "swearing". Where a word appears in more than one
list, the strictest rule wins — a safety rule is never masked by a
pronunciation entry.

Two lists ship with the app and are copied into your settings folder on first
run. **They are never overwritten afterwards**, so your edits survive upgrades:

- `pronunciation.wordlist` — British place names, technical jargon, and words
  voices habitually stress wrongly.
- `classroom-safe.wordlist` — a deliberately small, mild starter set.

The **Changes to this document** panel lists every substitution before you play
anything, so you can check the filter did what you expected.

> **A caveat worth reading.** The filter matches whole words. It will not catch
> a word spelled with symbols, split across a line break, or merely implied.
> Treat it as a safety net when reading unfamiliar text aloud, not a guarantee.
> Every setting draws the line somewhere different, which is why the shipped
> block list is nearly empty: fill it in with your own.

### Writing rules with full stops in them

A full stop *between* two letters is part of the word, so `e.g`, `Node.js` and
`U.S.A` all match. The stop at the *end* of "e.g." is punctuation, so write
these rules **without** the trailing dot.

---

## Another language

Every word the interface says is looked up by key, and the keys are answered by
a plain text file. English is compiled into the binary; anything else is a file
in the **languages** folder beside your settings, which the Settings tab names
and has a button to open.

Copy [`assets/lang/en.toml`](assets/lang/en.toml) — it is the reference file,
and its header explains the format — change `code`, `name` and `plural` at the
top, and translate the right of each line:

```
code   = "fr"
name   = "Français"
plural = "french"

# Open a file, choose what should happen to it, then press Apply.
general.subtitle = "Ouvrez un fichier, choisissez ce qui doit lui arriver, "
                   "puis appuyez sur Appliquer."
```

Three things follow from how it is built:

- **You do not have to finish.** Any key you have not reached falls back to
  English, so the file is useful from its first line. Settings has a **Re-read
  them** button, so the loop is edit, press, look — not edit, rebuild, restart.
- **One bad line costs one line.** A stray quote does not lose you the file;
  the Settings tab lists which lines could not be read, and why, along with any
  file in the folder that never became a language at all.
- **Placeholders are named.** `{name}`, never `{}`, so you can move an inserted
  value to wherever your grammar wants it. Do not rename them.

A file whose `code` matches a language already in the binary replaces it, which
is how a shipped translation gets improved rather than only added to. English is
the exception: it is the fallback everything else is measured against.

`ACCESSENGINE_LOCALE=fr` overrides what the computer is set to, which is the
quickest way to see a file in place without changing the whole machine.

Two things are deliberately left in English. The log file, because a line read
months later by whoever is diagnosing a fault is easier to search for in one
language; and the prompt sent to the vision model, which is a setting on the
Settings tab rather than a message — set it in whichever language you want your
image descriptions written in.

---

## Installing and running

Needs a [Rust toolchain](https://rustup.rs). On Linux you also need
speech-dispatcher for system voices (`sudo apt install speech-dispatcher`) and
the usual GUI development packages.

```sh
cargo run --release              # open the app
cargo run --release -- notes.md  # open a file straight away
cargo test                       # 51 tests
```

## Keyboard

| Key | Action |
| --- | --- |
| `Space` | Play / pause |
| `Esc` | Stop |
| `←` `→` | Previous / next sentence |
| `Ctrl`/`Cmd` + `O` | Open a file |

Pause behaves differently per engine, because the platforms do: ElevenLabs
pauses the audio, while the system engines have no pause and so restart the
current sentence when you resume.

---

## ElevenLabs

[ElevenLabs](https://elevenlabs.io) is a separate, paid text-to-speech service
known for more natural, expressive voices than most operating systems ship.
It's entirely optional — the system voices work offline with no account —
but if you want to try ElevenLabs's voices:

1. [Sign up](https://elevenlabs.io/sign-up) for an ElevenLabs account.
2. Generate an API key at
   [elevenlabs.io/app/settings/api-keys](https://elevenlabs.io/app/settings/api-keys).
3. Choose **ElevenLabs** as the speech engine on the Settings tab, then press
   the key button that appears and paste it in (or set it as an environment
   variable, below).

Each request uses your account's ElevenLabs quota and is billed by them, not
by this app — see their pricing page for current plans and limits.

### Your API key

The key is **not** saved to disk unless you tick "Remember this key on this
computer". If you do, it goes into the settings file as plain text, readable by
anything running as you, and the file is set to owner-only permissions on Unix.

On a shared machine, set the environment variable instead — it takes precedence
and never touches the config file:

```sh
export ELEVENLABS_API_KEY=sk_...
```

---

## Where things live

| | macOS | Linux | Windows |
| --- | --- | --- | --- |
| Settings, wordlists & languages | `~/Library/Application Support/org.AccessEngine.AccessEngine/` | `~/.config/accessengine/` | `%APPDATA%\AccessEngine\AccessEngine\config\` |
| Log file | same folder, `accessengine.log` | `~/.local/share/accessengine/` | `%APPDATA%\...\data\` |

The **Diagnostics** section at the foot of the Settings tab shows the exact
paths, engine capabilities, and a live tail of the log. The log folder itself
can be changed there, and the logs cleared.

## Logging

Everything the app and its dependencies emit goes to one file, flushed on every
line so a crash leaves a complete record. Panics are logged too, including from
worker threads. It rotates at 5 MB, keeping one previous generation.

```sh
ACCESSENGINE_DEBUG=1 cargo run          # the app's own debug lines
ACCESSENGINE_LOG=trace cargo run        # everything, including wgpu and winit
ACCESSENGINE_LOG=warn cargo run         # quieter
```

By default, and at `debug`, noisy dependencies (wgpu, winit, hyper and friends)
are filtered to warnings and above — otherwise a few seconds of running
produces several thousand lines of GPU adapter capabilities. Only `trace` lifts
that filter.

---

## Notes on the design

**Progress tracking is negotiated, not assumed.** The app asks the platform how
it can tell that a sentence has finished: a completion callback if the back end
has one (macOS, Windows, speech-dispatcher), otherwise polling with a guard
against the race where `is_speaking()` reports false before audio starts. If
neither exists, it queues the whole document at once and says so in the UI
rather than showing a progress bar that would be a lie.

**The speech plan is not the document.** Wordlists can drop sentences entirely,
so the list of things being spoken is tracked separately from the list of
things on screen. Skipped sentences are shown struck through, so it is visible
what the listener will not hear.

**Threading.** System speech must run on the main thread — AVSpeechSynthesizer
and SAPI both insist — so it is pumped once per frame. ElevenLabs is
network-bound and owns a worker thread. Ollama requests get their own
short-lived thread. Playback continues while the window is hidden.

## Licence

GNU General Public License, version 3 or later. The full text is in
[`LICENSE`](LICENSE); the short of it is that you may use, study, change and
share this, and anything you distribute that is built from it must come with
the same freedoms and its source.

> **A note on version 2.** GPLv2 was the first choice, but it cannot be used
> here. Nineteen crates in the dependency tree are Apache-2.0 *only*, with no
> permissive alternative offered — including ones this app cannot do without:
> `winit` (windowing), `cpal` (audio), `ab_glyph` (font rasterising), `glutin`
> and `accesskit_winit`. Apache-2.0 imposes conditions GPLv2 does not permit,
> so distributing a GPLv2 binary linked against them would be a licence
> conflict. Version 3 resolves it: it was written to be compatible with
> Apache-2.0, and it is what the sibling `watchspend` project uses.

The two asset licences below are separate from that and unaffected by it:

- **Ubuntu Bold** (`assets/fonts/`) — Ubuntu Font Licence 1.0, in
  `assets/fonts/UBUNTU-FONT-LICENCE-1.0.txt`.
- **Sound cues** (`assets/sounds/`) — two CC0 recordings from freesound.org;
  `CREDITS.txt` has the attribution and the edits made to them.

Both came from the `watchspend` project in this workspace.
