//! Finding media in messages, fetching it from Telegram and preparing audio
//! for speech-to-text with ffmpeg.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use tokio::process::Command;

use crate::telegram::{FileMeta, Message, Telegram, CLOUD_DOWNLOAD_LIMIT};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Voice,
    VideoNote,
    Audio,
    Video,
    Document,
}

impl MediaKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MediaKind::Voice => "voice",
            MediaKind::VideoNote => "video_note",
            MediaKind::Audio => "audio",
            MediaKind::Video => "video",
            MediaKind::Document => "document",
        }
    }

    pub fn human(self) -> &'static str {
        match self {
            MediaKind::Voice => "voice message",
            MediaKind::VideoNote => "video circle",
            MediaKind::Audio => "audio file",
            MediaKind::Video => "video",
            MediaKind::Document => "file",
        }
    }
}

#[derive(Debug, Clone)]
pub struct MediaRef {
    pub kind: MediaKind,
    pub file_id: String,
    pub file_size: Option<u64>,
    pub duration: Option<u32>,
}

impl MediaRef {
    fn from_meta(kind: MediaKind, m: &FileMeta) -> Self {
        Self {
            kind,
            file_id: m.file_id.clone(),
            file_size: m.file_size,
            duration: m.duration,
        }
    }
}

/// Returns the transcribable media attached to this message, if any.
pub fn extract_media(msg: &Message) -> Option<MediaRef> {
    if let Some(m) = &msg.voice {
        return Some(MediaRef::from_meta(MediaKind::Voice, m));
    }
    if let Some(m) = &msg.video_note {
        return Some(MediaRef::from_meta(MediaKind::VideoNote, m));
    }
    if let Some(m) = &msg.audio {
        return Some(MediaRef::from_meta(MediaKind::Audio, m));
    }
    if let Some(m) = &msg.video {
        return Some(MediaRef::from_meta(MediaKind::Video, m));
    }
    if let Some(m) = &msg.document {
        let mime = m.mime_type.as_deref().unwrap_or("");
        if mime.starts_with("audio/") || mime.starts_with("video/") {
            return Some(MediaRef::from_meta(MediaKind::Document, m));
        }
    }
    None
}

/// For guest mode: media in the summoning message itself, or in the message
/// it replies to.
pub fn extract_guest_media(msg: &Message) -> Option<MediaRef> {
    extract_media(msg).or_else(|| msg.reply_to_message.as_deref().and_then(extract_media))
}

/// Makes the file available on local disk and returns its path.
///
/// A local Bot API server (`--local`) returns an absolute path on its own
/// filesystem; the bot container mounts that directory read-only, so ffmpeg
/// can read it in place. The cloud API returns a relative path that we
/// download into `work_dir`.
pub async fn fetch_input(
    tg: &Telegram,
    media: &MediaRef,
    work_dir: &Path,
    max_bytes: u64,
) -> Result<PathBuf> {
    if let Some(size) = media.file_size {
        if size > max_bytes {
            bail!(
                "file is {} MB, above the {} MB limit",
                size / 1_000_000,
                max_bytes / 1_000_000
            );
        }
        if !tg.is_local_server() && size > CLOUD_DOWNLOAD_LIMIT {
            bail!("file is larger than 20 MB; the cloud Bot API cannot download it (run the local Bot API server)");
        }
    }

    let file = tg.get_file(&media.file_id).await?;
    let file_path = file
        .file_path
        .ok_or_else(|| anyhow!("Telegram returned no file_path (file too big?)"))?;

    if Path::new(&file_path).is_absolute() {
        let p = PathBuf::from(&file_path);
        tokio::fs::metadata(&p).await.with_context(|| {
            format!(
                "local Bot API file {file_path} is not readable; mount the server's working directory into the bot container"
            )
        })?;
        return Ok(p);
    }

    let dest = work_dir.join("input");
    tg.download_to(&file_path, &dest).await?;
    Ok(dest)
}

/// Converts any audio/video input to mono 16 kHz Opus in Ogg: small, and
/// accepted by OpenAI, Groq and ElevenLabs. ~24 kbit/s keeps 25 MB API limits
/// far away (about 2 hours of audio).
pub async fn to_speech_ogg(input: &Path, output: &Path, timeout: Duration) -> Result<()> {
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(input)
        .args([
            "-vn",
            "-map",
            "0:a:0",
            "-ac",
            "1",
            "-ar",
            "16000",
            "-c:a",
            "libopus",
            "-b:a",
            "24k",
            "-application",
            "voip",
        ])
        .arg(output);
    run(cmd, timeout)
        .await
        .map_err(|e| {
            let s = e.to_string();
            // Wording differs across ffmpeg versions.
            if s.contains("matches no streams")
                || s.contains("does not contain any stream")
                || s.contains("'0:a:0'")
            {
                anyhow!("this media has no audio track")
            } else {
                e
            }
        })
        .map(|_| ())
}

/// Duration in seconds, via ffprobe.
pub async fn probe_duration(path: &Path, timeout: Duration) -> Result<f64> {
    let mut cmd = Command::new("ffprobe");
    cmd.args([
        "-v",
        "error",
        "-show_entries",
        "format=duration",
        "-of",
        "default=nw=1:nk=1",
    ])
    .arg(path);
    let out = run(cmd, timeout).await?;
    out.trim()
        .parse::<f64>()
        .with_context(|| format!("ffprobe returned {:?}", out.trim()))
}

/// Splits `input` into chunks of `chunk_secs` seconds (stream copy, no
/// re-encoding). Returns the input itself when it is short enough.
pub async fn split(
    input: &Path,
    dir: &Path,
    duration: f64,
    chunk_secs: u64,
    timeout: Duration,
) -> Result<Vec<PathBuf>> {
    if chunk_secs == 0 || duration <= chunk_secs as f64 + 1.0 {
        return Ok(vec![input.to_path_buf()]);
    }
    let pattern = dir.join("part_%03d.ogg");
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(input)
        .args([
            "-f",
            "segment",
            "-segment_time",
            &chunk_secs.to_string(),
            "-c",
            "copy",
        ])
        .arg(&pattern);
    run(cmd, timeout).await?;

    let mut parts = Vec::new();
    let mut rd = tokio::fs::read_dir(dir).await?;
    while let Some(e) = rd.next_entry().await? {
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with("part_") && name.ends_with(".ogg") {
            parts.push(e.path());
        }
    }
    parts.sort();
    if parts.is_empty() {
        bail!("ffmpeg produced no chunks");
    }
    Ok(parts)
}

/// Runs a command with a timeout, returning stdout. The child is killed if the
/// future is dropped (cancel button) or the timeout fires.
async fn run(mut cmd: Command, timeout: Duration) -> Result<String> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let name = cmd.as_std().get_program().to_string_lossy().to_string();
    let child = cmd
        .spawn()
        .with_context(|| format!("failed to start {name}"))?;
    let out = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| anyhow!("{name} timed out after {}s", timeout.as_secs()))??;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let tail: String = stderr
            .lines()
            .rev()
            .take(3)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join(" | ");
        bail!("{name} failed: {tail}");
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(id: &str) -> Option<FileMeta> {
        Some(FileMeta {
            file_id: id.into(),
            ..Default::default()
        })
    }

    #[test]
    fn extracts_supported_media() {
        let mut m = Message {
            video_note: meta("vn"),
            ..Default::default()
        };
        assert_eq!(extract_media(&m).unwrap().kind, MediaKind::VideoNote);

        m = Message {
            voice: meta("v"),
            ..Default::default()
        };
        assert_eq!(extract_media(&m).unwrap().kind, MediaKind::Voice);

        let mut doc = FileMeta {
            file_id: "d".into(),
            mime_type: Some("application/pdf".into()),
            ..Default::default()
        };
        m = Message {
            document: Some(doc.clone()),
            ..Default::default()
        };
        assert!(extract_media(&m).is_none());
        doc.mime_type = Some("audio/mpeg".into());
        m = Message {
            document: Some(doc),
            ..Default::default()
        };
        assert_eq!(extract_media(&m).unwrap().kind, MediaKind::Document);
    }

    #[test]
    fn guest_media_falls_back_to_replied_message() {
        let replied = Message {
            voice: meta("v"),
            ..Default::default()
        };
        let m = Message {
            text: Some("@bot".into()),
            reply_to_message: Some(Box::new(replied)),
            ..Default::default()
        };
        assert_eq!(extract_guest_media(&m).unwrap().file_id, "v");
        let m = Message {
            text: Some("@bot".into()),
            ..Default::default()
        };
        assert!(extract_guest_media(&m).is_none());
    }

    fn have_ffmpeg() -> bool {
        std::process::Command::new("ffmpeg")
            .arg("-version")
            .output()
            .is_ok()
    }

    #[tokio::test]
    async fn converts_probes_and_splits_real_media() {
        if !have_ffmpeg() {
            eprintln!("ffmpeg not installed; skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        // A 25 s round "video note"-like MP4 with a tone, like a Telegram circle.
        let src = dir.path().join("circle.mp4");
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=blue:s=240x240:d=25",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=25",
                "-shortest",
                "-c:v",
                "libx264",
                "-c:a",
                "aac",
                "-y",
            ])
            .arg(&src)
            .status()
            .unwrap();
        assert!(status.success());

        let t = Duration::from_secs(60);
        let ogg = dir.path().join("speech.ogg");
        to_speech_ogg(&src, &ogg, t).await.unwrap();
        let d = probe_duration(&ogg, t).await.unwrap();
        assert!((d - 25.0).abs() < 1.0, "duration {d}");

        let parts_dir = dir.path().join("parts");
        tokio::fs::create_dir(&parts_dir).await.unwrap();
        let parts = split(&ogg, &parts_dir, d, 10, t).await.unwrap();
        assert_eq!(parts.len(), 3, "{parts:?}");

        let short = split(&ogg, &parts_dir, d, 600, t).await.unwrap();
        assert_eq!(short, vec![ogg.clone()]);
    }

    #[tokio::test]
    async fn reports_missing_audio_track() {
        if !have_ffmpeg() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("silent.mp4");
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=red:s=64x64:d=1",
                "-c:v",
                "libx264",
                "-y",
            ])
            .arg(&src)
            .status()
            .unwrap();
        assert!(status.success());
        let err = to_speech_ogg(&src, &dir.path().join("o.ogg"), Duration::from_secs(30))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no audio track"), "{err}");
    }
}
