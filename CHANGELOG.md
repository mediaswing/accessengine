# Changelog

What changed in each release, written for someone deciding whether to update
rather than for someone reading the diff.

## [2.2.0] - 2026-08-21

Two additions, both about what a video or photo's description doesn't tell you
unless the app says so. Describing a video now warns first, since a few
minutes of local inference and a model's best guess at the frames are not
obvious from the moment a video is chosen. And a video or JPEG/HEIC photo's
description now ends with a line disclosing where it came from — a local AI
model, not a transcript — and which engine is about to read it aloud.

### Added

- **A warning before describing a video.** Reading a video means minutes of
  local AI inference across dozens of frames, and the model can get details
  wrong — neither of which was obvious from the moment a video was chosen,
  when the job simply started. A dialog now asks first, with a "don't ask
  again" checkbox under **Settings** for anyone who processes video regularly.
- **Video and photo descriptions disclose that they were written by AI.** A
  video's description, and a JPEG or HEIC photo's, now ends with a line saying
  it was written by a local AI model running on Ollama, not transcribed —
  worth knowing wherever the description ends up, not only in the one-time
  warning shown before it starts. That line also names which half of the
  pipeline just left this computer: read with a system voice, it says so; read
  with ElevenLabs, it says the text is being sent to that cloud service to be
  spoken. On by default, with its own checkbox under **Settings** for video and
  for photos. PNGs are left alone, since one read here is far more often a
  screenshot or a diagram than a photo.

### Changed

- **The video narrator no longer repeats itself.** Each frame is described
  alone, so the same person or room was written up fresh in every frame that
  held it, and the narration pass that joins them carried that repetition
  straight through. It is now told to introduce someone or somewhere once and
  mention them again only when something about them has actually changed.

## [2.1.0] - 2026-08-21

A zip of audio files is now a playlist. Everything else here is a security and
accessibility pass over what was already there, and most of those findings are
the same shape: a decision the app had already made deliberately, stopping one
step short of the person who meets it.

### Added

- **Playlists.** Choose a `.zip` in the **Audio player** instead of a single
  file and every WAV and MP3 inside it is a track, played one after another. The
  player pane and the status line both say which one — the status line because
  that is the one place a screen reader is already watching, and a track change
  is exactly the sort of thing that happens while you are looking somewhere
  else.

  A **`media.txt`** inside the zip sets the running order and says which tracks
  are speech and which are music. Music that follows speech does not wait for
  it: it comes up underneath the last second or so and reaches full volume as
  the last word lands, which is the join a radio bulletin makes between the
  newsreader and the outro rather than a gap and then a jolt. The documented
  form is XML, but the file is called `.txt`, so a plain list of names one per
  line works too. Without a `media.txt` at all, the order is the file names read
  the way a person reads them, so `track2` comes before `track10`.

  Nothing is ever unpacked. The archive is held open and each track is
  decompressed as its turn comes, so a playlist leaves nothing behind on disk.
  A track named in the manifest that the zip does not hold, and a playable file
  the manifest never mentioned, are both noted in the log rather than passed
  over in silence — a track that never plays and never says why is the one kind
  of failure a listener cannot notice. See **Playlists** in the README.

### Fixed

- **The "in progress" tone no longer sounds when you open the app.** With the
  engine set to ElevenLabs and a key already saved, the voice picker fetched the
  voice list the moment the window drew, and starting any job sounded the tone
  that says one has begun. A tone with no button behind it is worse than no
  tone: you have to stop and work out what you just did, having done nothing.
  That fetch is now silent, which its answer always was.
- **Disabled controls can be seen again.** They were compositing to 3.38:1 on
  the light theme. The player keeps its four transport buttons greyed rather
  than hidden precisely so they can still be seen and counted, and then very
  nearly hid them. They are now 6.50:1 on light and 8.35:1 on dark, still
  unmistakably dimmer than the same label enabled.
- **Escape closes every dialog, not one of the three.** It had only ever been
  wired to the API key dialog.
- **Keyboard shortcuts no longer fire underneath the reset confirmation.**
  <kbd>⌘O</kbd> raised a file chooser behind the backdrop, and <kbd>Esc</kbd>
  stopped whatever was playing while leaving the dialog exactly where it was.
- **Two dialogs no longer open with the keyboard nowhere.** A dialog that claims
  no focus is a dialog a screen reader never announces. Both now start on the
  answer that changes nothing — Cancel, Not now — because a <kbd>Return</kbd>
  pressed out of habit should not reset your settings or start a
  multi-gigabyte download.
- **Dropping several files at once says so.** It took the first and said nothing
  about the rest, and the order the operating system hands them over in is not
  the order they looked in, so "the first one" was never reliably the one you
  pointed at.
- **The "open this folder" buttons open the right folder.** They built a
  `file://` URL by hand, so a space or a `#` anywhere in the path truncated it
  and the button silently opened somewhere else.

### Security

- **A binary planted next to the app can no longer be picked up.** On Windows,
  `where.exe` searches the current directory before `PATH`, and an app launched
  from Explorer inherits its own folder as that directory — which for a portable
  exe run out of Downloads is the one folder an attacker is most likely to be
  able to write to. Four lookups handed it the choice of which binary to spawn.
  They now refuse an answer from that folder and consider the rest of the
  candidates rather than only the first.
- **The release check only opens an `https` link.** It was handing a URL out of
  a JSON response straight to the browser without looking at its scheme. Every
  other URL the app opens is a constant compiled into the binary, and this one
  now falls back to a constant if it is not `https`.
- **A photo with a malformed coordinate no longer sends it anywhere.** An EXIF
  fraction with a zero denominator reads back as an infinity, and that
  coordinate is the one value on the image path that ever leaves the computer.
  It is now checked for being a real place first.
- **The diagnostic log no longer keeps what a vision model saw.** Describing an
  image wrote the opening of its answer to the log alongside the numbers that
  already say whether it succeeded — and those words are a description of
  whatever private photo or video was open, which is exactly the kind of
  content this file otherwise goes out of its way not to keep. The length,
  stop reason and token counts it already records say enough to tell a real
  answer from an empty one without it.

## [2.0.0] - 2026-08-18

The major number moves because one default changes: a photo's location is no
longer looked up unless you ask for it. Nothing else here takes anything away,
and no setting you have saved is lost.

### Added

- **Light or dark, whichever you want.** **Settings → Appearance** now offers
  **Same as this computer**, **Light** or **Dark**. The dark palette is not
  new — it has shipped since 1.0.0 — but the only way to reach it was to switch
  the whole machine over, which is not a reasonable thing to ask of somebody who
  wants one window dimmer. A bright screen is uncomfortable for plenty of people
  who have no wish to run their whole desktop dark, and the reverse is just as
  true for anyone who reads a light theme more easily. It changes as you pick
  it, with no restart, and defaults to following the computer exactly as before.

### Changed

- **A photo's location is no longer looked up unless you turn it on.** Most
  cameras and phones record the exact spot a photo was taken, and previous
  versions sent that coordinate to OpenStreetMap to be turned into a place name
  every time such a photo was read — with no setting, no prompt, and no mention
  of it anywhere. That is the one thing on the image path that ever left the
  computer, which made "images are read on this machine" true of the picture and
  not of where you were standing when you took it.

  It is now **Look up where a photo was taken**, under **Settings → Vision**,
  and it is off until you turn it on. Turned on, it behaves exactly as it did:
  the coordinate alone — never the photo — goes to OpenStreetMap and comes back
  as a place name that is read out with the description. If you want it, one
  checkbox brings it back for good.

### Fixed

- **A screen reader now says how an action ended.** The status line is where
  every outcome lands, including every failure, and it was an ordinary label:
  not focusable, not in the Tab order, and silent unless somebody went looking
  for it. So the app that exists to read things to people who cannot see them
  answered a failure with a chime — you were told that something had gone wrong
  and never what. It is now a live region: a failure interrupts, anything else
  waits its turn. The running commentary while a long job works is deliberately
  left out, since it changes several times a second and the progress tick is
  already its channel.
- **The percentage on the progress bar was hard to read for the first half of
  every job.** It is yellow, and against the light theme's white track that is
  1.45:1 — nowhere near legible. What made it work was the dark outline under
  it, and the outline was drawn only at the four corners, so the top, bottom and
  sides of every stroke touched bare white. The outline now closes all the way
  round.

### Security

- **The frames taken out of a video go somewhere this app owns.** The directory
  was created with a call that quietly accepts a directory — or a symlink to
  one — that something else put there first. Everything found in that directory
  is read back and described aloud, so the wrong one would be both a copy of
  what you were watching and a way to put a picture in front of the model that
  never came out of your video. It is now created exclusively, and on macOS
  nobody else can look inside.
- **The API key file is no longer written through whatever it finds.** The key
  is written to a temporary name and renamed into place; that temporary was
  opened with a call that follows a symlink and keeps permissions somebody else
  set. It is now created fresh, and a leftover from a crashed run is cleared
  first so the stricter rule cannot lock you out of saving a key.
- **Old-style API keys are kept out of the log.** The log is written to be
  pasted into bug reports and scrubs anything shaped like a key, but that shape
  was `sk_…`, and ElevenLabs issued plain 32-character keys with no prefix for
  years — nothing tells one of those from any other run of hex. The key in use
  is now removed from the log by value, whatever it looks like.
- **A voice id can no longer reshape a request.** It is the one value pasted
  straight into the URL path, and it is read from `config.json`, which is an
  ordinary editable file. Anything that is not a voice id is refused rather than
  sent.

## [1.9.0] - 2026-08-17

### Added

- **The interface can be translated, by editing one plain text file.** No
  programming, no build, no Rust toolchain. **Settings → Language** picks the
  language, or follows whatever the computer is set to; **Open Language Folder**
  puts you where the files live, and **Reload Language Files** shows your
  changes without a restart.

  Everything on screen in normal use moves with it: every label, button,
  caption, tooltip, dialog, keyboard shortcut, progress message and status line,
  including the ones a background job writes while it works. So does the
  spoken clock in the audio player, which is the piece of text a listener has to
  take in while the audio is running.

  A half-finished translation is still a working app. Any line not yet written
  falls back to English, so a file is worth testing from its very first line
  rather than only its last — which is the difference between a translation
  somebody starts and one somebody finishes. Whatever the app could not read is
  reported back by line number under the Language setting, rather than dropped
  in silence.

  A file dropped in the folder whose code matches a built-in language replaces
  it, so a shipped translation can be corrected as well as a new one added.

  The prompts sent to the vision model live in the language file too, which
  means a French interface asks the model in French and gets a French
  description that a French voice can read. A prompt you have written yourself
  in Settings is never overwritten by a language change, and the app says so
  when it has left one alone.

- **A French translation**, complete but written alongside the machinery rather
  than by a native speaker. It wants a proper reading; see the README.

- **macOS offers the app for video files.** Video has been readable since
  1.6.0, but the bundle never claimed the type, so the app was missing from
  Finder's **Open With** for an `.mp4` — the only ways in were the app's own
  file picker, dropping the file on the window, or a path on the command line.
  All six formats it reads conform to `public.movie`, so one declaration covers
  them. It stays an alternate handler, so nothing is taken away from QuickTime.

### Fixed

- **The arrow keys in the Shortcuts pane were drawn as empty boxes.** No font
  the app bundles — Ubuntu Bold, or any of the faces egui ships behind it — has
  a glyph for ← ↑ → ↓, so the two rows describing them showed tofu on every
  platform. They now read "Up / Down" and "Left / Right", which a screen reader
  also announces far better than an arrow. A test now refuses any language file
  containing a character the bundled fonts cannot draw, which is how this was
  found.

### Known limits

- Error text from the reading engines is still English. Everything on screen in
  normal use is not.
- Arabic and Hebrew cannot be laid out correctly, because egui has no
  bidirectional text support yet; they would render backwards.
- Chinese, Japanese, Korean and Vietnamese need a font the app does not bundle.
  Latin, Greek and Cyrillic are all covered.

## [1.8.0] - 2026-08-17

### Added

- **It reads PDFs.** Open a `.pdf` and it comes out as text, ready to read aloud
  or save as a WAV or MP3 like anything else. It is in the open dialog, in
  "Open With", and — on Windows — in the right-click **Speak to file** menu,
  since a PDF reads in a second or two and needs nothing installed.

  A PDF is harder to read than it sounds. It does not store paragraphs or even
  words: it stores instructions to put a particular glyph at a particular point
  on the page, and everything a reader needs — where a word ends, where a line
  breaks, which order the columns go in — was thrown away when the file was
  made. So the page is redrawn and the words worked back out of where the
  glyphs land. Character widths are read from the file's own fonts, which is
  what tells a word gap from ordinary letter spacing; without that a line comes
  out as "M a c B o o k".

  Two kinds of PDF give up nothing, and rather than calling both "empty" it
  says which you have. **A scan** — a photograph of a page, with no text in it
  at all — points you at the image reader, which is the part of this app that
  can read a picture of a page. **A file whose fonts number their glyphs
  instead of naming them** says so too; that is what an older Chinese, Japanese
  or Korean document looks like, and no reader can recover it from the file
  alone. **Encrypted files** say so as well, including the very common kind
  that opens with no password but is locked against printing or copying.

  Files that have been edited usually have a stale index of where their objects
  are. Trusting it reports perfectly readable documents as damaged, so the file
  is scanned for its objects instead of seeking to them.

- **Pages that could not be decoded are now called out.** A document mostly in
  English with a section in Chinese, Japanese or Korean opens and reads fine
  almost all the way through, and then hits pages whose fonts do not record
  what their letters are. Those pages come back with a third or more of their
  characters silently missing — text that looks ordinary on screen and is
  nonsense when spoken. The status line now says how many pages it happened to,
  and the log names them and explains what to do: opening the file in a PDF
  viewer and copying those pages into a plain text file gives them in full. The
  text is still there to read; nothing is hidden.

  Where one of those pages gave back nothing at all, a line now stands in its
  place — "Pages 27 to 41 could not be read: the fonts used there do not say
  what their letters are." Without it such pages are simply absent, and a gap in
  a spoken document sounds exactly like the document having nothing more to say,
  so a whole missing section would pass unnoticed. One marker covers a run of
  consecutive pages rather than interrupting once per page.

## [1.7.0] - 2026-08-15

### Added

- **A right-click "Speak to file" entry, on Windows.** Turn it on once in
  **Settings** and every text, Word and CSV file gets a **Speak to file** option
  in Explorer's right-click menu. Choosing it reads the file, speaks it with
  whatever engine, voice and rate you last saved, and writes the audio next to
  the original — as a WAV or an MP3, whichever you save as — without opening a
  window at all. Run it twice on the same file and the second result is
  numbered rather than written over the first.

  Because the app is a single portable `.exe` that most people run out of
  `Downloads` and delete afterwards, turning this on also copies the app into
  its own settings folder and points the right-click entry at that copy, so
  deleting the download later doesn't break it. **Settings** says where the
  copy lives. Nothing needs an admin prompt, and nothing outside your own
  Windows account is touched.

  Images and video are deliberately left out: they go through Ollama and
  ffmpeg and can run for anything up to the better part of an hour, which is
  not something to start from a menu with no progress bar and no way to cancel.
  Open those in the app as before.

### Fixed

- **A video frame the vision model was cut off describing no longer reaches
  the description.** If the answer for a frame arrived truncated after only a
  word or two, that fragment was written into the account you hear as though it
  were a real description of the frame. Photos already rejected the same
  failure; video now drops it too, and says so in the log, exactly as it does
  for a frame the model had nothing to say about.

## [1.6.0] - 2026-08-13

### Added

- **It describes video.** Open an `.mp4`, `.mov`, `.m4v`, `.avi`, `.mkv` or
  `.webm` and the app takes still frames out of it, reads each one with the same
  vision model that reads your images, and writes the answers up as a single
  continuous description of what happens. From there it is ordinary text: read
  it aloud, or save it as a WAV or MP3 like anything else. Nothing is uploaded
  anywhere. It needs ffmpeg, and the app offers to install it the first time you
  open a video, the same way it offers Ollama.

  Frames are taken wherever the picture changes enough to be a new shot, plus
  one every half minute so a long unbroken take is not described by its opening
  frame alone. That matters more than it sounds: every frame is a separate call
  to the vision model, which can take the better part of a minute on a computer
  with no graphics card, so a ten-minute lecture on one slide costs a handful of
  frames while a fast-cut trailer of the same length costs many. Three settings
  decide it — how much change counts as a new shot, how long before a frame is
  taken anyway, and the most frames one video may use — and each says what it
  means in time rather than in numbers.

  The write-up is done by the vision model itself unless you name a text model
  in **Settings**, so this needs no second download to work. If it fails, or
  comes back as a stub, you get each frame described in turn under the time it
  appears rather than an error — and that form is the more trustworthy of the
  two, since every sentence in it came from a frame.

- **A sound while you wait.** A short tone as work begins, and a quiet tick
  every fifteen seconds while it carries on, so a job that is taking a while and
  a job that has stuck no longer sound the same — which for a video can be the
  difference between a minute and forty. Neither plays over a document being
  read aloud, and the audio player stays silent throughout. The tick has its own
  switch under **Settings**, since it is the one sound that reports nothing new.

### Changed

- **The progress bar can be read.** It is taller, and the percentage is written
  across the middle of it in yellow rather than tucked against the left edge.
  Before, the bar was shorter than the text inside it, so the number was clipped
  top and bottom whatever the size of your display.

## [1.5.0] - 2026-08-10

### Added

- **The audio player shows how much is left.** A countdown under the transport
  buttons — "2 minutes 55 seconds left" — on its own line and in the largest
  type on the pane. The position and the length were both already there, but
  working out the answer from two other numbers is not an answer, and "how much
  longer" is the question a listener actually has. It appears whenever the
  length is known, which is every file you can open from disk.

- **A link to where ElevenLabs keeps your key.** The dialog that asks for one
  now has a button that opens the right page of your ElevenLabs account. "Paste
  the key from your account" was only useful advice if you already knew where in
  that account to look.

### Changed

- **A key is checked as you enter it, and the sound tells you the answer.**
  Saving a key used to chime immediately — a sound celebrating a key that had
  been written to disk and not yet shown to anyone. The key is now tried against
  ElevenLabs straight away: the success sound means accepted, the failure sound
  means refused. If it simply could not be checked — no network, ElevenLabs
  down — it says so and keeps the key, since that is not the key's fault.

- **A key ElevenLabs turns down is removed, and asked for again.** A refused key
  is worth nothing; every request made with it fails the same way. Until now the
  only route back to the dialog was to switch the engine to System voices and
  back again to make the app notice, which is not a thing anyone should have to
  discover. The key is now deleted and the dialog reopens with a line saying
  why. A key set in `ELEVENLABS_API_KEY` is reported but never deleted — that
  one is yours, not the app's.

- **The audio player stays quiet.** The chime that says an action finished was
  also playing when a recording was loaded, and again as one reached its end —
  landing over the last words of the very thing the app had been asked to play.
  In the player the sound *is* the output, so it makes none of its own. Failures
  there still make their noise: "that file would not open" is worth interrupting
  for, "that file opened" is not. Everywhere else the sounds are unchanged.

- **The pane is called "Audio player"**, not "Audio Player".

## [1.3.0] - 2026-08-10

### Added

- **Finishing and failing now make a sound.** A short chime when an action
  succeeds, a lower tone when something goes wrong — so you know which happened
  without watching the status line change colour. It plays over a document
  being read aloud rather than waiting for it to end, because "that failed" is
  worth knowing at the time.

  Only outcomes make a sound. Progress messages — "reading part 2 of 5" — stay
  silent, or a long document would chime through its own narration. There is a
  checkbox under **Settings** to turn the sounds off, and ticking it plays the
  success sound so you can hear that it worked.

  The two recordings are CC0 from freesound.org. Both were trimmed of silence,
  levelled to match each other and faded at both ends; as published, the
  failure sound was about 11 dB quieter than the success one, which would have
  made the sound telling you something went wrong the one you strained to hear.

## [1.2.8] - 2026-08-10

### Added

- **Word documents can now have their formatting read out.** A new **What to
  read** dropdown on the main pane chooses between *Words only*, which is what
  the app has always done and is still what it does unless you change it, and
  *Words and formatting*, which announces where bold, italic, underline,
  strikethrough, colour and highlighting start and end — "your payment of,
  bold, dark red text, £82.50, end dark red text, end bold, is due on the
  30th". If you cannot see the page there is otherwise no way to know that a
  date was emphasised, or that a contract put one clause in red.

  Both ends of each run are announced rather than only the start, so you can
  tell which words were covered. Only the *change* between one run and the next
  is spoken, so a whole emphasised sentence is announced once rather than at
  every word. Colours are named — "dark red", "light blue" — rather than read
  out as the hex numbers Word stores. Black is not announced at all, since Word
  writes it explicitly throughout documents nobody has ever recoloured.

  Changing the setting re-reads the document you already have open. It is
  greyed out, with the reason, for files that carry no formatting.

  Formatting applied through a Word *style* is not announced — only formatting
  applied directly, which is what the bold button does. In practice this
  reports what the author chose to emphasise, instead of announcing every
  heading in the document as bold.

## [1.2.7] - 2026-08-09

### Fixed

- **Prices and other quoted values in a CSV were read out with their quote
  marks.** A spreadsheet that writes `"Technology", "£50.28"` — with a space
  after the comma — had the quotes treated as part of the value instead of as
  punctuation around it, so they were spoken. Worse and less visible: a value
  that was not recognised as quoted no longer protected what was inside it, so
  a cell like `"London, England"` was split into two columns and every value
  after it on that row was read out under the wrong heading. Both are fixed,
  and a quote mark used to mean inches — `5" bore` — is still read as one.

- **A CSV whose lines end in a comma announced a column that wasn't there.**
  Every row finished with "Column 3: empty", which is the last thing you hear
  about each row and tells you nothing about any of them. Trailing empty cells
  that no heading covers are no longer read out. A cell that is empty *under* a
  heading is still announced, since that is missing data, and a genuine extra
  value is still read rather than quietly skipped.

## [1.2.6] - 2026-08-09

### Changed

- **The ElevenLabs API key is now kept in a file, not in the keychain.** It goes
  in `elevenlabs.key` in the app's own settings folder — `~/Library/Application
  Support` on macOS, `%APPDATA%` on Windows — as plain text, in a file only your
  account can read. A key saved by an earlier version is moved there the first
  time this one runs, so there is nothing to re-enter.

  This is a deliberate step back from the macOS login keychain and Windows
  DPAPI, because both were reached by handing the key to another program and
  both could fail in ways that *stored the wrong thing and reported success*.
  The macOS one had the worse version of that: `security` reads a password from
  the terminal, not from the pipe it was given, so running the app from a
  terminal made it hang on a prompt the user could not see, and then store
  whatever they eventually typed. What the old storage bought was thinner than
  it sounds — the DPAPI blob decrypts for this same account, and the keychain
  item could be read back without a prompt — so anything already running as you
  could reach either. Actually keeping the key you typed is worth more.

### Fixed

- **Entering an API key could hang the app and then save the wrong thing.** The
  cause is above. The window stopped responding, a password prompt appeared in
  the terminal if the app had been started from one, and whatever was typed
  there was saved in place of the key — after which the app still reported that
  no key had been entered.

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
