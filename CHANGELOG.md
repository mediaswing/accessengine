# Changelog

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project uses [semantic versioning](https://semver.org/).

Each released version needs a section here: the release workflow refuses to
build a tag that has none, and copies the matching section onto the release
page.

## [Unreleased]

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

[1.5.0]: https://github.com/mediaswing/accessengine/releases/tag/v1.5.0
[1.3.0]: https://github.com/mediaswing/accessengine/releases/tag/v1.3.0
[1.2.0]: https://github.com/mediaswing/accessengine/releases/tag/v1.2.0
[1.1.0]: https://github.com/mediaswing/accessengine/releases/tag/v1.1.0
[1.0.0]: https://github.com/mediaswing/accessengine/releases/tag/v1.0.0
