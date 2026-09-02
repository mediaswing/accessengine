# The Accessibility Engine

A cross-platform desktop reader that opens a text file and reads it aloud —
with the system's own voices, or with one of five cloud voice services if you
have an account with one. It can
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

**Six speech engines.**

- *System voices* — whatever the operating system provides. Free, offline, and
  instant. macOS offers around 180 out of the box. This is the default, and
  nothing you read with it leaves your computer.
- *ElevenLabs*, *OpenAI*, *Deepgram*, *Google Cloud* and *Amazon Polly* —
  optional, each needs an account and credentials of its own, and each is
  billed by that provider. Each sentence is fetched while the previous one
  plays, so none of them stutters between sentences.

  All five behave the same way once chosen: the voice is picked on the General
  tab, how it is driven is on Settings, and the reading can be saved as an MP3.
  See **[Cloud voices](#cloud-voices)** below for what each one costs and needs.

**Plays it back.** An **Audio Player** tab with the buttons anyone expects —
play, pause, stop, previous, next, a position you can drag — so a reading you
have just saved can be listened to without leaving the app or going to find the
file. It plays MP3 and WAV.

**Reads playlists.** A zip holding a `media.xml` opens as a running order. See
**[Playlists](#playlists)** below; the short of it is that music following
speech fades up under the end of it, the way a radio bulletin does.

**Converts from the file manager.** A right-click entry — *Save as MP3 with
AccessEngine* — added from the Settings tab, using the voice and wordlists you
have already set. See **[The right-click entry](#the-right-click-entry)**.

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
cargo test                       # 208 tests
```

## Keyboard

| Key | Action |
| --- | --- |
| `Space` | Play / pause |
| `Esc` | Stop |
| `←` `→` | Previous / next sentence |
| `Ctrl`/`Cmd` + `O` | Open a file |

Pause behaves differently per engine, because the platforms do: the cloud
engines pause the audio, while the system engines have no pause and so restart
the current sentence when you resume.

---

## Cloud voices

The system voices are local, free, and need no account: they run on your own
machine and nothing you read with them is sent anywhere. Everything in this
section is optional, and only applies if you choose one of the other engines.

> **What leaving your machine means.** When a cloud engine is chosen, the text
> of whatever you are reading is sent to that provider — sentence by sentence,
> as it is read — so that they can speak it. Wordlists are applied *before*
> anything is sent, so a sentence a wordlist skips is never transmitted at all.
> Nothing else goes anywhere: this app has no telemetry, no analytics, and no
> remote logging, and it never contacts a provider you have not selected.

> **What they cost.** All five are paid services, billed by the provider and
> not by this app, per character of text synthesised. Several of them give a
> new account some trial credit — that is a trial, not a free tier. None of
> them offers permanent free text to speech. Check the provider's own pricing
> page before reading a long book to yourself.

| Engine | What you need | Where to get it |
| --- | --- | --- |
| **ElevenLabs** | An API key | [elevenlabs.io/app/settings/api-keys](https://elevenlabs.io/app/settings/api-keys) — see [ElevenLabs](#elevenlabs) |
| **OpenAI** | An API key | [platform.openai.com/api-keys](https://platform.openai.com/api-keys) — see [OpenAI](#openai) |
| **Deepgram** | An API key | Deepgram console → Settings → API Keys — see [Deepgram](#deepgram) |
| **Google Cloud** | An API key, in a project with the Cloud Text-to-Speech API enabled | See [Google Cloud](#google-cloud) |
| **Amazon Polly** | AWS credentials and a region | See [Amazon Polly](#amazon-polly) |

The same three steps set up any of them:

1. Choose the engine on the **Settings** tab.
2. Press the credentials button that appears, and paste yours in.
3. Go back to **General** and press **Fetch my voices**.

That third step is the one that tells you whether the credential works, so it
is worth doing before a long document rather than after. If a provider turns
your credential down, the button says so and offers to take another; it is on
the Settings tab whenever a cloud engine is chosen, so a credential saved with
one character wrong can always be replaced from the window.

There is a section for each below — after a note on where credentials are
kept — covering what that provider wants and what it puts on the Settings tab
that the others do not.

### Your credentials

Credentials are **not** saved to disk unless you tick "Remember these on this
computer". If you do, they go into the settings file as plain text, readable by
anything running as you, and the file is set to owner-only permissions on Unix.
This app does not use the platform keychain.

On a shared machine, set the environment variable instead — it takes precedence
over the settings file and never touches it:

```sh
export ELEVENLABS_API_KEY=sk_...
export OPENAI_API_KEY=sk-...
export DEEPGRAM_API_KEY=...
export GOOGLE_API_KEY=AIza...
```

Nothing is compiled into this app: there is no bundled key, no client secret,
and no account of ours behind any of these. Credentials never appear in the log
file — every one of them is redacted wherever it could be printed, and there
are tests that hold it to that.

### ElevenLabs

The most straightforward of the five: one key, and nothing to enable first.

1. Sign up at [elevenlabs.io/sign-up](https://elevenlabs.io/sign-up).
2. Take the key from **Settings → API Keys**
   ([elevenlabs.io/app/settings/api-keys](https://elevenlabs.io/app/settings/api-keys)).
   Unlike OpenAI's, it can be read again later.
3. Paste it in, then **Fetch my voices** — which returns the voices on your own
   account, so anything you have cloned or added from the voice library is in
   the list alongside the stock ones.

Settings adds a **model**, which is free text with a menu of the four worth
knowing about: Multilingual v2 (the default, and the best quality), Turbo v2.5,
Flash v2.5 for the lowest latency, and v3 for the most expressive. It is free
text so a model announced tomorrow can be typed in today.

It also adds the two sliders ElevenLabs itself exposes — **Stability**, where
lower is more expressive and higher more consistent, and **Similarity**, how
closely the output tracks the original recording. Both are worth a Test button
press rather than reasoning about.

### OpenAI

1. Sign up at [platform.openai.com/signup](https://platform.openai.com/signup).
   Text to speech is billed against the account like anything else on the
   platform, so it needs credit on it; a key alone is not enough.
2. Create a secret key at
   [platform.openai.com/api-keys](https://platform.openai.com/api-keys).
   **It is shown once, on creation.** Copy it then or make another later.
3. Paste it in and **Fetch my voices**.

OpenAI publishes no endpoint that lists voices — they are a fixed, documented
set, and the API rejects anything outside it — so the list comes from this app.
**Fetch my voices** still makes a request, to `/v1/models`, because that is what
actually answers the question you are pressing the button to ask: whether the
key works. A menu that filled itself in without a single request would leave a
bad key to be discovered halfway through a document.

Settings adds a **model** — GPT-4o mini TTS (the default), TTS-1 for the lowest
latency, or TTS-1 HD — and **How to read it**, a free-text instruction such as
*read slowly and clearly, as if to a class*. Only GPT-4o mini TTS listens to
that; the older two ignore it, and the app says so under the box.

The **Speed** slider is the mirror image: the older two models honour it, and
GPT-4o mini TTS — the default — does not. To change the pace of the default
model, ask for it in **How to read it** instead.

### Deepgram

1. Sign up at [console.deepgram.com/signup](https://console.deepgram.com/signup).
2. Take a key from the console, under your project's **Settings → API Keys**.
3. Paste it in and **Fetch my voices**.

The voice list is fetched from Deepgram's own `/v1/models`, so it is what your
account can actually use rather than a list baked in here that would go stale
the week Deepgram adds a voice.

Deepgram makes no distinction between a voice and a model: `aura-2-thalia-en`
*is* the voice, and it is sent as the model. So the voice picker on **General**
is the whole of the choice, and Deepgram is the one provider with no model
setting — a second control meaning the same as the first would be worse than
none. The Settings tab says as much where the model would otherwise be.

### Google Cloud

Google's own preferred credential is a *service account*: a JSON file holding
an RSA private key, which a client signs a JWT with and exchanges for an access
token every hour. This app uses the other mechanism Google supports on the same
REST endpoints — an ordinary **API key** — because a service account would mean
an RSA signer, a token cache, and a file on disk materially worse to lose than
a key, all for one provider out of five.

So what you need is:

1. A Google Cloud project with billing enabled —
   [create one](https://console.cloud.google.com/projectcreate) — with the
   **Cloud Text-to-Speech API** turned on for it.
2. An API key from **APIs & Services → Credentials**
   ([console.cloud.google.com/apis/credentials](https://console.cloud.google.com/apis/credentials))
   in that project.
3. **Restrict that key to the Cloud Text-to-Speech API.** An unrestricted Cloud
   API key is a key to the whole project; this one only ever needs to speak.

Then paste it in and press **Fetch my voices** as with any other engine.

Google offers well over a thousand voices, so the voice picker gains a filter
box. Each is labelled with its language, its gender where stated, and its
family — Standard, WaveNet, Neural2, Chirp — which is the biggest difference in
both how it sounds and what it costs.

### Amazon Polly

Polly is the one provider whose credential is a set rather than a single
string. You need an AWS account
([sign up](https://portal.aws.amazon.com/billing/signup)) and an access key
from **IAM → Security credentials**
([console](https://console.aws.amazon.com/iam/home#/security_credentials)).

Rather than invent anything, this app asks the standard AWS chain, in order:

1. `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and optionally
   `AWS_SESSION_TOKEN` from the environment;
2. an access key ID and secret access key typed into this app;
3. the profile in `~/.aws/credentials` — the file the AWS CLI writes, so a
   machine already set up for `aws` needs nothing entered here at all. Which
   profile is a setting.

So on a machine where `aws` already works, there is nothing to enter: choose
Amazon Polly and press **Fetch my voices**.

The **AWS region** is typed rather than chosen, because AWS adds regions faster
than any list here could be updated; the arrow beside it offers the common
ones. Leave it blank and the same kind of chain applies — `AWS_REGION`, then
`AWS_DEFAULT_REGION`, then `~/.aws/config`, then `eu-west-2` — and the field
shows what that has come to as its hint. Pick the region your Polly quota is
in: asking for a voice in one you have not been granted fails per sentence.

The credentials want the **AmazonPollyReadOnlyAccess** policy, which is enough
to list voices and speak, and nothing else.

Polly is also the one provider that offers a choice of synthesis engine —
generative, neural, long-form or standard — and not every voice supports every
one. The Settings tab narrows that list to the engines the chosen voice can
actually be spoken by, and choosing a voice that cannot use the current engine
moves the engine rather than leaving the pair broken.

Polly will not read more than 3000 characters in one request. Reading in
sentences never comes close; reading in paragraphs occasionally can, and the
app says so and names the setting rather than failing obscurely.

### Saving to MP3

**Save the reading as an MP3** works with any of the five, because every one of
them is asked for MP3 and the segments are simply appended. It does not work
with the system voices: the platform engines speak to the sound card and offer
no way to capture what they produce, so there is nothing to write.

An export is one paid request per sentence, so a long document is confirmed
first with an estimate of how long it will take.

---

## Playlists

A zip containing a `media.xml` opens as a running order rather than as an
archive. The manifest is small:

```xml
<media version="1.2">
    <content pos="1" type="M">bed.mp3</content>
    <content pos="2" type="B">bulletin.mp3</content>
    <content pos="3" type="M">outro.mp3</content>
</media>
```

- **`pos`** is the running order. It is read from the attribute, not from the
  order the elements happen to appear in — the two are not promised to agree,
  and this is the one that says what it means.
- **`type`** is `M` for music or `B` for spoken word.
- The audio files live in the same zip. They may sit in a folder; the manifest
  need not repeat the path, and a difference in capitalisation is forgiven.

**Where music follows speech, it fades up under the end of the speech** over
about three seconds, rather than starting flat afterwards. That is the one join
treated specially, and it is what a bulletin sounds like on the radio. Every
other join is a straight cut, on purpose: a voice fading in loses its first
words, and two spoken items should never overlap at all.

A track named in the running order but missing from the zip costs that track
and no others — the rest still plays. A zip with no manifest in it is not a
playlist and is not opened as one.

Only MP3 and WAV can be played, which are the formats this app itself writes.

---

## The right-click entry

The Settings tab can add **Save as MP3 with AccessEngine** to the menu you get
when you right-click a document. It converts using the settings already in the
app, without opening the window.

Two things go on disk:

1. **A script**, in the same folder as your settings — `%APPDATA%` on Windows,
   `~/Library/Application Support` on macOS, `~/.config` on Linux. It is three
   lines and all it does is call this app with `--convert`.
2. **A registration**, which differs by platform:

| | Where it appears |
| --- | --- |
| Windows | File Explorer's right-click menu, on the document types this app reads |
| macOS | A Finder Quick Action, under **Quick Actions** or **Services** |
| Linux | A Nautilus script, under **Scripts**. Other file managers are not covered |

**The script holds no settings and no credentials.** It calls the app, and the
app reads its own `config.json` — so the engine, the voice, your wordlists and
the chunking are whatever you last set in the window, and changing them there
changes what the entry does with nothing to reinstall.

Converting needs one of the cloud engines, for the same reason saving audio
does. With the system voices chosen, the entry says so rather than quietly
writing nothing.

The app's location is baked into the script when you install it, since that is
the one thing the script cannot look up. **Move or reinstall the app and you
will need to add the entry again**; the Settings tab says so too.

### The command line

The entry runs the same thing you can run yourself:

```sh
accessengine --convert notes.md            # writes notes.mp3 beside it
accessengine --convert notes.md -o out.mp3 # somewhere else
accessengine --help
```

One verb, deliberately, and it does exactly what pressing Apply with **Save the
reading as an MP3** does — the same reader, the same voice, and the same
wordlists. That last part is the one that matters: a word a safety list keeps
out of a reading stays out of a file converted this way.

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
and SAPI both insist — so it is pumped once per frame. The cloud engines are
network-bound and share one worker thread. Ollama requests get their own
short-lived thread. Playback continues while the window is hidden.

**One cloud worker, five providers.** The queue, the audio device, the
one-sentence-ahead prefetch and the handling of a fresh play arriving
mid-sentence are the same work whoever is being asked, so they live in
`speech::cloud` and each provider's module holds only the two requests it
actually makes — synthesise a chunk, list the voices. Adding a sixth provider
is one file. Provider-specific settings are kept provider-specific rather than
flattened into a common denominator: Polly really does have an engine choice
that OpenAI does not, and pretending otherwise would lose it.

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
