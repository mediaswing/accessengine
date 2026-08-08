# Changelog

What changed in each release, written for someone deciding whether to update
rather than for someone reading the diff.

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
