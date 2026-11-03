//! `audio.opus-v1`: the library's validate-and-transcode step as an ingest pipeline. Same
//! ffmpeg wrapper, limits and output (Ogg Opus 48 kHz stereo, ≤ 15 min) as local uploads.

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use ingest_core::{Pipeline, PipelineOutput, ProcessError};
use serde_json::{Value, json};

use crate::adapters::outbound::ffmpeg::Ffmpeg;
use crate::application::ports::{AudioError, AudioProcessor};
use crate::domain::library::MAX_DURATION;

pub const PIPELINE_ID: &str = "audio.opus-v1";

pub struct AudioOpusPipeline {
    audio: Ffmpeg,
}

impl AudioOpusPipeline {
    pub fn new(audio: Ffmpeg) -> Self {
        Self { audio }
    }
}

fn max_duration(params: &Value) -> Result<Duration, String> {
    match params.get("max_duration_s") {
        None => Ok(MAX_DURATION),
        Some(v) => match v.as_u64() {
            Some(s) if (1..=MAX_DURATION.as_secs()).contains(&s) => Ok(Duration::from_secs(s)),
            _ => Err(format!(
                "max_duration_s must be 1..={}",
                MAX_DURATION.as_secs()
            )),
        },
    }
}

fn audio_error(err: AudioError) -> ProcessError {
    match err {
        AudioError::Unsupported => ProcessError::permanent("not_audio", None),
        AudioError::Timeout => ProcessError::Resource("ffmpeg timed out".into()),
        AudioError::ResourceLimit(reason) => ProcessError::Resource(reason),
        // Couldn't start or read: infrastructure, not the file.
        AudioError::Failed(reason) => ProcessError::Transient(reason),
    }
}

#[async_trait]
impl Pipeline for AudioOpusPipeline {
    fn version(&self) -> &str {
        "audio.opus-v1/ogg-opus-48k-stereo-128k-20ms"
    }

    fn validate_params(&self, params: &Value) -> Result<(), String> {
        let Some(obj) = params.as_object() else {
            return Err("params must be an object".into());
        };
        if let Some(unknown) = obj.keys().find(|k| k.as_str() != "max_duration_s") {
            return Err(format!("unknown parameter {unknown}"));
        }
        max_duration(params).map(|_| ())
    }

    async fn run(
        &self,
        input: &Path,
        out_dir: &Path,
        params: &Value,
    ) -> Result<PipelineOutput, ProcessError> {
        let max = max_duration(params).map_err(|e| ProcessError::permanent("policy_denied", e))?;
        let probe = self.audio.probe(input).await.map_err(audio_error)?;
        if probe.audio_streams == 0 || probe.duration.is_zero() {
            return Err(ProcessError::permanent("not_audio", None));
        }
        if probe.duration > max {
            return Err(ProcessError::Permanent {
                code: "too_long".into(),
                detail: None,
                data: json!({ "max_secs": max.as_secs() }),
            });
        }
        let output = out_dir.join("output.ogg");
        self.audio
            .transcode(input, &output)
            .await
            .map_err(audio_error)?;
        // Measure what will be stored, not what the original claimed.
        let stored = self.audio.probe(&output).await.map_err(audio_error)?;
        let size = tokio::fs::metadata(&output)
            .await
            .map_err(|e| ProcessError::Transient(e.to_string()))?
            .len();
        if stored.audio_streams == 0 || stored.duration.is_zero() || size == 0 {
            return Err(ProcessError::permanent("processing_failed", None));
        }
        Ok(PipelineOutput {
            path: output,
            media_type: "audio/ogg".into(),
            extension: "ogg".into(),
            metadata: json!({ "duration_ms": stored.duration.as_millis() as u64 }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::outbound::ffmpeg::FfmpegSettings;

    fn pipeline() -> AudioOpusPipeline {
        AudioOpusPipeline::new(Ffmpeg::new(FfmpegSettings::default()))
    }

    #[test]
    fn validates_params() {
        let p = pipeline();
        assert!(p.validate_params(&json!({})).is_ok());
        assert!(p.validate_params(&json!({ "max_duration_s": 900 })).is_ok());
        assert!(
            p.validate_params(&json!({ "max_duration_s": 901 }))
                .is_err()
        );
        assert!(p.validate_params(&json!({ "bitrate": 1 })).is_err());
        assert!(p.validate_params(&json!([])).is_err());
    }

    async fn tone(dir: &Path, secs: u32) -> std::path::PathBuf {
        let path = dir.join("tone.mp3");
        let ok = tokio::process::Command::new("ffmpeg")
            .args(["-v", "error", "-f", "lavfi", "-i"])
            .arg(format!("sine=frequency=440:duration={secs}"))
            .arg(&path)
            .status()
            .await
            .unwrap()
            .success();
        assert!(ok);
        path
    }

    #[tokio::test]
    #[ignore = "needs ffmpeg/ffprobe on PATH"]
    async fn ffmpeg_pipeline_transcodes_and_enforces_duration() {
        let dir = tempfile::tempdir().unwrap();
        let input = tone(dir.path(), 3).await;
        let out = pipeline()
            .run(&input, dir.path(), &json!({}))
            .await
            .unwrap();
        assert_eq!(out.extension, "ogg");
        let ms = out.metadata["duration_ms"].as_u64().unwrap();
        assert!((2_800..=3_200).contains(&ms), "{ms}");

        let err = pipeline()
            .run(&input, dir.path(), &json!({ "max_duration_s": 1 }))
            .await
            .unwrap_err();
        assert!(matches!(err, ProcessError::Permanent { ref code, .. } if code == "too_long"));

        let text = dir.path().join("text.mp3");
        tokio::fs::write(&text, "not audio").await.unwrap();
        let err = pipeline()
            .run(&text, dir.path(), &json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, ProcessError::Permanent { ref code, .. } if code == "not_audio"));
    }
}
