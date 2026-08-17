//! Finding ffmpeg, getting one installed, and pulling frames out of a video.
//!
//! ffmpeg is only needed for video, so nothing here runs until the user opens
//! one. Everything is blocking and expects to be called from a worker thread,
//! never from the UI thread.
//!
//! The app never asks ffmpeg to *describe* anything — it only asks for stills.
//! Which stills is the whole question, since every frame handed on costs a
//! vision-model call that can run for minutes: see [`Sampling`].

use anyhow::{Context, Result, anyhow, bail};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// Whether there is an ffmpeg to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    NotInstalled,
    Available,
}

pub fn status() -> Status {
    if binary_path().is_some() {
        Status::Available
    } else {
        Status::NotInstalled
    }
}

/// Finds the `ffmpeg` binary.
///
/// The usual install locations are checked explicitly as well as `PATH`, for
/// the same reason [`crate::ollama::binary_path`] does it: a GUI app launched
/// from Finder or the Start menu inherits a minimal environment that often
/// doesn't include them.
pub fn binary_path() -> Option<PathBuf> {
    find("ffmpeg")
}

/// Finds `ffprobe`, which ships alongside ffmpeg and is only used to read a
/// video's length. Missing is survivable; see [`duration`].
pub fn probe_path() -> Option<PathBuf> {
    find("ffprobe")
}

fn find(tool: &str) -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    let (locator, fallbacks) = (
        crate::sysexec::system32("where.exe"),
        vec![
            format!("C:\\ffmpeg\\bin\\{tool}.exe"),
            std::env::var("LOCALAPPDATA")
                .map(|dir| format!("{dir}\\Microsoft\\WinGet\\Links\\{tool}.exe"))
                .unwrap_or_default(),
            std::env::var("ProgramFiles")
                .map(|dir| format!("{dir}\\ffmpeg\\bin\\{tool}.exe"))
                .unwrap_or_default(),
        ],
    );
    #[cfg(not(target_os = "windows"))]
    let (locator, fallbacks) = (
        PathBuf::from("/usr/bin/which"),
        vec![
            format!("/opt/homebrew/bin/{tool}"),
            format!("/usr/local/bin/{tool}"),
            format!("/usr/bin/{tool}"),
        ],
    );

    if let Ok(out) = Command::new(&locator).arg(tool).output()
        && out.status.success()
    {
        // `where` can report several matches, one per line; take the first.
        let found = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(str::to_string);
        if let Some(path) = found {
            return Some(path.into());
        }
    }
    fallbacks
        .into_iter()
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .find(|path| path.exists())
}

/// A package manager and the arguments that install ffmpeg with it.
struct Installer {
    program: PathBuf,
    args: &'static [&'static str],
}

/// Homebrew on macOS, winget on Windows — the same two the app already drives
/// for Ollama, so a user who has installed one dependency this way meets no
/// new machinery for the second.
fn package_manager() -> Option<Installer> {
    #[cfg(target_os = "windows")]
    {
        let found = Command::new(crate::sysexec::system32("where.exe"))
            .arg("winget")
            .output()
            .ok()
            .filter(|out| out.status.success())?;
        let path = String::from_utf8_lossy(&found.stdout)
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())?
            .to_string();
        Some(Installer {
            program: path.into(),
            args: &[
                "install",
                "--id",
                "Gyan.FFmpeg",
                "--exact",
                "--silent",
                "--accept-package-agreements",
                "--accept-source-agreements",
                "--disable-interactivity",
            ],
        })
    }
    #[cfg(not(target_os = "windows"))]
    {
        Some(Installer {
            program: crate::homebrew::path()?,
            args: &["install", "ffmpeg"],
        })
    }
}

/// What the app would run to install ffmpeg, or `None` if this machine has no
/// package manager it can drive. Shown to the user verbatim before anything
/// happens, because installing software is not something to do by surprise.
pub fn install_command() -> Option<String> {
    let installer = package_manager()?;
    Some(
        std::iter::once(installer.program.to_string_lossy().to_string())
            .chain(installer.args.iter().map(|arg| (*arg).to_string()))
            .collect::<Vec<_>>()
            .join(" "),
    )
}

/// Advice for when there is no package manager to drive.
#[cfg(target_os = "macos")]
pub fn manual_install_advice() -> String {
    crate::t!("install.advice.homebrew_missing.ffmpeg")
}
#[cfg(not(target_os = "macos"))]
pub fn manual_install_advice() -> String {
    crate::t!("install.advice.no_manager.ffmpeg")
}

/// Where to get ffmpeg without a package manager.
pub const DOWNLOAD_URL: &str = "https://ffmpeg.org/download.html";

/// Installs ffmpeg with whichever package manager this machine has, streaming
/// the output back a line at a time so the UI can show what a multi-minute
/// install is doing. Deliberately the same shape as [`crate::ollama::install`].
pub fn install(mut on_line: impl FnMut(String)) -> Result<()> {
    let installer = package_manager()
        .ok_or_else(|| anyhow!("there is no package manager here that can install ffmpeg"))?;

    let mut child = Command::new(&installer.program)
        .args(installer.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("could not run {}", installer.program.display()))?;

    // These tools write progress to stderr and results to stdout; both are
    // useful, and neither is worth losing.
    let stderr = child.stderr.take().map(|pipe| {
        std::thread::spawn(move || {
            BufReader::new(pipe)
                .lines()
                .map_while(Result::ok)
                .collect::<Vec<_>>()
        })
    });
    if let Some(stdout) = child.stdout.take() {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            on_line(line);
        }
    }
    let status = child
        .wait()
        .context("the installer did not finish cleanly")?;
    if let Some(handle) = stderr
        && let Ok(lines) = handle.join()
    {
        for line in lines {
            on_line(line);
        }
    }

    if !status.success() {
        bail!(
            "installing ffmpeg failed (exit status {status}). Try installing it by hand \
             from {DOWNLOAD_URL}."
        );
    }
    Ok(())
}

/// How long the video runs, if ffprobe is there to say.
///
/// Only used to tell the user what they have opened, so a missing or
/// unparseable answer is `None` rather than an error — a video whose length
/// nobody can measure is still a video that can be described.
pub fn duration(video: &Path) -> Option<Duration> {
    let probe = probe_path()?;
    let mut command = Command::new(probe);
    command
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(video)
        .stdin(Stdio::null());
    no_console_window(&mut command);
    let out = command.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let seconds: f64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    (seconds.is_finite() && seconds > 0.0).then(|| Duration::from_secs_f64(seconds))
}

/// Which frames to pull out of a video.
///
/// Every frame selected here becomes one vision-model call, and on a machine
/// without a GPU a single call can take a minute. So this is a budget as much
/// as a filter, and it works two ways at once:
///
/// * **`scene_threshold`** takes a frame whenever the picture changes enough to
///   be a different shot. That is what makes the cost track the *content* — a
///   ten-minute lecture on one slide is a handful of frames, a fast-cut trailer
///   of the same length is many.
/// * **`floor`** takes one anyway if nothing has been selected for that long,
///   because a slow pan across a landscape never trips a scene change and would
///   otherwise be described by its opening frame alone.
/// * **`max_frames`** is the hard stop, so no video can commit the user to an
///   afternoon of inference by being long or busy.
#[derive(Debug, Clone, Copy)]
pub struct Sampling {
    /// 0.0 to 1.0. ffmpeg's own scene score: how different a frame is from the
    /// one before it. Around 0.4 is a cut; much lower catches camera movement.
    pub scene_threshold: f32,
    pub floor: Duration,
    pub max_frames: usize,
}

/// The longest edge frames are written at.
///
/// The same 2048 the still-image path uses, and for the same reason — see
/// [`crate::extract::image`], where the measurements are. Done here in ffmpeg
/// rather than afterwards because ffmpeg is already decoding the picture.
pub const MAX_LONG_EDGE: u32 = 2048;

/// A still taken out of a video, and where in the video it came from.
pub struct Frame {
    pub path: PathBuf,
    pub at: Duration,
}

/// The filtergraph that does the choosing: which frames, and how big.
///
/// Commas inside a filter expression are protected by the single quotes, which
/// is why the select expression is quoted and the scale one is not. The three
/// clauses added together are an "or" — ffmpeg selects on any non-zero value:
///
/// * `gte(scene,…)` is the cut. `gte` rather than `gt` because the threshold is
///   presented to the user as "this much change counts as a new shot", and a
///   score landing exactly on it should count; ffmpeg does return round numbers
///   like `0.400000`, so this is not hypothetical.
/// * `isnan(prev_selected_t)` takes the opening frame: before anything has been
///   selected there is no previous time, and without this clause a video that
///   opens on its only shot yields nothing at all.
/// * `gte(t-prev_selected_t,…)` is the floor under the whole thing, for the
///   long static shot that never trips a cut.
fn frame_filter(sampling: Sampling) -> String {
    format!(
        "select='gte(scene,{threshold})+isnan(prev_selected_t)+gte(t-prev_selected_t,{floor})',\
         scale='min({edge},iw)':'min({edge},ih)':force_original_aspect_ratio=decrease,showinfo",
        threshold = sampling.scene_threshold,
        floor = sampling.floor.as_secs_f32(),
        edge = MAX_LONG_EDGE,
    )
}

/// Keeps the frames the filter chose rather than padding them back out to a
/// constant rate, which would write the same still hundreds of times.
const RATE_FLAG: &str = "-fps_mode";
/// What [`RATE_FLAG`] was called before ffmpeg 5.0. Still accepted by current
/// versions, but deprecated there, so it is the fallback rather than the
/// default.
const OLD_RATE_FLAG: &str = "-vsync";

/// One run of ffmpeg, and what it said while running.
struct Pass {
    status: std::process::ExitStatus,
    /// Frame timestamps, in the order ffmpeg wrote the frames.
    times: Vec<Duration>,
    /// The last few lines of stderr, kept only to explain a failure.
    tail: Vec<String>,
}

/// True for ffmpeg's complaint about a command-line option it does not have.
fn mentions_unknown_option(line: &str) -> bool {
    let line = line.to_ascii_lowercase();
    line.contains("unrecognized option") || line.contains("unknown option")
}

/// Deletes the frames from an abandoned run, so a retry cannot pick up stills
/// that the run which actually succeeded never wrote.
fn clear_frames(into: &Path) {
    let Ok(entries) = std::fs::read_dir(into) else {
        return;
    };
    for path in entries.filter_map(|entry| entry.ok().map(|e| e.path())) {
        if path.extension().is_some_and(|ext| ext == "jpg") {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Runs ffmpeg once. `Ok(None)` means the job was cancelled while it ran.
#[allow(clippy::too_many_arguments)]
fn one_pass(
    binary: &Path,
    video: &Path,
    filter: &str,
    sampling: Sampling,
    pattern: &Path,
    rate_flag: &str,
    cancel: &crate::jobs::Cancel,
) -> Result<Option<Pass>> {
    use std::sync::atomic::Ordering;

    let mut command = Command::new(binary);
    command
        .arg("-hide_banner")
        // Without this ffmpeg treats the app's own stdin as its console and can
        // sit waiting for a keypress that will never come.
        .arg("-nostdin")
        .arg("-i")
        .arg(video)
        .args(["-vf", filter])
        .args([rate_flag, "vfr"])
        .args(["-frames:v", &sampling.max_frames.to_string()])
        .args(["-q:v", "3"])
        .arg(pattern)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    no_console_window(&mut command);

    let mut child = command
        .spawn()
        .with_context(|| format!("could not run {}", binary.display()))?;

    // ffmpeg says everything on stderr, including the `showinfo` lines that
    // carry the timestamps. Read on its own thread so a full pipe can never
    // wedge the process we are waiting on.
    let stderr = child.stderr.take().expect("stderr was piped");
    let reader = std::thread::spawn(move || {
        let mut times = Vec::new();
        let mut tail: Vec<String> = Vec::new();
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if let Some(at) = showinfo_time(&line) {
                times.push(at);
            }
            tail.push(line);
            if tail.len() > 12 {
                tail.remove(0);
            }
        }
        (times, tail)
    });

    let status = loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            return Ok(None);
        }
        match child.try_wait().context("ffmpeg could not be waited on")? {
            Some(status) => break status,
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    };
    let (times, tail) = reader.join().unwrap_or_default();
    Ok(Some(Pass {
        status,
        times,
        tail,
    }))
}

/// Writes the chosen frames into `into` as JPEGs and reports what came out.
///
/// `cancel` is polled while ffmpeg runs; setting it kills the process and
/// returns nothing, which the caller discards.
pub fn extract_frames(
    video: &Path,
    sampling: Sampling,
    into: &Path,
    cancel: &crate::jobs::Cancel,
    mut on_line: impl FnMut(String),
) -> Result<Vec<Frame>> {
    let binary = binary_path().ok_or_else(|| anyhow!("ffmpeg is not installed"))?;
    let filter = frame_filter(sampling);
    let pattern = into.join("frame-%05d.jpg");

    crate::log::line(format!(
        "video: asking ffmpeg for at most {} frames — scene ≥ {}, or one every {:.0}s",
        sampling.max_frames,
        sampling.scene_threshold,
        sampling.floor.as_secs_f32()
    ));

    let mut pass = one_pass(
        &binary, video, &filter, sampling, &pattern, RATE_FLAG, cancel,
    )?;

    // `-fps_mode` arrived in ffmpeg 5.0. Older builds want the option it
    // replaced, and are otherwise perfectly capable of everything asked here —
    // so an ffmpeg that has never heard of the flag gets one more go with the
    // old spelling rather than a video the app claims it cannot read.
    if let Some(run) = &pass
        && !run.status.success()
        && run.tail.iter().any(|line| mentions_unknown_option(line))
    {
        crate::log::line(format!(
            "video: this ffmpeg does not know {RATE_FLAG}; retrying with {OLD_RATE_FLAG}"
        ));
        clear_frames(into);
        pass = one_pass(
            &binary,
            video,
            &filter,
            sampling,
            &pattern,
            OLD_RATE_FLAG,
            cancel,
        )?;
    }

    let Some(run) = pass else {
        // Cancelled while ffmpeg was running.
        return Ok(Vec::new());
    };
    let (times, tail) = (run.times, run.tail);

    if !run.status.success() {
        for line in &tail {
            on_line(line.clone());
        }
        bail!(
            "ffmpeg could not read {}: {}",
            video.file_name().unwrap_or_default().to_string_lossy(),
            tail.last().map(String::as_str).unwrap_or("no reason given")
        );
    }

    let mut files: Vec<PathBuf> = std::fs::read_dir(into)
        .with_context(|| format!("could not list {}", into.display()))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "jpg"))
        .collect();
    // The `%05d` pattern numbers them in order, so sorting by name is sorting
    // by time.
    files.sort();

    if files.is_empty() {
        bail!(
            "no frames could be taken from {} — it may not contain any video",
            video.file_name().unwrap_or_default().to_string_lossy()
        );
    }
    if times.len() < files.len() {
        // Never seen in practice, but the alternative to noticing is pairing a
        // frame with another frame's timestamp and narrating the wrong order.
        crate::log::line(format!(
            "video: ffmpeg wrote {} frames but reported {} timestamps; keeping the ones that match",
            files.len(),
            times.len()
        ));
    }

    let frames: Vec<Frame> = files
        .into_iter()
        .zip(times)
        .map(|(path, at)| Frame { path, at })
        .collect();
    if frames.is_empty() {
        bail!("ffmpeg took frames from the video but reported no timestamps for them");
    }
    crate::log::line(format!(
        "video: {} frames taken, {} to {}",
        frames.len(),
        crate::audio::spoken_time(frames[0].at),
        crate::audio::spoken_time(frames[frames.len() - 1].at)
    ));
    Ok(frames)
}

/// Pulls `pts_time` out of one `showinfo` line.
///
/// The line is long and its fields move about between ffmpeg versions, so this
/// looks for the one field it needs rather than parsing the whole shape.
fn showinfo_time(line: &str) -> Option<Duration> {
    let rest = line.split("pts_time:").nth(1)?;
    let value = rest.split_whitespace().next()?;
    let seconds: f64 = value.parse().ok()?;
    seconds
        .is_finite()
        .then(|| Duration::from_secs_f64(seconds.max(0.0)))
}

/// Stops a console window flashing up in front of the app on Windows.
fn no_console_window(command: &mut Command) {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt as _;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    #[cfg(not(target_os = "windows"))]
    let _ = command;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_timestamp_is_read_out_of_a_showinfo_line() {
        // Verbatim from ffmpeg 7.1, trimmed of the fields after the one wanted.
        let line = "[Parsed_showinfo_2 @ 0x600003a4c000] n:   3 pts:   181181 pts_time:7.5491 \
                    duration:1001 duration_time:0.0417 fmt:yuvj420p sar:1/1 s:1920x1080";
        let at = showinfo_time(line).expect("the timestamp should be found");
        assert!((at.as_secs_f64() - 7.5491).abs() < 0.0001);
    }

    /// The trigger for the one retry, worded as the two ffmpeg versions in
    /// question actually word it.
    #[test]
    fn an_ffmpeg_too_old_for_the_rate_flag_is_recognised() {
        assert!(mentions_unknown_option("Unrecognized option 'fps_mode'."));
        assert!(mentions_unknown_option("Unknown option \"fps_mode\""));
        // Anything else is a real failure and must not cause a pointless retry.
        assert!(!mentions_unknown_option(
            "clip.mp4: No such file or directory"
        ));
        assert!(!mentions_unknown_option(
            "Invalid data found when processing input"
        ));
    }

    #[test]
    fn lines_without_a_timestamp_are_ignored() {
        assert!(showinfo_time("frame=   12 fps=0.0 q=3.0 size=N/A time=00:00:07.54").is_none());
        assert!(showinfo_time("[out#0/image2 @ 0x14f704080] video:284KiB").is_none());
        // A field that is there but not a number is not a timestamp either.
        assert!(showinfo_time("pts_time:N/A duration:1001").is_none());
    }

    /// The filter is one string with three clauses in it, and getting a comma
    /// or a quote wrong turns a selective pass into one that takes every frame
    /// in the video — which is the expensive way to find out.
    #[test]
    fn the_filter_asks_for_cuts_a_floor_and_a_size() {
        let filter = frame_filter(Sampling {
            scene_threshold: 0.4,
            floor: Duration::from_secs(30),
            max_frames: 40,
        });
        // `gte`, not `gt`: a score landing exactly on the threshold is a cut.
        assert!(filter.contains("gte(scene,0.4)"), "{filter}");
        assert!(filter.contains("gte(t-prev_selected_t,30)"), "{filter}");
        assert!(filter.contains("isnan(prev_selected_t)"), "{filter}");
        assert!(filter.contains("showinfo"), "{filter}");
        // The scale clause must not upscale a small video to the cap.
        assert!(
            filter.contains("force_original_aspect_ratio=decrease"),
            "{filter}"
        );
    }

    /// Only meaningful where ffmpeg is actually installed, which is the machine
    /// this feature is developed on rather than CI.
    #[test]
    fn a_real_ffmpeg_reports_the_length_of_a_video_it_made() {
        let Some(binary) = binary_path() else {
            eprintln!("no ffmpeg on this machine; skipping");
            return;
        };
        let path = std::env::temp_dir().join("soe-ffmpeg-duration-test.mp4");
        let made = Command::new(binary)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x240:rate=10:duration=3",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&path)
            .status()
            .expect("ffmpeg should run");
        assert!(made.success(), "ffmpeg could not write a test video");

        let length = duration(&path).expect("a video ffmpeg just wrote should have a duration");
        std::fs::remove_file(&path).ok();
        assert!(
            (length.as_secs_f32() - 3.0).abs() < 0.5,
            "a 3 second video measured {length:?}"
        );
    }

    /// The real thing end to end: a video built to contain three hard cuts and
    /// nothing else, which should come back as three frames with the times of
    /// those cuts — not one frame, and not ninety.
    #[test]
    fn scene_changes_are_what_gets_taken_out_of_a_real_video() {
        let Some(binary) = binary_path() else {
            eprintln!("no ffmpeg on this machine; skipping");
            return;
        };
        let dir = std::env::temp_dir().join("soe-ffmpeg-frames-test");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).expect("the frame directory should be creatable");
        let video = dir.join("cuts.mp4");

        // Three seconds each of three test patterns, so two unmistakable cuts
        // plus the opening frame.
        //
        // Patterns rather than solid colours, which is not fussiness: ffmpeg
        // scores a scene change from the change in mean absolute difference
        // between frames, and a cut between two flat colour fields scores
        // **zero** — every frame either side is identical to its neighbour, so
        // there is no change in the difference for the cut to stand out from.
        // Written with `color=red`/`green`/`blue` this test failed while the
        // code was right, which is the wrong way round.
        let made = Command::new(&binary)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x240:rate=10:duration=3",
                "-f",
                "lavfi",
                "-i",
                "smptebars=size=320x240:rate=10:duration=3",
                "-f",
                "lavfi",
                "-i",
                "rgbtestsrc=size=320x240:rate=10:duration=3",
                "-filter_complex",
                "[0:v][1:v][2:v]concat=n=3:v=1:a=0[out]",
                "-map",
                "[out]",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&video)
            .status()
            .expect("ffmpeg should run");
        assert!(made.success(), "ffmpeg could not write a test video");

        let frames_dir = dir.join("frames");
        std::fs::create_dir_all(&frames_dir).unwrap();
        let cancel: crate::jobs::Cancel =
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let frames = extract_frames(
            &video,
            Sampling {
                scene_threshold: 0.4,
                // Far longer than the clip, so only the cuts can select.
                floor: Duration::from_secs(600),
                max_frames: 40,
            },
            &frames_dir,
            &cancel,
            |_| {},
        )
        .expect("frames should come out of a video ffmpeg just wrote");

        let times: Vec<f32> = frames.iter().map(|f| f.at.as_secs_f32()).collect();
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(
            times.len(),
            3,
            "expected the opening frame and two cuts: {times:?}"
        );
        assert!(times[0] < 0.5, "the opening frame was not taken: {times:?}");
        assert!(
            (times[1] - 3.0).abs() < 0.5,
            "the first cut was missed: {times:?}"
        );
        assert!(
            (times[2] - 6.0).abs() < 0.5,
            "the second cut was missed: {times:?}"
        );
    }

    /// The other half of the sampling: a shot that never cuts still has to be
    /// looked at more than once. Run with cuts turned off — a threshold of 1.0
    /// is effectively unreachable — so the only thing that can select a frame
    /// here is the floor.
    #[test]
    fn a_video_with_no_cuts_is_still_sampled_on_the_floor() {
        let Some(binary) = binary_path() else {
            eprintln!("no ffmpeg on this machine; skipping");
            return;
        };
        let dir = std::env::temp_dir().join("soe-ffmpeg-floor-test");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).expect("the frame directory should be creatable");
        let video = dir.join("one-shot.mp4");

        let made = Command::new(&binary)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x240:rate=10:duration=10",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&video)
            .status()
            .expect("ffmpeg should run");
        assert!(made.success(), "ffmpeg could not write a test video");

        let frames_dir = dir.join("frames");
        std::fs::create_dir_all(&frames_dir).unwrap();
        let cancel: crate::jobs::Cancel =
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let frames = extract_frames(
            &video,
            Sampling {
                scene_threshold: 1.0,
                floor: Duration::from_secs(2),
                max_frames: 40,
            },
            &frames_dir,
            &cancel,
            |_| {},
        )
        .expect("frames should come out of a video ffmpeg just wrote");

        let times: Vec<f32> = frames.iter().map(|f| f.at.as_secs_f32()).collect();
        std::fs::remove_dir_all(&dir).ok();

        // Ten seconds, one every two: the opening frame and four or five more.
        assert!(
            (5..=6).contains(&times.len()),
            "a 10 second shot sampled every 2 seconds gave {times:?}"
        );
        assert!(times[0] < 0.5, "the opening frame was not taken: {times:?}");
        for pair in times.windows(2) {
            let gap = pair[1] - pair[0];
            assert!(
                (1.5..=2.5).contains(&gap),
                "frames {pair:?} are {gap}s apart, not the 2s asked for"
            );
        }
    }

    /// The cap is the one setting that protects the user from an afternoon of
    /// inference, so it has to hold even when every frame qualifies.
    #[test]
    fn no_more_frames_come_back_than_were_asked_for() {
        let Some(binary) = binary_path() else {
            eprintln!("no ffmpeg on this machine; skipping");
            return;
        };
        let dir = std::env::temp_dir().join("soe-ffmpeg-cap-test");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).expect("the frame directory should be creatable");
        let video = dir.join("long.mp4");

        let made = Command::new(&binary)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x240:rate=10:duration=20",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&video)
            .status()
            .expect("ffmpeg should run");
        assert!(made.success(), "ffmpeg could not write a test video");

        let frames_dir = dir.join("frames");
        std::fs::create_dir_all(&frames_dir).unwrap();
        let cancel: crate::jobs::Cancel =
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let frames = extract_frames(
            &video,
            Sampling {
                // A frame every second of twenty, capped at three.
                scene_threshold: 1.0,
                floor: Duration::from_secs(1),
                max_frames: 3,
            },
            &frames_dir,
            &cancel,
            |_| {},
        )
        .expect("frames should come out of a video ffmpeg just wrote");
        let taken = frames.len();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(taken, 3, "the cap of 3 frames was not honoured");
    }

    /// A cancelled extraction must stop rather than run the video to its end,
    /// since the whole point of Cancel is a user who has changed their mind.
    #[test]
    fn a_cancelled_extraction_returns_nothing() {
        let Some(_) = binary_path() else {
            eprintln!("no ffmpeg on this machine; skipping");
            return;
        };
        let dir = std::env::temp_dir().join("soe-ffmpeg-cancel-test");
        std::fs::create_dir_all(&dir).unwrap();
        let missing = dir.join("not-a-video.mp4");
        let cancel: crate::jobs::Cancel =
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));

        let frames = extract_frames(
            &missing,
            Sampling {
                scene_threshold: 0.4,
                floor: Duration::from_secs(30),
                max_frames: 40,
            },
            &dir,
            &cancel,
            |_| {},
        )
        .expect("a cancelled extraction is not a failure");
        std::fs::remove_dir_all(&dir).ok();
        assert!(frames.is_empty());
    }
}
