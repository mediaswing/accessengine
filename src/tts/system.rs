//! The operating system's built-in voices — the no-API-key path.
//!
//! Both supported platforms already have a speech synthesiser installed, and on
//! both it is the same one the user's screen reader speaks with, so the voice
//! they hear here is the voice they are used to.
//!
//! * **macOS** uses `say`, which both plays speech and writes it to a file.
//!   Text goes in over stdin, because a document easily exceeds the
//!   command-line length limit.
//! * **Windows** uses `System.Speech` (SAPI 5) driven by a short PowerShell
//!   script. The script is passed as `-EncodedCommand`, and the text goes in a
//!   UTF-8 temporary file that the script deletes as soon as it has read it —
//!   between them those two avoid every quoting, encoding and length problem
//!   that comes with putting a document on a Windows command line.
//!
//! Speaking returns the child process rather than waiting on it: killing that
//! process is what "stop" means, and neither platform offers anything better.

use super::Voice;
use anyhow::{Result, bail};
use std::path::Path;
use std::process::Child;

/// True if this build has a working system-voice implementation.
pub const SUPPORTED: bool = cfg!(any(target_os = "macos", target_os = "windows"));

/// Shown wherever the system engine is offered but cannot work.
pub const UNSUPPORTED_MESSAGE: &str =
    "System voices are available on macOS and Windows. On this system, choose ElevenLabs instead.";

/// Lists the installed system voices.
pub fn list_voices() -> Result<Vec<Voice>> {
    platform::list_voices()
}

/// Starts speaking through the default output device. The returned child is
/// still running; killing it stops playback.
pub fn speak(text: &str, voice: &str, rate: u32) -> Result<Child> {
    check(text)?;
    platform::speak(text, voice.trim(), rate)
}

/// Renders speech straight to a 16-bit mono WAV file.
pub fn write_wav(text: &str, voice: &str, rate: u32, destination: &Path) -> Result<()> {
    check(text)?;
    platform::write_wav(text, voice.trim(), rate, destination)?;
    if !destination.exists() {
        bail!("the system voice reported success but wrote no file");
    }
    Ok(())
}

fn check(text: &str) -> Result<()> {
    if !SUPPORTED {
        bail!("{UNSUPPORTED_MESSAGE}");
    }
    if text.trim().is_empty() {
        bail!("there is no text to read");
    }
    Ok(())
}

// --------------------------------------------------------------------- macOS

/// Parses `say -v ?`, whose lines look like:
///
/// ```text
/// Alex                en_US    # Most people recognize me by my voice.
/// Bad News            en_US    # The light you see at the end of the tunnel…
/// ```
///
/// The name can contain spaces, so it is everything before the final token on
/// the left of the `#`, which is the locale.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_say_voices(stdout: &str) -> Vec<Voice> {
    stdout
        .lines()
        .filter_map(|line| {
            let left = line.split('#').next()?.trim_end();
            let (name, locale) = left.rsplit_once(char::is_whitespace)?;
            let name = name.trim();
            let locale = locale.trim();
            if name.is_empty() || locale.is_empty() {
                return None;
            }
            Some(Voice {
                id: name.to_string(),
                name: name.to_string(),
                detail: locale.replace('_', "-"),
            })
        })
        .collect()
}

#[cfg(target_os = "macos")]
mod platform {
    use super::{Voice, parse_say_voices};
    use anyhow::{Context, Result, bail};
    use std::io::Write as _;
    use std::path::Path;
    use std::process::{Child, Command, Stdio};

    pub fn list_voices() -> Result<Vec<Voice>> {
        let output = Command::new("/usr/bin/say")
            .args(["-v", "?"])
            .output()
            .context("could not run `say` to list the system voices")?;
        if !output.status.success() {
            bail!("`say` could not list the installed voices");
        }
        let voices = parse_say_voices(&String::from_utf8_lossy(&output.stdout));
        if voices.is_empty() {
            bail!("macOS reported no installed voices");
        }
        Ok(voices)
    }

    /// Builds a `say` invocation shared by the speak and save paths.
    ///
    /// An empty `voice` leaves the system default in place. `rate` is words per
    /// minute; macOS itself defaults to 175.
    fn command(voice: &str, rate: u32) -> Command {
        let mut command = Command::new("/usr/bin/say");
        if !voice.is_empty() {
            command.args(["-v", voice]);
        }
        command.args(["-r", &rate.clamp(50, 500).to_string()]);
        command
    }

    /// Feeds `text` to a spawned `say` over stdin and hands back the child.
    fn spawn_with_text(mut command: Command, text: &str) -> Result<Child> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("could not run `say`")?;

        let mut stdin = child
            .stdin
            .take()
            .context("could not write the text to `say`")?;
        let text = text.to_string();
        // Write on another thread: `say` may not drain a long document until it
        // starts speaking, and a blocked write here would deadlock the caller.
        std::thread::spawn(move || {
            let _ = stdin.write_all(text.as_bytes());
        });

        Ok(child)
    }

    pub fn speak(text: &str, voice: &str, rate: u32) -> Result<Child> {
        spawn_with_text(command(voice, rate), text)
    }

    pub fn write_wav(text: &str, voice: &str, rate: u32, destination: &Path) -> Result<()> {
        let mut command = command(voice, rate);
        command
            .arg("--file-format=WAVE")
            // Little-endian signed 16-bit at 22.05 kHz: what the voices render
            // at, and what the Windows path is asked to produce too.
            .arg("--data-format=LEI16@22050")
            .arg("-o")
            .arg(destination);

        let child = spawn_with_text(command, text)?;
        let output = child
            .wait_with_output()
            .context("`say` did not finish cleanly")?;
        if !output.status.success() {
            bail!(
                "`say` could not write the audio: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }
}

// ------------------------------------------------------------------- Windows

/// Escapes a value for a PowerShell single-quoted string, where the only
/// special character is the quote itself, doubled.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn ps_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// Converts words per minute to a SAPI rate.
///
/// SAPI's scale is -10 to 10 and multiplicative — roughly 1.15× per step from a
/// default around 175 wpm — so the conversion is logarithmic rather than the
/// linear mapping it is often mistaken for.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn sapi_rate(wpm: u32) -> i32 {
    /// What rate 0 sounds like, and what the app's own slider defaults to.
    const BASELINE_WPM: f64 = 175.0;

    let wpm = f64::from(wpm.clamp(50, 500));
    let steps = (wpm / BASELINE_WPM).ln() / 1.15_f64.ln();
    (steps.round() as i32).clamp(-10, 10)
}

/// Parses the tab-separated `name<TAB>culture` lines the listing script emits.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn parse_sapi_voices(stdout: &str) -> Vec<Voice> {
    stdout
        .lines()
        .filter_map(|line| {
            let (name, culture) = line.trim_end_matches('\r').split_once('\t')?;
            let name = name.trim();
            if name.is_empty() {
                return None;
            }
            Some(Voice {
                id: name.to_string(),
                name: name.to_string(),
                detail: culture.trim().to_string(),
            })
        })
        .collect()
}

#[cfg(target_os = "windows")]
mod platform {
    use super::{Voice, parse_sapi_voices, ps_quote, sapi_rate};
    use anyhow::{Context, Result, bail};
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64;
    use std::os::windows::process::CommandExt as _;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};

    /// Keeps a console window from flashing up behind the GUI every time the
    /// app speaks. `CREATE_NO_WINDOW`.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    /// PowerShell takes a UTF-16LE, base64 command with `-EncodedCommand`,
    /// which sidesteps `cmd`'s quoting rules entirely.
    fn powershell(script: &str) -> Command {
        let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let mut command = Command::new("powershell.exe");
        command
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-EncodedCommand",
            ])
            .arg(BASE64.encode(utf16))
            .creation_flags(CREATE_NO_WINDOW);
        command
    }

    pub fn list_voices() -> Result<Vec<Voice>> {
        let script = "\
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Speech
$synth = New-Object System.Speech.Synthesis.SpeechSynthesizer
foreach ($voice in $synth.GetInstalledVoices()) {
    if ($voice.Enabled) {
        $info = $voice.VoiceInfo
        [Console]::Out.WriteLine($info.Name + \"`t\" + $info.Culture.Name)
    }
}
$synth.Dispose()";

        let output = powershell(script)
            .output()
            .context("could not run PowerShell to list the installed voices")?;
        if !output.status.success() {
            bail!(
                "Windows could not list the installed voices: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let voices = parse_sapi_voices(&String::from_utf8_lossy(&output.stdout));
        if voices.is_empty() {
            bail!("Windows reported no installed voices");
        }
        Ok(voices)
    }

    /// Writes the document somewhere the script can read it as UTF-8. The
    /// script deletes it as soon as it has been read, so nothing is left behind
    /// even when speech is stopped halfway through.
    fn write_text_file(text: &str) -> Result<PathBuf> {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "accessengine-{}-{}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::write(&path, text).context("could not stage the text for the speech engine")?;
        Ok(path)
    }

    /// The shared preamble: read the document, delete it, build the
    /// synthesiser, and pick the voice and speed.
    ///
    /// Selecting a voice is wrapped in `try`, because a voice can be uninstalled
    /// between the app listing it and the user pressing Apply — falling back to
    /// the system default is much better than silence.
    fn preamble(text_file: &Path, voice: &str, rate: u32) -> String {
        let mut script = format!(
            "\
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Speech
$path = {}
$text = [IO.File]::ReadAllText($path, [Text.Encoding]::UTF8)
Remove-Item -LiteralPath $path -Force -ErrorAction SilentlyContinue
$synth = New-Object System.Speech.Synthesis.SpeechSynthesizer
$synth.Rate = {}
",
            ps_quote(&text_file.to_string_lossy()),
            sapi_rate(rate)
        );
        if !voice.is_empty() {
            script.push_str(&format!(
                "try {{ $synth.SelectVoice({}) }} catch {{ }}\n",
                ps_quote(voice)
            ));
        }
        script
    }

    pub fn speak(text: &str, voice: &str, rate: u32) -> Result<Child> {
        let text_file = write_text_file(text)?;
        let script = format!(
            "{}$synth.SetOutputToDefaultAudioDevice()\n$synth.Speak($text)\n$synth.Dispose()",
            preamble(&text_file, voice, rate)
        );
        powershell(&script)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("could not run PowerShell to speak the text")
    }

    pub fn write_wav(text: &str, voice: &str, rate: u32, destination: &Path) -> Result<()> {
        let text_file = write_text_file(text)?;
        // 16-bit mono at 22.05 kHz, stated explicitly so a Windows recording is
        // the same shape as a macOS one and the MP3 encoder sees no surprises.
        let script = format!(
            "\
{}$format = New-Object System.Speech.AudioFormat.SpeechAudioFormatInfo(22050, \
[System.Speech.AudioFormat.AudioBitsPerSample]::Sixteen, \
[System.Speech.AudioFormat.AudioChannel]::Mono)
$synth.SetOutputToWaveFile({}, $format)
$synth.Speak($text)
$synth.Dispose()",
            preamble(&text_file, voice, rate),
            ps_quote(&destination.to_string_lossy())
        );

        let output = powershell(&script)
            .output()
            .context("could not run PowerShell to write the audio")?;
        if !output.status.success() {
            bail!(
                "Windows could not write the audio: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }
}

// ----------------------------------------------------- everywhere else

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod platform {
    use super::{UNSUPPORTED_MESSAGE, Voice};
    use anyhow::{Result, bail};
    use std::path::Path;
    use std::process::Child;

    pub fn list_voices() -> Result<Vec<Voice>> {
        bail!("{UNSUPPORTED_MESSAGE}")
    }

    pub fn speak(_text: &str, _voice: &str, _rate: u32) -> Result<Child> {
        bail!("{UNSUPPORTED_MESSAGE}")
    }

    pub fn write_wav(_text: &str, _voice: &str, _rate: u32, _destination: &Path) -> Result<()> {
        bail!("{UNSUPPORTED_MESSAGE}")
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_sapi_voices, parse_say_voices, ps_quote, sapi_rate};

    #[test]
    fn parses_say_names_locales_and_multiword_names() {
        let stdout = "\
Albert              en_US    # Hello! My name is Albert.
Bad News            en_US    # The light you see at the end of the tunnel.
Amélie              fr_CA    # Bonjour! Je m'appelle Amélie.
";
        let voices = parse_say_voices(stdout);
        assert_eq!(voices.len(), 3);
        assert_eq!(voices[0].name, "Albert");
        assert_eq!(voices[0].detail, "en-US");
        assert_eq!(voices[1].name, "Bad News");
        assert_eq!(voices[2].name, "Amélie");
        assert_eq!(voices[2].detail, "fr-CA");
    }

    #[test]
    fn skips_blank_and_malformed_say_lines() {
        let stdout = "\n   \nOnlyOneToken\nAlex   en_US   # hi\n";
        let voices = parse_say_voices(stdout);
        assert_eq!(voices.len(), 1);
        assert_eq!(voices[0].id, "Alex");
    }

    #[test]
    fn parses_sapi_voice_lines_including_crlf() {
        let stdout = "Microsoft Hazel Desktop\ten-GB\r\nMicrosoft David Desktop\ten-US\r\n\t\n";
        let voices = parse_sapi_voices(stdout);
        assert_eq!(voices.len(), 2);
        assert_eq!(voices[0].name, "Microsoft Hazel Desktop");
        assert_eq!(voices[0].detail, "en-GB");
        assert_eq!(voices[1].detail, "en-US");
    }

    #[test]
    fn sapi_rate_is_zero_at_the_default_and_ordered_around_it() {
        assert_eq!(sapi_rate(175), 0);
        assert!(sapi_rate(90) < 0);
        assert!(sapi_rate(350) > 0);
        // Monotonic and inside SAPI's range across the whole slider.
        let mut previous = i32::MIN;
        for wpm in 90..=350 {
            let rate = sapi_rate(wpm);
            assert!((-10..=10).contains(&rate), "{wpm} wpm gave rate {rate}");
            assert!(rate >= previous, "rate fell going from {} wpm", wpm - 1);
            previous = rate;
        }
    }

    #[test]
    fn ps_quote_doubles_embedded_quotes() {
        assert_eq!(ps_quote(r"C:\Users\Jo\out.wav"), r"'C:\Users\Jo\out.wav'");
        // The one character that could end the string early.
        assert_eq!(ps_quote("it's"), "'it''s'");
        assert_eq!(
            ps_quote("'; Remove-Item C:\\ ;'"),
            "'''; Remove-Item C:\\ ;'''"
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn the_real_system_reports_voices() {
        let voices = super::list_voices().expect("macOS should have voices installed");
        assert!(!voices.is_empty());
        assert!(voices.iter().all(|v| !v.id.is_empty()));
    }

    /// Exercises the whole system-voice save path against the real synthesiser.
    #[test]
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn renders_real_speech_to_wav_and_mp3() {
        let wav = std::env::temp_dir().join("accessengine-system-tts-test.wav");
        super::write_wav(
            "Testing the speech output engine, one two three.",
            "",
            200,
            &wav,
        )
        .expect("the system voice should render to a WAV file");

        let pcm = crate::audio::read_wav(&wav).expect("the WAV should be readable");
        std::fs::remove_file(&wav).ok();

        assert_eq!(pcm.channels, 1);
        assert_eq!(pcm.sample_rate, 22_050);
        // Roughly a second and a half of speech; assert only that it is real.
        assert!(
            pcm.samples.len() > 11_000,
            "expected speech, got {} samples",
            pcm.samples.len()
        );
        assert!(
            pcm.samples.iter().any(|s| s.abs() > 500),
            "the rendered audio was silent"
        );

        let mp3 = crate::audio::encode_mp3(&pcm).expect("the WAV should encode to MP3");
        assert!(mp3.len() > 1_000, "suspiciously small MP3: {}", mp3.len());
    }

    /// The live-playback path: the process should start and stay running until
    /// it is killed, which is what the Stop button relies on.
    ///
    /// Skipped on a Windows CI runner, which has no audio endpoint at all —
    /// SAPI throws on `SetOutputToDefaultAudioDevice` there, so the process
    /// would exit immediately and this would be testing the runner rather than
    /// the code. The file-rendering test above needs no device and still runs
    /// everywhere, so the Windows synthesis path is not left unexercised.
    #[test]
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn speaking_spawns_a_killable_process() {
        if cfg!(target_os = "windows") && std::env::var_os("CI").is_some() {
            return;
        }
        let mut child = super::speak("Testing.", "", 250).expect("speech should start");
        assert!(
            child.try_wait().unwrap().is_none(),
            "the speech process exited before it could be stopped"
        );
        child.kill().expect("the process should be killable");
        assert!(child.wait().is_ok());
    }

    #[test]
    fn refuses_to_render_empty_text() {
        let path = std::env::temp_dir().join("accessengine-should-not-exist.wav");
        assert!(super::write_wav("   \n  ", "", 175, &path).is_err());
        assert!(!path.exists());
    }
}
