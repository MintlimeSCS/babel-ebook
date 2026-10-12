//! Job-level checkpoint persistence for resumable translations.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::{Config, OutputMode};
use crate::core::BabelEbookError;

/// Lifecycle status of a single chapter within a checkpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChapterStatus {
    /// Chapter has not been processed yet.
    Pending,
    /// Chapter was translated successfully.
    Completed,
    /// Chapter translation failed.
    Failed,
}

/// Per-chapter checkpoint entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChapterCheckpoint {
    /// Index of the chapter in the source book spine.
    pub index: usize,
    /// Href path of the chapter document.
    pub href: String,
    /// Current translation status.
    pub status: ChapterStatus,
    /// Translated chapter content, when available.
    pub content: Option<Vec<u8>>,
    /// Error message, when translation failed.
    pub error: Option<String>,
}

/// Full checkpoint for one translation job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    /// Unique identifier for the translation job.
    pub job_id: String,
    /// Hash of the source file used to detect changes.
    pub source_hash: String,
    /// Legacy exact-settings identity; incompatible checkpoints are preserved.
    #[serde(default)]
    pub translation_signature: String,
    /// R05 semantic settings identity; empty in legacy R02/R04 checkpoints.
    #[serde(default)]
    pub compatibility_signature: String,
    /// Original path to the source file.
    #[serde(default)]
    pub source_path: String,
    /// Checkpoint entries for each chapter.
    pub chapters: Vec<ChapterCheckpoint>,
}

/// On-disk JSON store for checkpoints.
#[derive(Debug, Clone)]
pub struct CheckpointStore {
    /// Directory where checkpoint files are stored.
    dir: PathBuf,
}

impl CheckpointStore {
    /// Hash only output-affecting settings, never credentials or destination paths.
    #[must_use]
    pub fn translation_signature(config: &Config) -> String {
        hex::encode(Sha256::digest(
            Self::translation_settings(config).to_string().as_bytes(),
        ))
    }

    /// Compare completed translations independently of request token limits.
    /// Legacy checkpoints must first match their exact original settings.
    ///
    /// # Panics
    /// Panics if the internal settings schema changes without updating its field indices.
    #[must_use]
    pub fn compatibility_signature(config: &Config) -> String {
        let mut settings = Self::translation_settings(config);
        let array = settings.as_array_mut().expect("settings are an array");
        array[0] = "epub-resume-compatibility-r05".into();
        if let Some(provider) = array[6].as_array_mut() {
            provider.remove(3);
        }
        array.remove(8);
        array.remove(7);
        hex::encode(Sha256::digest(settings.to_string().as_bytes()))
    }

    fn translation_settings(config: &Config) -> serde_json::Value {
        let chapters: std::collections::BTreeMap<_, _> = config
            .chapter_prompts
            .keys()
            .map(|href| (href, config.system_prompt_for_chapter(href)))
            .collect();
        let provider = config.provider_config.as_ref();
        serde_json::json!([
            "epub-translation-v2",
            config.source_lang,
            config.target_lang,
            config.provider,
            config.model,
            config.base_url,
            provider.map(|p| (
                &p.name,
                &p.default_model,
                &p.base_url,
                p.max_tokens,
                p.temperature
            )),
            config.max_input_tokens,
            config.max_output_tokens,
            config.temperature,
            config.output_mode,
            config.translation_scope,
            config.system_prompt(),
            chapters,
            config.refine,
            config.refine_prompt(),
            config.exclude_selectors,
            config.translate_tags,
            config.translate_attributes,
            config.preserve_classes,
            config.output_font,
            config.skip_doc_patterns
        ])
    }
    /// Create the store, ensuring the directory exists.
    ///
    /// # Errors
    ///
    /// Returns `BabelEbookError::Anyhow` if the checkpoint directory cannot be
    /// created.
    pub fn new(dir: PathBuf) -> Result<Self, BabelEbookError> {
        std::fs::create_dir_all(&dir).map_err(|e| {
            BabelEbookError::Anyhow(anyhow::anyhow!(
                "failed to create checkpoint directory {}: {e}",
                dir.display()
            ))
        })?;
        Ok(Self { dir })
    }

    /// Load a checkpoint by job id if it exists.
    #[must_use]
    pub fn load(&self, job_id: &str) -> Option<Checkpoint> {
        self.load_checked(job_id).unwrap_or_else(|error| {
            tracing::warn!(job_id, %error, "checkpoint could not be loaded");
            None
        })
    }

    /// Load without treating corrupt or inaccessible existing data as a new job.
    pub fn load_checked(&self, job_id: &str) -> Result<Option<Checkpoint>, BabelEbookError> {
        let path = self.path(job_id);
        match std::fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content).map(Some).map_err(|error| {
                BabelEbookError::Configuration(format!(
                    "Cannot parse checkpoint {}; existing progress was preserved: {error}",
                    path.display()
                ))
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(BabelEbookError::Configuration(format!(
                "Cannot read checkpoint {}; existing progress was preserved: {error}",
                path.display()
            ))),
        }
    }

    /// Retain a pre-run snapshot; keep the latest three snapshots per job.
    /// Backups are excluded from the desktop list by their subdirectory.
    pub fn backup(&self, job_id: &str) -> Result<(), BabelEbookError> {
        let source = self.path(job_id);
        let result = (|| -> std::io::Result<()> {
            if !source.exists() {
                return Ok(());
            }
            let directory = self.dir.join("backups");
            std::fs::create_dir_all(&directory)?;
            let time = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let destination = directory.join(format!("{job_id}-{time}.json"));
            std::fs::copy(&source, &destination)?;
            let prefix = format!("{job_id}-");
            let mut snapshots: Vec<_> = std::fs::read_dir(directory)?
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
                .map(|entry| entry.path())
                .collect();
            snapshots.sort();
            for path in snapshots.iter().take(snapshots.len().saturating_sub(3)) {
                if let Err(error) = std::fs::remove_file(path) {
                    tracing::warn!(%error, "Could not prune an old checkpoint backup");
                }
            }
            Ok(())
        })();
        result.map_err(|error| BabelEbookError::Configuration(format!("Cannot back up checkpoint {}; translation stopped to preserve existing progress: {error}", source.display())))
    }

    /// Persist a checkpoint atomically (write temp + rename).
    pub fn save(&self, checkpoint: &Checkpoint) -> Result<(), BabelEbookError> {
        let path = self.path(&checkpoint.job_id);
        let tmp = path.with_extension("tmp");
        let content = serde_json::to_string_pretty(checkpoint)
            .map_err(|e| BabelEbookError::Anyhow(anyhow::anyhow!("serialize checkpoint: {e}")))?;
        std::fs::write(&tmp, content).map_err(|e| {
            BabelEbookError::Anyhow(anyhow::anyhow!(
                "write checkpoint tmp {}: {e}",
                tmp.display()
            ))
        })?;
        std::fs::rename(&tmp, &path).map_err(|e| {
            BabelEbookError::Anyhow(anyhow::anyhow!("rename checkpoint {}: {e}", path.display()))
        })
    }

    /// Persist a checkpoint atomically using async I/O.
    ///
    /// The sync `save` API is preserved for callers that do not need async
    /// scheduling; this method performs the same operation with
    /// `tokio::fs::write` + `tokio::fs::rename` so it does not block the
    /// runtime thread.
    pub async fn save_async(&self, checkpoint: &Checkpoint) -> Result<(), BabelEbookError> {
        let path = self.path(&checkpoint.job_id);
        let tmp = path.with_extension("tmp");
        let content = serde_json::to_string_pretty(checkpoint)
            .map_err(|e| BabelEbookError::Anyhow(anyhow::anyhow!("serialize checkpoint: {e}")))?;
        tokio::fs::write(&tmp, content).await.map_err(|e| {
            BabelEbookError::Anyhow(anyhow::anyhow!(
                "write checkpoint tmp {}: {e}",
                tmp.display()
            ))
        })?;
        tokio::fs::rename(&tmp, &path).await.map_err(|e| {
            BabelEbookError::Anyhow(anyhow::anyhow!("rename checkpoint {}: {e}", path.display()))
        })
    }

    /// Generate a short stable job id from the source path and a hash of the
    /// translation parameters.
    ///
    /// The id is deterministic for the same source path, target language,
    /// output mode, provider, and model so that repeated runs of the same book
    /// with the same settings resume the same checkpoint.
    #[must_use]
    pub fn generate_job_id(
        source: &Path,
        target_lang: &str,
        output_mode: OutputMode,
        provider: &str,
        model: &str,
    ) -> String {
        let params_hash = Self::hash_params(target_lang, output_mode, provider, model);
        let input = format!("{}|{params_hash}", source.display());
        let mut hasher = Sha256::new();
        hasher.update(input.as_bytes());
        let hash = hasher.finalize();
        hex::encode(&hash[..8])
    }

    /// Compute a short SHA-256 hex hash of the translation parameters that
    /// should affect checkpoint identity.
    fn hash_params(
        target_lang: &str,
        output_mode: OutputMode,
        provider: &str,
        model: &str,
    ) -> String {
        let input = format!("{target_lang}|{}|{provider}|{model}", output_mode.as_str());
        let mut hasher = Sha256::new();
        hasher.update(input.as_bytes());
        let hash = hasher.finalize();
        hex::encode(&hash[..8])
    }

    /// Compute a SHA-256 hex hash of the source file contents.
    ///
    /// # Errors
    ///
    /// Returns `BabelEbookError::Anyhow` if the file cannot be read.
    pub fn source_hash(path: &Path) -> Result<String, BabelEbookError> {
        let file = std::fs::File::open(path)
            .map_err(|e| BabelEbookError::Anyhow(anyhow::anyhow!("open source for hash: {e}")))?;
        let mut reader = std::io::BufReader::new(file);
        let mut hasher = Sha256::new();
        std::io::copy(&mut reader, &mut hasher)
            .map_err(|e| BabelEbookError::Anyhow(anyhow::anyhow!("hash source file: {e}")))?;
        Ok(hex::encode(hasher.finalize()))
    }

    fn path(&self, job_id: &str) -> PathBuf {
        self.dir.join(format!("{job_id}.json"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoint_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(dir.path().to_path_buf()).unwrap();
        let cp = Checkpoint {
            job_id: "job-1".into(),
            source_hash: "hash".into(),
            translation_signature: "settings".into(),
            compatibility_signature: String::new(),
            source_path: "input/book.epub".into(),
            chapters: vec![
                ChapterCheckpoint {
                    index: 0,
                    href: "ch01.xhtml".into(),
                    status: ChapterStatus::Completed,
                    content: Some(b"<p>translated</p>".to_vec()),
                    error: None,
                },
                ChapterCheckpoint {
                    index: 1,
                    href: "ch02.xhtml".into(),
                    status: ChapterStatus::Failed,
                    content: None,
                    error: Some("api error".into()),
                },
            ],
        };
        store.save(&cp).unwrap();
        let loaded = store.load("job-1").expect("checkpoint exists");
        assert_eq!(loaded.job_id, "job-1");
        assert_eq!(loaded.chapters.len(), 2);
        assert!(loaded.chapters[0].content.is_some());
        assert_eq!(loaded.chapters[1].status, ChapterStatus::Failed);
    }

    #[test]
    fn generate_job_id_properties() {
        let source_a = Path::new("input/book-a.epub");
        let id_a = CheckpointStore::generate_job_id(
            source_a,
            "zh-CN",
            OutputMode::Bilingual,
            "deepseek",
            "deepseek-chat",
        );
        assert!(!id_a.is_empty());
        assert!(id_a.chars().all(|c| c.is_ascii_hexdigit()));

        // Same source + same params must produce the same id.
        let id_a2 = CheckpointStore::generate_job_id(
            source_a,
            "zh-CN",
            OutputMode::Bilingual,
            "deepseek",
            "deepseek-chat",
        );
        assert_eq!(id_a, id_a2);

        // Different source produces a different id.
        let source_b = Path::new("input/book-b.epub");
        let id_b = CheckpointStore::generate_job_id(
            source_b,
            "zh-CN",
            OutputMode::Bilingual,
            "deepseek",
            "deepseek-chat",
        );
        assert_ne!(id_a, id_b);
        assert!(id_b.chars().all(|c| c.is_ascii_hexdigit()));

        // Different target language produces a different id.
        let id_diff_lang = CheckpointStore::generate_job_id(
            source_a,
            "en",
            OutputMode::Bilingual,
            "deepseek",
            "deepseek-chat",
        );
        assert_ne!(id_a, id_diff_lang);

        // Different output mode produces a different id.
        let id_diff_mode = CheckpointStore::generate_job_id(
            source_a,
            "zh-CN",
            OutputMode::TranslationOnly,
            "deepseek",
            "deepseek-chat",
        );
        assert_ne!(id_a, id_diff_mode);

        // Different provider produces a different id.
        let id_diff_provider = CheckpointStore::generate_job_id(
            source_a,
            "zh-CN",
            OutputMode::Bilingual,
            "openai",
            "deepseek-chat",
        );
        assert_ne!(id_a, id_diff_provider);

        // Different model produces a different id.
        let id_diff_model = CheckpointStore::generate_job_id(
            source_a,
            "zh-CN",
            OutputMode::Bilingual,
            "deepseek",
            "deepseek-coder",
        );
        assert_ne!(id_a, id_diff_model);
    }

    #[test]
    fn generate_job_id_is_stable_across_time() {
        let source = Path::new("input/book-a.epub");
        let id1 = CheckpointStore::generate_job_id(
            source,
            "zh-CN",
            OutputMode::Bilingual,
            "deepseek",
            "deepseek-chat",
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
        let id2 = CheckpointStore::generate_job_id(
            source,
            "zh-CN",
            OutputMode::Bilingual,
            "deepseek",
            "deepseek-chat",
        );
        assert_eq!(id1, id2);
    }
}
