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

**Reads files aloud.** Open a `.txt`, `.md`, `.csv`, `.json`, `.log`, `.rst` or
`.org` file. Markdown is stripped down to prose first, so you hear the words
rather than the asterisks — code blocks are skipped, links read as their text,
and images are announced by their alt text. The document is
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
| Settings & wordlists | `~/Library/Application Support/org.AccessEngine.AccessEngine/` | `~/.config/accessengine/` | `%APPDATA%\AccessEngine\AccessEngine\config\` |
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
