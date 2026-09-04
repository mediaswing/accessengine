# Changelog

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project uses [semantic versioning](https://semver.org/).

Each released version needs a section here: the release workflow refuses to
build a tag that has none, and copies the matching section onto the release
page.

## [2.0.0] - 2026-09-04

### Added

- **Word documents.** `.docx` and the `.doc` that came before it open, along
  with `.docm` and the `.dot`, `.dotx` and `.dotm` templates. A table in a
  document is read as a table, every figure under the name of its column, the
  same way one on a slide already was.

  Two things are deliberately not read. Text someone deleted with track changes
  on was not on the page the author saw, and neither was the instruction behind
  a field: you hear "our site", not `HYPERLINK "http://example.com"`. Headers,
  footers, footnotes and comments are left out for the reason master slides
  are — a header repeats on every page, and "Confidential — page 3 of 40"
  between every paragraph would be worse than nothing.

  A `.doc` is not stored in reading order. Word wrote an edit by appending it
  and adjusting a table that says what order the pieces go back in, so a
  heavily edited document is thoroughly out of sequence in the file itself.
  That table is followed rather than guessed at, which is the difference
  between a document that reads correctly and one that reads in the order it
  was typed. Its eight-bit text is decoded as Windows-1252 rather than
  Latin-1, which is where Word's autocorrect keeps its curly quotes, its
  ellipsis and its em dash.

  A document saved with a password says so instead of reading out the
  ciphertext. One from Word 95 or earlier says so too, and asks to be re-saved,
  rather than reading noise out of a header it does not understand.

- **PDFs.** The words come out in reading order, and the lines of a paragraph
  are joined back into one so the pause falls at the end of a sentence rather
  than at the end of every line of type. A word hyphenated across two lines is
  put back together.

  A PDF with no text layer — a scan, which is a picture of a page rather than
  the words on it — says so, and points at describing the pages as images or
  running the file through OCR first. "This PDF is empty" would send somebody
  looking for a fault in a file that is exactly as its author left it.

  This is the one format the app does not read itself, and
  [`pdf-extract`](https://crates.io/crates/pdf-extract) is the first dependency
  here taken on for something this app could have written. It could not have
  written it well: an object parser, cross reference streams, compression
  filters, and every font's own private mapping from byte to character through
  `ToUnicode` maps and `Differences` arrays. Written partially that does not
  give partial output, it gives confident nonsense — and confident nonsense
  read aloud to somebody who cannot see the page is the worst thing this app
  could do. It costs nineteen crates and brings no async runtime and no second
  HTTP stack, which is what ruled out `aws-sdk-polly` back when Polly was
  written by hand. A damaged PDF is refused rather than taking the application
  down with it.

### Changed

- **The right-click entry now offers itself on the new file types.** *Save as
  MP3 with AccessEngine* is registered per extension, against the list of what
  this app can read, so that it appears on the documents it can actually open
  and nowhere else — and that list has just grown by seven. On Windows it is a
  registry key under `SystemFileAssociations` for each one; on macOS a Finder
  Quick Action; on Linux a desktop entry.

  This is the one change here that reaches outside the application, and it does
  not apply itself: the keys are written when the entry is switched on in
  Settings, and nothing rewrites them on launch or on upgrade. Anyone who
  already had the entry before this release keeps the old list until they turn
  it off and on again.

- The reader that understands legacy Office containers moved out of
  `powerpoint` into `cfb`, since a `.doc` and a `.ppt` differ entirely in what
  their streams hold and not at all in how those streams are found. The XML
  entity decoder and the table builder moved likewise, out of `powerpoint` and
  into `xml` and `document`, where the presentation, document and playlist
  readers can all reach them. No behaviour changed with them.

## [1.9.0] - 2026-09-03

### Added

- **An Audio Player tab.** A plain transport for listening back to what the app
  has just made: play, pause, stop, previous and next, a position you can drag,
  and a volume of its own. A reading that has just been saved is offered by
  name rather than having to be gone and found. It is deliberately not a second
  reader — the audio is finished by the time it arrives here, so there is no
  plan, no highlighting and no wordlist, only a running order and a playhead.

  It plays MP3 and WAV, which are the two formats this app itself produces and
  the two its decoder is built with. Anything else is refused by name in the
  file dialog rather than opened and then failing.

- **Playlists.** A zip with a `media.xml` inside it opens as a running order.
  Each `<content>` names a file, `pos` gives the order — read from the
  attribute rather than from the order the elements happen to appear in, since
  the two are not promised to agree — and `type` says whether it is music (`M`)
  or spoken word (`B`).

  That last distinction earns its keep at one join: **where music follows
  speech it fades up under the end of the speech** rather than starting flat
  after it, which is what a bulletin sounds like on the radio. Every other join
  is a straight cut, deliberately — a voice fading in loses its first words,
  and two spoken items must never overlap at all. A track the running order
  names but the zip does not contain costs that track and no others.

  A zip is only treated as a playlist if there is a manifest inside it, so a
  zip of holiday photographs is still just a zip.

- **A right-click menu entry**, added from the Settings tab: *Save as MP3 with
  AccessEngine*, on the documents this app can read. It writes a three-line
  script into the app's own settings folder — `%APPDATA%` on Windows,
  `~/Library/Application Support` on macOS, `~/.config` on Linux — and
  registers it: File Explorer's menu on Windows, a Finder Quick Action on
  macOS, a Nautilus script on Linux. Other Linux file managers are not covered,
  and the tab says so.

  The script carries no settings and no credentials. It calls the app, and the
  app reads its own `config.json` — so the engine, the voice, the wordlists and
  the chunking are whatever the window was last set to, and changing them
  changes what the entry does with nothing to reinstall.

- **A command line for that entry to use.** `accessengine --convert FILE`
  writes an MP3 beside the file and never opens a window; `--out` puts it
  somewhere else. It is one verb rather than a general interface, and it does
  exactly what pressing Apply with **Save the reading as an MP3** does — the
  same reader, the same voice, and the same wordlists, which is the part that
  matters: a word a safety list keeps out of a reading stays out of a file
  converted without the window ever opening. It needs a cloud engine, and says
  so plainly when there is not one, or no credentials, or no voice chosen.

- Four more speech engines, alongside the system voices and ElevenLabs:
  **OpenAI**, **Deepgram**, **Google Cloud** and **Amazon Polly**. They are
  chosen the same way ElevenLabs already was — a **Speech engine** dropdown on
  Settings — and once chosen they behave the same way: the voice is picked on
  the General tab, everything about how that voice is driven is on Settings,
  the sentence being spoken is highlighted, play, pause, stop and the arrow
  keys work, and the wordlists have had their say before a single word is sent
  anywhere.

  They also all save. **Save the reading as an MP3** used to be ElevenLabs
  only, because that was the one engine that hands back a file rather than a
  noise; every one of these does the same, so the export now works with any of
  them. The system voices still cannot be recorded, and the button still says
  so.

  Each provider keeps its own voice, so trying Deepgram for an afternoon and
  going back to ElevenLabs finds the ElevenLabs voice exactly where it was
  left. A voice that has since been removed or renamed is named in the box and
  said out loud above it, rather than the selection quietly emptying.

- **Where the credentials go.** Each provider has its own key, entered in the
  same dialog as before and stored under the same rule: nothing is written to
  disk unless **Remember these on this computer** is ticked, the settings file
  is owner-only on Unix, and an environment variable — `OPENAI_API_KEY`,
  `DEEPGRAM_API_KEY`, `GOOGLE_API_KEY`, and `ELEVENLABS_API_KEY` as before —
  takes precedence over the file and never touches it. The dialog says where in
  each provider's own console the credential actually is, which is five
  different answers.

  Amazon Polly is the exception, because AWS credentials are a set rather than
  a string. It uses the standard chain: the `AWS_*` environment variables, then
  anything typed into this app, then the profile in `~/.aws/credentials` that
  the AWS CLI writes — so a machine already set up for `aws` needs nothing
  entered here at all. Requests are signed with Signature Version 4 in about
  eighty lines rather than by pulling in the AWS SDK.

  Google Cloud uses an API key rather than a service account, and the README
  explains why: a service account would mean an RSA signer, a token cache, and
  a secret on disk materially worse to lose than a key.

### Changed

- Building the speech plan — running the wordlists over a document to work out
  what will actually be spoken — has moved out of the interface, because the
  command line has to reach the same answer and two copies of that loop would
  eventually disagree about which words get said out loud.

- The ElevenLabs worker is now the worker for all five cloud engines. The
  queue, the audio device, the one-sentence-ahead prefetch and the handling of
  a fresh play arriving mid-sentence were never particular to ElevenLabs, so
  they have moved to `speech::cloud` unchanged, and each provider's own module
  is left holding the two requests it actually makes. ElevenLabs behaves
  exactly as it did, down to the wording of its error messages, which a test
  now holds it to.

- Errors from a cloud provider say what to do rather than quoting a status
  code. A refused key names the tab to enter another on, a rate limit says to
  wait, and a provider having a bad day says so — for all five, from one place,
  so a provider added later cannot forget to.

- The **Diagnostics** section names the cloud engine in use, the voice, and
  whether its credentials came from the environment or the settings file.

- A settings file naming a speech engine this build has never heard of falls
  back to the system voices instead of being discarded whole. That is what a
  downgrade looks like — settings written by a newer version, opened by an
  older one — and it used to cost every other setting in the file.

### Fixed

- A settings file written before 1.3 spells its engine in snake_case
  (`eleven_labs`), and nothing since has recognised that spelling: every launch
  quietly reset the engine to the system voices and mentioned it only in the
  log. The old spellings are read back as the engine they name, and the next
  save rewrites them in the current form.

- Apply is no longer a dead button with its reason hidden. Saving audio needs a
  cloud engine, so the reset above left it permanently disabled — and the
  explanation was hover text on a greyed-out button, which is nowhere a screen
  reader lands. Whatever is blocking Apply is now written underneath it.

- The list of local vision models failed to load whenever Ollama reported a
  model carrying no family metadata, which it sends as `"families": null`
  rather than as an empty list. One such model emptied the whole picker.

### Note on cost

Only the system voices are free, and they are the default. The four new
engines are all paid services billed by the provider, not by this app. Some
offer trial credit to a new account; none of them has a permanent free tier for
text to speech. Every one of them receives the text of whatever is being read.

## [1.6.0] - 2026-09-02

### Added
- The interface can be read in another language. Every word it says is now
  looked up by key rather than written in place, and the keys are answered by a
  plain text file: adding a language is a file dropped in the **languages**
  folder beside the settings, not a rebuild. English is compiled into the
  binary and is the fallback for every key a translation has not reached yet,
  so a file is useful from its first line rather than only its last.

  Settings gains a **Language** picker — following the computer, or a language
  by name — and a **Re-read them** button, so a translator's loop is edit,
  press, look. Anything wrong with a file is said on that tab rather than
  logged where nobody will see it: which line could not be read and why, and
  which files in the folder never became a language at all. Placeholders are
  named rather than positional, so an inserted value can move to wherever a
  language's grammar wants it, and a test holds every translation to the same
  set of them as English. `ACCESSENGINE_LOCALE` overrides what the computer is
  set to.

  Ported from the `watchspend` project in this workspace, which has been doing
  this for its own interface.

- PowerPoint presentations can be read. Both formats open — `.pptx`, and the
  `.ppt` that came before it, along with `.pptm`, `.pps` and `.ppsx` — and
  which one a file is is decided by what is inside it rather than by its name,
  so a `.pptx` that arrived renamed to `.ppt` still opens.

  Each slide is announced by its number, and a slide with no text on it says so
  rather than being skipped: someone following along by ear is counting, and a
  deck whose fourth slide is one photograph should not renumber the fifth.
  Master slides and layouts are left out, because their placeholder text is
  "Click to edit Master title style". Speaker notes are not read. A
  password-protected presentation says so rather than reading out ciphertext,
  and a presentation whose slides would expand to more than 64 MB of XML is
  refused rather than allowed to fill memory.

### Changed

- A table on a PowerPoint slide is read as a table rather than as a handful of
  unrelated words. `Region · Sales · North · 1,200` used to arrive as four
  separate paragraphs; it now reads "A table of 1 row and 2 columns: Region,
  Sales. Row 1. Region: North. Sales: 1,200." — the same sentence a `.csv`
  produces, from literally the same code, so the two formats cannot drift
  apart. A cell holding several paragraphs stays one cell, and an empty cell is
  left out without shifting the values after it into the wrong column.

  The words around a table and around a slide — "Row", "Column", "Slide 3. No
  text on this slide." — have moved into the language file with the rest of the
  interface. They are spoken aloud into somebody's document, so leaving them in
  English while the app was in French made no sense; and putting them there
  fixed the grammar of "A table of 1 rows", which a two-row slide table hit
  every time.
- A CSV is read as a table rather than as lines of text. Each row is announced
  by number, and every value is spoken under the name of its column — the way
  a screen reader reads a table, and for the same reason: by the fourth row
  nobody is still holding the header line in their head. `Alice,30,Leeds`
  becomes "Row 1. Name: Alice. Age: 30. City: Leeds." rather than "Alice
  thirty Leeds".

  The delimiter is sniffed rather than assumed, so a `.csv` saved by a
  spreadsheet where the comma is the decimal point — semicolons — or dumped
  out of a database — tabs — reads correctly; the judgement is which
  delimiter gives every row the same number of fields, so a prose column full
  of commas cannot outvote the real one. Quoted fields keep the delimiters,
  line breaks and doubled quotes inside them. A first row that is all numbers
  is taken as data rather than as column names. Empty cells are left out.

### Fixed

- **Accessibility.** Every primary button in the app is drawn with a symbol in
  front of its words — "📂  Open a file…" — and a screen reader was announcing
  the symbol first: "open file folder Open a file". The symbol is now left out
  of the spoken name, so a listener hears the sentence a sighted user reads.
  The two symbols in the wordlist changes panel are read as what they mean
  ("becomes", "3 times") rather than as the names of their characters, and the
  image preview carries the model's own description as its alternative text
  once there is one.
- **Accessibility.** The status line at the foot of the window is now a live
  region. Everything the app has to say about what just happened arrives there,
  and nothing takes focus to say it, so for anyone reading the window by ear it
  was previously only read when they went looking for it.
- **Security.** A CSV expands as it is read — every value gains the name of its
  column — by as much as twenty times. A 64 MB file, which the reader accepts,
  could therefore turn into well over a gigabyte of text and freeze the window.
  What a table expands to is now held to the same 64 MB ceiling as the file it
  came from, and says which row it stopped at.
- **Security.** A `.pptx` counted its slides against its memory budget by the
  size each one *claimed* in the zip directory. An archive declaring every
  slide empty and then handing over eight megabytes of each would never reach
  the budget at all. The budget now counts what was actually read.
- **Security.** A `.ppt` whose sector table pointed in a circle was followed a
  quarter of a million times, turning a kilobyte of malformed input into a
  hundred megabytes of output. A chain is now cut at the number of sectors the
  file actually has. Sector arithmetic is checked rather than plain, so a
  32-bit build cannot be made to panic on a large sector number; and a stream
  that is present but unreadable now says so, rather than reporting itself as
  a stream that was never there.
- Three characters in the interface had no glyph in any bundled font and were
  drawn as empty boxes: the arrows in the keyboard shortcuts table, and the
  full-width plus on "Install a wordlist". The arrows now read as words, which
  a screen reader announces sensibly as well. A test now checks every character
  of every shipped language against the fonts the app installs, so the next one
  is caught before it ships rather than by whoever cannot read it.

### Removed
- HTML is no longer read. `.html`, `.htm`, `.xhtml` and `.xml` have gone from
  the file picker, and a page handed to the app another way — on the command
  line, or through the dialog's "All files" — is turned away by name rather
  than read out tag by tag. The stripper it went through was a few hundred
  lines of guesswork about a format that has no small correct subset: it
  understood a handful of block elements and six entities, and everything else
  in a real page — tables, `<template>` bodies, `<svg>` text, character
  references beyond those six — was read aloud or silently dropped. Save the
  page as plain text or markdown instead.

## [1.5.0] - 2026-09-02

### Changed

- The top toolbar — "Open file", Play/Pause, Stop, previous/next sentence, and
  the progress bar beneath it — has been removed. Opening a file and starting
  playback were already duplicated on the General tab's "Open a file…" and
  Apply buttons; pause, stop and skip remain reachable via Space/Ctrl+P,
  Escape, and the arrow keys.
- The General/Wordlists/Settings tab strip now runs the full width of the
  window at the very top, directly under the title bar, rather than sitting
  above just the side panel.

## [1.3.0] - 2026-09-02

### Added

- A reading can be saved as an MP3. The toolbar's "Save as MP3" writes what
  would actually be spoken — after the wordlists have had their say — to a
  file. Only the ElevenLabs engine can do this: it returns MP3 already, so an
  export is the same requests playback makes, written to a file rather than to
  the sound card. The system voices cannot be recorded at all, because the
  speech back ends play to the audio device and offer no way to capture what
  they produce, so the button explains that rather than quietly doing something
  else. A save expected to take half a minute or more asks first, quoting a
  time measured from the round trips the app has already made.
- "Open file" now accepts images as well as documents. An image opens on the
  Image tab and is described straight away, with no further button to press:
  the app asks Ollama what is installed — starting it if it is not running —
  and uses the first model that can read images. Previously nothing asked until
  the refresh button was found, so the model menu sat empty and "Describe"
  failed with a request to choose a model that was not there to choose. The
  finished description arrives as a dialog offering to read it aloud or save it
  as a text file.
- The place a photograph was taken is now named. A geotagged image previously
  ended its description with its coordinates read out as numbers; the
  coordinates — never the image — are now looked up with OpenStreetMap, and the
  description ends with something like "Taken in Moseley, Birmingham, United
  Kingdom." If the lookup fails or finds nothing, it says the place could not be
  looked up.
- A text-size setting, and a light/dark/system theme choice, both in
  Diagnostics. Ctrl/Cmd with plus and minus does the same as the slider.
- Every keyboard shortcut is listed in Diagnostics, and there are now chorded
  forms — Ctrl/Cmd with P, and with the arrow keys — that work whatever has
  focus.

### Changed

- The Image tab is settings and a preview. The button for loading an image, the
  "Describe" button, the description text box and the row of buttons under it
  are gone: opening an image is the whole instruction, and the description
  presents itself when it is ready.
- Logs are one file per day, rolling over at midnight and kept for a
  fortnight. An existing `accessengine.log` is moved into the new `logs`
  folder under the date it was last written.
- The interface follows the `watchspend` design: its palette, with both themes
  stated explicitly and every meaningful colour measured against the surface it
  is drawn on, its spacing, and a tab strip of equal-width buttons.

### Fixed

- Opening a large document with few paragraph or sentence breaks froze the
  window, for minutes in the worst case. Splitting the text into chunks counted
  the whole of the remaining document once per chunk, which is quadratic; an
  8 MB paragraph took nearly two seconds, and the 64 MB the reader accepts
  would have taken well over a minute.
- Screen readers announced the transport buttons by the name of their symbol —
  "black right-pointing double triangle" for the skip button — and every text
  field and drop-down as an unnamed box, because the captions beside them were
  never tied to them. Both are fixed throughout.
- Keyboard shortcuts stopped working as soon as anything had focus, which is
  the normal state after a single press of Tab: Space, Escape, the arrows and
  Ctrl/Cmd + O all became inert with nothing to say why.
- Warnings were drawn in a colour that fell below the contrast floor the rest
  of the interface meets, on the light theme. Status messages relied on colour
  alone to separate a success from a failure, which says nothing to a screen
  reader; errors now say so in words.
- The document pane put every sentence in the tab order — several hundred stops
  on a long document. It is now a single stop, with the arrow keys moving
  through the text and Enter reading from the sentence in focus.
- Hints that only appeared on hover, including the explanation of what
  "Remember this key" does with an API key, are now on screen. Disabled buttons
  explain why they are disabled, which is when the explanation is most wanted.
- Adding a wordlist that shares a name with an installed one silently replaced
  it. It now refuses and says so.
- An HTML comment containing a `>` had its tail read aloud.

### Security

- The Image tab's promise that nothing leaves the machine is now conditional on
  the Ollama address actually being on this machine, and says where the image
  is going, and whether unencrypted, when it is not.
- Starting Ollama prefers the absolute paths the official installers use, with
  a bare program name only as a last resort: a bare name is resolved by a
  search this app does not control, which on Windows can include the directory
  the app was launched from.
- Release URLs from the update check are only handed to the platform opener if
  they are plainly `http` or `https`.
- HEIC conversion writes into a directory created for that one conversion,
  rather than to a predictable path in a shared temporary directory.

## [1.2.0] - 2026-09-02

### Added

- A startup check for a newer release. If GitHub has a tag newer than the
  running version, a dialog offers a link to the release page — nothing is
  downloaded or replaces the running binary automatically, since the Windows
  and Linux builds are not code-signed. Can be turned off, or triggered
  on demand, from the Diagnostics tab.

### Fixed

- Image previews never loaded on Windows: the `file://` URI built for the
  preview was missing the third slash a Windows drive-letter path needs
  (`file:///C:/...`), so `egui`'s file loader read it as a UNC network path
  instead and failed. macOS and Linux were unaffected, because a Unix path
  already starts with `/` and supplied that slash by accident.
- Opening a plain text file saved with Windows line endings left a stray
  carriage return in the middle of a wrapped sentence, wherever a paragraph
  spanned more than one line in the source file — visible in the document
  pane and audible as a stray pause on back ends that read punctuation.
  Line endings are now normalised to `\n` on load.
- Added `.gitattributes` marking the bundled font and sound files as
  binary. Without it, a Windows checkout with the common `core.autocrlf`
  setting enabled has no guarantee `git` correctly detects a compact WAV
  encoding as binary, and could silently rewrite its bytes on checkout.

## [1.1.0] - 2026-09-01

### Added

- macOS now ships as `AccessEngine.app` rather than a bare executable.
  Double-clicking the old download had nothing macOS could launch it with, so
  it opened Terminal instead of the app.
- HEIC and HEIF photos can be described. Ollama decodes images with stb_image,
  which has no HEIF support, so on macOS they are converted to JPEG with `sips`
  first. Other platforms explain the limitation and suggest exporting as JPEG.
- A geotagged photo's location is read out at the end of its description.
  Coordinates rather than a place name, because resolving one to the other
  would mean sending the location to a geocoding service.

### Fixed

- Changing a wordlist while a document was being read did not affect what was
  actually spoken. The ElevenLabs worker holds its own copy of the plan, so
  enabling the classroom-safe list mid-read went on speaking the unfiltered
  text to the end of the document. It now restarts from the current sentence
  with the new text, including after a change made while paused.
- Testing an ElevenLabs voice behaved as though the document were playing: the
  progress bar jumped to sentence one and the sample's end announced "Finished
  reading." Samples no longer report document progress, and stop any real
  playback first.
- Skipping repeatedly during ElevenLabs playback nested playback inside itself
  once per key press, holding on to every superseded document until playback
  ended. Superseding commands are now handled iteratively.
- HEIC images are no longer previewed through a decoder that cannot read them,
  which showed a load failure beside a working Describe button.

## [1.0.0] - 2026-09-01

### Added

- First release: a cross-platform reader that opens a text file and speaks it
  with system voices or ElevenLabs, describes images with a local vision model
  through Ollama, and applies editable wordlists for safety and pronunciation.
- Release workflow building Linux, macOS and Windows binaries, publishing them
  with checksums, and submitting them to VirusTotal.

[1.9.0]: https://github.com/mediaswing/accessengine/releases/tag/v1.9.0
[1.6.0]: https://github.com/mediaswing/accessengine/releases/tag/v1.6.0
[1.5.0]: https://github.com/mediaswing/accessengine/releases/tag/v1.5.0
[1.3.0]: https://github.com/mediaswing/accessengine/releases/tag/v1.3.0
[1.2.0]: https://github.com/mediaswing/accessengine/releases/tag/v1.2.0
[1.1.0]: https://github.com/mediaswing/accessengine/releases/tag/v1.1.0
[1.0.0]: https://github.com/mediaswing/accessengine/releases/tag/v1.0.0
