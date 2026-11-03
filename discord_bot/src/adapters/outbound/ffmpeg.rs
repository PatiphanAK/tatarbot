//! `AudioProcessor` on ffprobe/ffmpeg, hardened for untrusted uploads.
//!
//! * Inputs are local files only: `file:` paths, `-protocol_whitelist file`,
//!   no stdin. Playlists (m3u, HLS), concat and network protocols can't be
//!   opened because only the demuxers in `FORMAT_WHITELIST` are allowed. The
//!   image's ffmpeg is also built without them.
//! * Each process runs with limits: address space, CPU seconds, output file
//!   size, stack size, and a wall-clock timeout (killed on expiry).
//! * Output: first audio stream only, metadata stripped, Ogg Opus 48 kHz
//!   stereo with 20 ms frames (what Discord voice sends, so playback passes
//!   the packets through), capped at 15 minutes.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::process::Command;

use crate::application::ports::{AudioError, AudioProcessor, ProbeResult};
use crate::domain::library::MAX_DURATION;

/// Demuxers accepted for uploads (mp3, ogg/opus/vorbis, flac, wav, mp4/m4a,
/// mkv/webm, raw aac). Everything else is refused before parsing.
pub const FORMAT_WHITELIST: &str = "mp3,ogg,flac,wav,mov,matroska,aac";

#[derive(Debug, Clone)]
pub struct FfmpegSettings {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    pub probe_timeout: Duration,
    pub transcode_timeout: Duration,
    pub memory_limit_bytes: u64,
}

impl Default for FfmpegSettings {
    fn default() -> Self {
        Self {
            ffmpeg: "ffmpeg".into(),
            ffprobe: "ffprobe".into(),
            probe_timeout: Duration::from_secs(20),
            // 15 minutes of audio on a quarter vCPU, with headroom.
            transcode_timeout: Duration::from_secs(240),
            memory_limit_bytes: 512 * 1024 * 1024,
        }
    }
}

pub struct Ffmpeg {
    settings: FfmpegSettings,
}

/// Stack limit for both tools. glibc reserves every thread's stack at the soft
/// RLIMIT_STACK, so an inherited large value multiplies across ffmpeg's threads (ffmpeg 8
/// runs each pipeline stage in its own thread, whatever `-threads` says) and can exhaust
/// RLIMIT_AS: on a host with 16 MiB, `pthread_create` failed under the 512 MiB limit.
/// 8 MiB is the usual Linux default; set explicitly so the host's value doesn't matter.
const STACK_BYTES: u64 = 8 * 1024 * 1024;

/// What a process may consume (RLIMIT_AS, RLIMIT_CPU, RLIMIT_FSIZE; RLIMIT_STACK is
/// always `STACK_BYTES`).
#[derive(Debug, Clone, Copy)]
struct Limits {
    memory_bytes: u64,
    cpu_seconds: u64,
    file_bytes: u64,
}

#[derive(Deserialize)]
struct ProbeJson {
    #[serde(default)]
    streams: Vec<ProbeStream>,
    format: Option<ProbeFormat>,
}

#[derive(Deserialize)]
struct ProbeStream {
    codec_type: Option<String>,
}

#[derive(Deserialize)]
struct ProbeFormat {
    duration: Option<String>,
}

impl Ffmpeg {
    /// Bare program names are resolved against this process's PATH now, because the children
    /// run with an empty environment (`env_clear`): without PATH, the system default search
    /// path applies, which doesn't include /usr/local/bin where the images install ffmpeg.
    pub fn new(mut settings: FfmpegSettings) -> Self {
        settings.ffmpeg = resolve_program(settings.ffmpeg);
        settings.ffprobe = resolve_program(settings.ffprobe);
        Self { settings }
    }

    async fn run(
        &self,
        program: &Path,
        args: &[String],
        timeout: Duration,
        limits: Limits,
    ) -> Result<Vec<u8>, AudioError> {
        let mut command = Command::new(program);
        command
            .args(args)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        apply_limits(&mut command, limits);

        let child = command
            .spawn()
            .map_err(|e| AudioError::Failed(format!("cannot start {}: {e}", program.display())))?;
        let output = tokio::time::timeout(timeout, child.wait_with_output())
            .await
            .map_err(|_| AudioError::Timeout)?
            .map_err(|e| AudioError::Failed(e.to_string()))?;
        if output.status.success() {
            return Ok(output.stdout);
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let exit = describe_exit(&output.status);
        tracing::info!(
            program = %program.display(),
            %exit,
            stderr = %stderr.chars().take(500).collect::<String>(),
            "audio processing refused or failed"
        );
        Err(classify_failure(&output.status, &stderr, exit))
    }
}

/// A signal or an allocation/thread failure means a resource limit was hit (the file may be
/// fine); any other non-zero exit means ffmpeg refused the input.
fn classify_failure(status: &std::process::ExitStatus, stderr: &str, exit: String) -> AudioError {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if status.signal().is_some() {
            return AudioError::ResourceLimit(exit);
        }
    }
    let exhausted = [
        "Resource temporarily unavailable",
        "Cannot allocate memory",
        "pthread_create",
    ];
    if exhausted.iter().any(|m| stderr.contains(m)) {
        return AudioError::ResourceLimit(exit);
    }
    let _ = status;
    AudioError::Unsupported
}

/// Exit code, or the signal and the limit it usually means: SIGXCPU = RLIMIT_CPU,
/// SIGXFSZ = RLIMIT_FSIZE, SIGKILL = the timeout (kill_on_drop) or the kernel,
/// SIGSEGV/SIGABRT/SIGBUS = often a failed allocation under RLIMIT_AS.
fn describe_exit(status: &std::process::ExitStatus) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            let meaning = match signal {
                libc::SIGXCPU => "SIGXCPU, CPU-time limit",
                libc::SIGXFSZ => "SIGXFSZ, file-size limit",
                libc::SIGKILL => "SIGKILL",
                libc::SIGSEGV => "SIGSEGV, possibly a failed allocation (address-space limit)",
                libc::SIGABRT => "SIGABRT, possibly a failed allocation (address-space limit)",
                libc::SIGBUS => "SIGBUS",
                _ => "other signal",
            };
            let core = if status.core_dumped() {
                ", core dumped"
            } else {
                ""
            };
            return format!("signal {signal} ({meaning}{core})");
        }
    }
    match status.code() {
        Some(code) => format!("exit code {code}"),
        None => status.to_string(),
    }
}

/// `name` as an absolute path from PATH, if it is a bare name found there; else unchanged
/// (absolute and relative paths are used as given; a missing program fails at spawn).
fn resolve_program(name: PathBuf) -> PathBuf {
    if name.components().count() != 1 {
        return name;
    }
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .map(|dir| dir.join(&name))
        .find(|candidate| candidate.is_file())
        .unwrap_or(name)
}

/// `file:` prefix: never interpret the path as a URL or protocol.
fn file_arg(path: &Path) -> String {
    format!("file:{}", path.display())
}

#[cfg(unix)]
fn apply_limits(command: &mut Command, limits: Limits) {
    // SAFETY: the closure runs in the forked child before exec and only calls
    // getrlimit/setrlimit, which are async-signal-safe; it allocates nothing.
    unsafe {
        command.pre_exec(move || {
            let set = |resource, value: u64| {
                let limit = libc::rlimit {
                    rlim_cur: value as libc::rlim_t,
                    rlim_max: value as libc::rlim_t,
                };
                if libc::setrlimit(resource, &limit) == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            };
            set(libc::RLIMIT_AS, limits.memory_bytes)?;
            set(libc::RLIMIT_CPU, limits.cpu_seconds)?;
            set(libc::RLIMIT_FSIZE, limits.file_bytes)?;
            // Never above the inherited hard limit: raising it needs privileges.
            let mut stack = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            let hard = if libc::getrlimit(libc::RLIMIT_STACK, &mut stack) == 0 {
                stack.rlim_max
            } else {
                STACK_BYTES
            };
            set(libc::RLIMIT_STACK, STACK_BYTES.min(hard))?;
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn apply_limits(_: &mut Command, _: Limits) {}

#[async_trait]
impl AudioProcessor for Ffmpeg {
    async fn probe(&self, input: &Path) -> Result<ProbeResult, AudioError> {
        let args: Vec<String> = [
            "-v",
            "error",
            "-protocol_whitelist",
            "file",
            "-format_whitelist",
            FORMAT_WHITELIST,
            "-show_entries",
            "format=duration:stream=codec_type",
            "-of",
            "json",
        ]
        .into_iter()
        .map(String::from)
        .chain([file_arg(input)])
        .collect();
        let limits = Limits {
            memory_bytes: self.settings.memory_limit_bytes,
            cpu_seconds: self.settings.probe_timeout.as_secs().max(1),
            file_bytes: 0,
        };
        let stdout = self
            .run(
                &self.settings.ffprobe,
                &args,
                self.settings.probe_timeout,
                limits,
            )
            .await?;
        parse_probe(&stdout)
    }

    async fn transcode(&self, input: &Path, output: &Path) -> Result<(), AudioError> {
        let max_seconds = MAX_DURATION.as_secs().to_string();
        let args: Vec<String> = [
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-protocol_whitelist",
            "file",
            "-format_whitelist",
            FORMAT_WHITELIST,
            "-i",
        ]
        .into_iter()
        .map(String::from)
        .chain([file_arg(input)])
        .chain(
            [
                "-map",
                "0:a:0",
                "-vn",
                "-sn",
                "-dn",
                "-map_metadata",
                "-1",
                "-map_chapters",
                "-1",
                "-ac",
                "2",
                "-ar",
                "48000",
                "-c:a",
                "libopus",
                "-b:a",
                "128k",
                "-vbr",
                "on",
                "-frame_duration",
                "20",
                "-application",
                "audio",
                "-t",
                &max_seconds,
                "-threads",
                "1",
                "-f",
                "ogg",
                "-y",
            ]
            .into_iter()
            .map(String::from),
        )
        .chain([file_arg(output)])
        .collect();
        let limits = Limits {
            memory_bytes: self.settings.memory_limit_bytes,
            cpu_seconds: self.settings.transcode_timeout.as_secs().max(1),
            // 15 min at 128 kb/s is ~15 MB; 64 MB stops runaway output.
            file_bytes: 64 * 1024 * 1024,
        };
        self.run(
            &self.settings.ffmpeg,
            &args,
            self.settings.transcode_timeout,
            limits,
        )
        .await
        .map(|_| ())
    }
}

fn parse_probe(stdout: &[u8]) -> Result<ProbeResult, AudioError> {
    let probe: ProbeJson = serde_json::from_slice(stdout).map_err(|_| AudioError::Unsupported)?;
    let audio_streams = probe
        .streams
        .iter()
        .filter(|s| s.codec_type.as_deref() == Some("audio"))
        .count();
    let seconds = probe
        .format
        .and_then(|f| f.duration)
        .and_then(|d| d.parse::<f64>().ok())
        .filter(|d| d.is_finite() && *d > 0.0)
        .unwrap_or(0.0);
    Ok(ProbeResult {
        duration: Duration::from_secs_f64(seconds.min(1e7)),
        audio_streams,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_probe_output() {
        let json = br#"{"streams":[{"codec_type":"video"},{"codec_type":"audio"}],
                        "format":{"duration":"183.250000"}}"#;
        let probe = parse_probe(json).unwrap();
        assert_eq!(probe.audio_streams, 1);
        assert_eq!(probe.duration, Duration::from_millis(183_250));

        let no_duration = parse_probe(br#"{"streams":[],"format":{}}"#).unwrap();
        assert_eq!(
            (no_duration.audio_streams, no_duration.duration),
            (0, Duration::ZERO)
        );
        assert_eq!(parse_probe(b"garbage"), Err(AudioError::Unsupported));
    }

    #[test]
    fn bare_program_names_resolve_against_path() {
        // `sh` exists on every test host; the resolved path is absolute and runnable even
        // with an empty environment.
        let sh = resolve_program("sh".into());
        assert!(sh.is_absolute(), "{sh:?}");
        assert!(
            std::process::Command::new(&sh)
                .env_clear()
                .args(["-c", "exit 0"])
                .status()
                .unwrap()
                .success()
        );
        assert_eq!(
            resolve_program("/opt/x/ffmpeg".into()),
            PathBuf::from("/opt/x/ffmpeg")
        );
        assert_eq!(
            resolve_program("no-such-tool-xyz".into()),
            PathBuf::from("no-such-tool-xyz")
        );
    }

    #[cfg(unix)]
    #[test]
    fn classifies_resource_failures_apart_from_refusals() {
        use std::os::unix::process::ExitStatusExt;
        let killed = std::process::ExitStatus::from_raw(libc::SIGXCPU);
        let exited = std::process::ExitStatus::from_raw(1 << 8);
        assert!(matches!(
            classify_failure(&killed, "", "x".into()),
            AudioError::ResourceLimit(_)
        ));
        assert!(matches!(
            classify_failure(
                &exited,
                "pthread_create() failed: Resource temporarily unavailable",
                "x".into()
            ),
            AudioError::ResourceLimit(_)
        ));
        assert_eq!(
            classify_failure(
                &exited,
                "Invalid data found when processing input",
                "x".into()
            ),
            AudioError::Unsupported
        );
    }

    // ---- Against the real binaries (`ffmpeg`/`ffprobe` on PATH).

    fn ffmpeg() -> Ffmpeg {
        crate::test_support::trace_to_test_output();
        Ffmpeg::new(FfmpegSettings::default())
    }

    /// A 2 s sine wave as mp3, via ffmpeg's lavfi source (test setup only).
    async fn make_mp3(dir: &Path) -> PathBuf {
        let path = dir.join("tone.mp3");
        let status = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=2",
            ])
            .arg(&path)
            .status()
            .await
            .unwrap();
        assert!(status.success());
        path
    }

    #[tokio::test]
    #[ignore = "needs ffmpeg/ffprobe on PATH"]
    async fn ffmpeg_transcodes_audio_to_ogg_opus() {
        let dir = tempfile::tempdir().unwrap();
        let input = make_mp3(dir.path()).await;
        let output = dir.path().join("out.ogg");

        let probe = ffmpeg().probe(&input).await.unwrap();
        assert_eq!(probe.audio_streams, 1);
        assert!((probe.duration.as_secs_f64() - 2.0).abs() < 0.1);

        ffmpeg().transcode(&input, &output).await.unwrap();
        let out = std::process::Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "stream=codec_name,sample_rate,channels",
            ])
            .args(["-of", "csv=p=0"])
            .arg(&output)
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "opus,48000,2");
    }

    /// glibc sizes thread stacks from the inherited RLIMIT_STACK. ffmpeg runs several
    /// threads, so a host with a large stack limit (the Ubuntu 26.04 GitHub runner: 16 MiB)
    /// made `pthread_create` fail under RLIMIT_AS. The wrapper must not depend on it.
    #[tokio::test]
    #[ignore = "needs ffmpeg/ffprobe on PATH"]
    async fn ffmpeg_ignores_a_large_inherited_stack_limit() {
        let dir = tempfile::tempdir().unwrap();
        let input = make_mp3(dir.path()).await;
        let mut old = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: plain getrlimit/setrlimit on this process; restored below.
        unsafe { assert_eq!(libc::getrlimit(libc::RLIMIT_STACK, &mut old), 0) };
        let large = libc::rlimit {
            rlim_cur: (64u64 * 1024 * 1024).min(old.rlim_max) as libc::rlim_t,
            rlim_max: old.rlim_max,
        };
        unsafe { assert_eq!(libc::setrlimit(libc::RLIMIT_STACK, &large), 0) };
        let result = ffmpeg()
            .transcode(&input, &dir.path().join("out.ogg"))
            .await;
        unsafe { libc::setrlimit(libc::RLIMIT_STACK, &old) };
        result.unwrap();
    }

    #[tokio::test]
    #[ignore = "needs ffmpeg/ffprobe on PATH"]
    async fn ffmpeg_refuses_playlists_and_non_audio() {
        let dir = tempfile::tempdir().unwrap();
        let secret = dir.path().join("secret.mp3");
        tokio::fs::copy(make_mp3(dir.path()).await, &secret)
            .await
            .unwrap();
        // Without the whitelists, ffprobe follows both playlists below and
        // reads `secret.mp3` (checked by hand): a local file disclosure.
        let cases = [
            (
                "hls.m3u8",
                "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:2\n#EXTINF:1.0,\nsecret.mp3\n\
                 #EXT-X-ENDLIST\n"
                    .to_owned(),
            ),
            (
                "list.ffconcat",
                "ffconcat version 1.0\nfile secret.mp3\n".to_owned(),
            ),
            (
                "remote.m3u8",
                "#EXTM3U\n#EXTINF:1,\nhttp://169.254.170.2/v2/credentials\n".into(),
            ),
            ("text.mp3", "just some text, not audio".into()),
        ];
        for (name, body) in cases {
            let path = dir.path().join(name);
            tokio::fs::write(&path, body).await.unwrap();
            let probe = ffmpeg().probe(&path).await;
            assert!(
                matches!(probe, Err(AudioError::Unsupported))
                    || matches!(probe, Ok(p) if p.audio_streams == 0),
                "{name}: {probe:?}"
            );
            let out = dir.path().join(format!("{name}.ogg"));
            assert!(ffmpeg().transcode(&path, &out).await.is_err(), "{name}");
        }
    }

    #[tokio::test]
    #[ignore = "needs ffmpeg/ffprobe on PATH"]
    async fn ffmpeg_honours_the_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let input = make_mp3(dir.path()).await;
        let slow = Ffmpeg::new(FfmpegSettings {
            transcode_timeout: Duration::from_millis(1),
            ..FfmpegSettings::default()
        });
        let result = slow.transcode(&input, &dir.path().join("o.ogg")).await;
        assert!(matches!(
            result,
            Err(AudioError::Timeout | AudioError::Unsupported)
        ));
    }
}
