# Changelog

What changed in each release, written for someone deciding whether to update
rather than for someone reading the diff.

## [1.2.5] - 2026-08-09

### Added

- **A newer version now shows its changelog, not just a link.** The startup
  check for a newer release opens a window with what changed in it, and two
  buttons: **Download Release**, which goes straight to the build for this
  platform, and **Not Right Now**, which dismisses it for the session.
  Previously all this gave you was a small link to the releases page under
  the header.

### Fixed

- **Non-ASCII characters in an API key, or in a system voice's name, were
  silently corrupted on Windows.** The app talks to PowerShell over standard
  input and output for both, and Windows reads and writes those streams
  through a legacy code page rather than UTF-8 whenever there is no console
  window attached — which is always, for the windowless PowerShell this app
  runs. An ElevenLabs key with anything but plain ASCII in it could fail to
  save with "the key was not stored correctly," and an installed voice with
  an accented name could show up wrong in the voice picker. Both streams are
  now explicitly read and written as UTF-8.

- **Text and CSV files saved as "ANSI" read out as replacement characters
  instead of accented letters.** Excel's plain "CSV (Comma delimited)"
  export, and Notepad before Windows 10's 1809 update, both write
  Windows-1252 rather than UTF-8 — a name like "José" came out with the
  accent replaced by a stray placeholder glyph instead of being read
  correctly. A file that isn't valid UTF-8 is now read as Windows-1252
  instead of losing the accented characters.

## [1.2.3] - 2026-08-09

### Added

- **A reset button.** **Settings** now has **Reset All Settings To Defaults**,
  which puts the speech engine, voice, speaking rate, action, audio format,
  vision model and image prompt back to how the app arrives — useful if a
  setting has been changed to something that no longer works and it is not
  obvious which one. It asks first, and says plainly what it will and will not
  touch: your dictionary is kept, and so is your ElevenLabs API key.

- **A diagnostics log, and a button to copy it.** The app now keeps a record of
  what it did during each session — the files it opened, the vision models it
  called and what they answered, how long each step took. **Settings** has a
  **Copy Diagnostics To Clipboard** button that puts the whole thing on the
  clipboard, ready to paste into a bug report, so nobody has to go hunting for a
  file to report a problem. The log starts fresh each time the app opens and the
  previous session's is kept alongside it, so restarting to have another go
  doesn't destroy the evidence of what went wrong the first time. It records
  what the app *did*, not what you read: the text of your documents never goes
  in, only how much of it there was, and anything shaped like an API key is
  scrubbed out. On macOS it is in `~/Library/Logs/accessengine`, where Console
  finds it on its own.

### Fixed

- **The ElevenLabs API key was not being saved.** Entering a key worked for as
  long as the app stayed open, and was gone the next time it started — the app
  said "API key saved" and had in fact saved nothing. Saving a password to the
  keychain means answering two prompts, the password and a confirmation, and the
  app only ever answered the first; the keychain then stored an empty password
  and reported success. Anyone who launched the app from a terminal saw this as
  a stray password prompt appearing in the terminal window. The key is now
  stored properly, and read back and checked before the app claims to have saved
  it, so a failure can no longer be silent. Enter your key once more and it will
  stay.

- **Photographs of places were answered with a road sign.** A photo of a city
  square came back as the words "One Way" and nothing else — no description of
  the buildings, the crossing or the trees. The app asked the vision model to
  transcribe the text in an image and to describe the picture *only if it
  contained no text*, and almost every real photograph contains some text
  somewhere: a shop front, a number plate, a road sign. One incidental sign was
  enough to suppress the description entirely. Photos are now always described,
  with any text in them read out afterwards, while a photographed page or
  screenshot is still transcribed in full and without a preamble. If you were
  using the standard prompt it is updated for you; a prompt you wrote yourself
  is left alone.

- **Photographed text was read out as gibberish.** A photo of a page came back
  as invented words — `L1: 2 l1s q4k b3wv f0 rjzrsw0` — so the only intelligible
  thing in the audio was the line saying where the photo was taken. Photos were
  sent at full resolution, and Ollama's own shrinking of them to fit the vision
  model turned small text to mush; a vision model handed mush does not say it
  cannot read the image, it invents text that looks about right. Photos are now
  shrunk before they are sent, carefully enough to stay legible. This costs the
  model exactly the same to read as before, so nothing is slower, and the same
  photo that produced nonsense now transcribes correctly. HEIC photos from an
  iPhone were the worst affected.

- **Sideways photos are turned upright.** A photo taken with the phone rotated
  is stored on its side with a note to turn it, which was lost on the way to the
  vision model — so it was asked to read a page lying on its side. The rotation
  is now applied to the picture itself before it is sent.

## [1.2.2] - 2026-08-08

### Fixed

- **Photos were read out as a single syllable.** Any image much larger than
  about 1500 pixels — which is every photo a phone takes — came back described
  as one letter, followed by wherever it was taken. The image filled the vision
  model's entire context window, leaving it room for exactly one word of answer,
  and that word was what got spoken. The model is now given enough room to
  answer in, and a photo that still runs out of room says so plainly instead of
  reading out a fragment. Nothing needs reinstalling or re-downloading; the same
  photo now reads properly.

### Added

- **Spreadsheets are read as tables.** Open a `.csv` or `.tsv` and it is read
  the way a table has to be heard to make sense: how big it is — "table with 12
  rows and 4 columns" — and then every value under the heading it sits below,
  "date of birth: 10 December 1815", rather than a bare list of values nobody
  can keep count of. Underscores in headings become spaces, so a column called
  `date_of_birth` is spoken as words. Values containing commas, quotes or line
  breaks are read as the single values they are, and files separated by
  semicolons or tabs are recognised as well as commas.

## [1.2.0] - 2026-08-08

### Added

- **Homebrew can be installed from inside the app** (macOS). Reading an image
  needs Ollama, and installing Ollama needs Homebrew — so a Mac with neither
  used to be handed a link to brew.sh and left to it. The app now offers to
  install Homebrew itself, running Homebrew's own installer and showing what it
  is doing as it goes. That installer needs administrator access, so a dialog
  asks for your Mac password once; it goes straight to macOS and is not stored
  anywhere, and dismissing it cancels the install rather than failing obscurely.
- **Where a photo was taken.** A photo that carries a GPS position in its EXIF
  data now has the place named at the end of its description — "Taken in
  Sheffield, England, United Kingdom". The coordinate is all that leaves this
  computer, to OpenStreetMap: no image, no filename, no account. A photo with no
  location tag, or a lookup that doesn't answer, reads exactly as it did before.
- **A new version says so.** One quiet check against the latest published
  release when the app starts, compared to the version you are running. If
  there is a newer one, a link to the release page appears under the header.
  Nothing is downloaded or installed for you.

## [1.1.0] - 2026-08-08

### Fixed

- **Images could not be read at all.** Ollama removed the model runner that
  `llama3.2-vision` was built for, so the vision model this app defaulted to
  stopped loading — after a 7.8 GB download that could never work. The default
  is now `qwen2.5vl:3b`: about 3 GB, and better at reading text off a page.
  If you had the old model saved, it is swapped for the new one automatically;
  you can reclaim the disk space with `ollama rm llama3.2-vision`.
- A failure to load a vision model now explains itself and points at the
  setting that fixes it, instead of repeating Ollama's own wording — which
  reads as a crash when the fix is one dropdown away.

### Added

- **Audio Player.** A new pane, <kbd>⌘2</kbd>, for playing back a spoken-word
  WAV or MP3 — one this app saved, or an audiobook chapter from anywhere else.
  Play, pause, stop, and a ten-second rewind for the sentence you missed, with
  the position read out in words. Drop an audio file anywhere on the window and
  it lands there. <kbd>⌘P</kbd> plays or pauses and <kbd>⌘R</kbd> skips back,
  from any pane.
- **A vision model picker** in Settings, listing models that work with current
  Ollama alongside their download sizes, so choosing one is not a research
  exercise. Any other Ollama vision model can still be typed in by name.
- Long documents sent to ElevenLabs now report which part they are on, rather
  than sitting on "Synthesising speech…" for minutes. There has never been a
  length limit — text past the API's 10,000-character ceiling is split at
  sentence boundaries and joined back into one continuous recording — but until
  now nothing said so while it was happening.

### Security

- **Programs are launched by absolute path.** Four were launched by bare name,
  where the operating system decides which binary that means — a search whose
  order can begin with the directory the app was unzipped into. Two of those
  four are handed the ElevenLabs API key in plaintext.
- **Files are bounded before they are read.** A `.docx` is a zip, and its text
  was read with no limit on how far it could expand; a few hundred kilobytes on
  disk could become gigabytes in memory and take the app down. Word documents,
  plain text and images now each have a ceiling.
- An image whose filename begins with a dash is no longer mistaken for an
  option by the converter, and files staged in the temporary directory are
  created exclusively and, on macOS, readable only by you.

## [1.0.0] - 2026-08-08

First release. Reads `.txt`, `.docx` and images aloud, or saves the speech as a
WAV or MP3, using the voices built into macOS and Windows or — with an API key —
ElevenLabs. Includes the pronunciation dictionary and full keyboard operation.
