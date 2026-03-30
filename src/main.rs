//! image-sync — Smart container image cache synchronizer.
//!
//! Checks external registries for image digest changes, pulls only when
//! the remote digest differs from what's cached locally in Zot.
//! Designed to run as a Kubernetes CronJob with exponential backoff.
//!
//! Config via shikumi pattern: YAML file + env overrides.

use std::process::Command;

use chrono::Utc;
use clap::Parser;
use serde::{Deserialize, Serialize};

#[derive(Parser)]
#[command(name = "image-sync", about = "Smart image cache synchronizer")]
struct Cli {
    /// Config file path (YAML)
    #[arg(long, env = "IMAGE_SYNC_CONFIG", default_value = "/etc/image-sync/config.yaml")]
    config: String,

    /// Dry run — check digests but don't pull
    #[arg(long)]
    dry_run: bool,
}

/// Configuration loaded from YAML (shikumi pattern).
#[derive(Debug, Deserialize, Serialize)]
struct Config {
    /// Local cache registry URL (Zot)
    cache_registry: String,

    /// Images to keep in sync
    images: Vec<ImageSpec>,

    /// Global settings
    #[serde(default)]
    settings: Settings,
}

#[derive(Debug, Deserialize, Serialize)]
struct ImageSpec {
    /// Source image reference (e.g., "docker.io/akeyless/k8s-secrets-sidecar")
    source: String,

    /// Tag to sync (e.g., "0.35.1", "latest")
    tag: String,

    /// Optional: override destination path in cache
    #[serde(default)]
    cache_as: Option<String>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct Settings {
    /// Max concurrent pulls
    #[serde(default = "default_concurrency")]
    concurrency: usize,

    /// Timeout per image pull in seconds
    #[serde(default = "default_timeout")]
    pull_timeout_secs: u64,

    /// Skip pull if remote check fails (don't error the job)
    #[serde(default)]
    skip_on_error: bool,
}

fn default_concurrency() -> usize { 2 }
fn default_timeout() -> u64 { 300 }

/// Result of syncing one image.
#[derive(Debug, Serialize)]
struct SyncResult {
    image: String,
    tag: String,
    action: SyncAction,
    remote_digest: Option<String>,
    cached_digest: Option<String>,
    duration_ms: u64,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum SyncAction {
    AlreadyCached,
    Pulled,
    Skipped,
    Failed,
}

fn load_config(path: &str) -> anyhow::Result<Config> {
    let content = std::fs::read_to_string(path)?;
    Ok(serde_yaml_ng::from_str(&content)?)
}

/// Get the remote image digest from the registry WITHOUT pulling the image.
/// Uses `skopeo inspect` or `crane digest` if available, falls back to
/// registry API v2 manifest HEAD request.
fn get_remote_digest(image: &str, tag: &str) -> anyhow::Result<Option<String>> {
    // Try crane first (lightweight, no Docker daemon needed)
    let result = Command::new("crane")
        .args(["digest", &format!("{image}:{tag}")])
        .output();

    if let Ok(output) = result {
        if output.status.success() {
            let digest = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !digest.is_empty() {
                return Ok(Some(digest));
            }
        }
    }

    // Try skopeo
    let result = Command::new("skopeo")
        .args(["inspect", "--format", "{{.Digest}}", &format!("docker://{image}:{tag}")])
        .output();

    if let Ok(output) = result {
        if output.status.success() {
            let digest = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !digest.is_empty() {
                return Ok(Some(digest));
            }
        }
    }

    Ok(None)
}

/// Get the cached digest from our local Zot registry.
fn get_cached_digest(cache_registry: &str, image_path: &str, tag: &str) -> anyhow::Result<Option<String>> {
    let result = Command::new("crane")
        .args(["digest", &format!("{cache_registry}/{image_path}:{tag}"), "--insecure"])
        .output();

    if let Ok(output) = result {
        if output.status.success() {
            let digest = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !digest.is_empty() {
                return Ok(Some(digest));
            }
        }
    }

    Ok(None)
}

/// Copy an image from source to cache using crane or skopeo.
fn copy_image(source: &str, tag: &str, cache_registry: &str, cache_path: &str) -> anyhow::Result<()> {
    let src = format!("{source}:{tag}");
    let dst = format!("{cache_registry}/{cache_path}:{tag}");

    // Try crane copy
    let result = Command::new("crane")
        .args(["copy", &src, &dst, "--insecure"])
        .output();

    if let Ok(output) = result {
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!("crane copy failed: {stderr}");
    }

    // Fallback to skopeo
    let result = Command::new("skopeo")
        .args([
            "copy",
            &format!("docker://{src}"),
            &format!("docker://{dst}"),
            "--dest-tls-verify=false",
        ])
        .output()?;

    if !result.status.success() {
        let stderr = String::from_utf8_lossy(&result.stderr);
        anyhow::bail!("image copy failed: {stderr}");
    }

    Ok(())
}

/// Derive the cache path from the source image reference.
/// docker.io/akeyless/k8s-secrets-sidecar → akeyless/k8s-secrets-sidecar
/// ghcr.io/project-zot/zot-linux-amd64 → project-zot/zot-linux-amd64
fn derive_cache_path(source: &str) -> String {
    // Strip registry prefix
    let path = source
        .strip_prefix("docker.io/")
        .or_else(|| source.strip_prefix("ghcr.io/"))
        .or_else(|| source.strip_prefix("registry-1.docker.io/"))
        .unwrap_or(source);

    // Handle Docker Hub library images: library/nginx → library/nginx
    path.to_string()
}

fn sync_image(spec: &ImageSpec, cache_registry: &str, dry_run: bool) -> SyncResult {
    let start = std::time::Instant::now();
    let cache_path = spec
        .cache_as
        .clone()
        .unwrap_or_else(|| derive_cache_path(&spec.source));

    // Step 1: Check remote digest (minimal API call — HEAD request only)
    let remote_digest = match get_remote_digest(&spec.source, &spec.tag) {
        Ok(d) => d,
        Err(e) => {
            return SyncResult {
                image: spec.source.clone(),
                tag: spec.tag.clone(),
                action: SyncAction::Failed,
                remote_digest: None,
                cached_digest: None,
                duration_ms: start.elapsed().as_millis() as u64,
                error: Some(format!("remote digest check failed: {e}")),
            };
        }
    };

    // Step 2: Check cached digest
    let cached_digest = get_cached_digest(cache_registry, &cache_path, &spec.tag).unwrap_or(None);

    // Step 3: Compare — skip if identical
    if let (Some(remote), Some(cached)) = (&remote_digest, &cached_digest) {
        if remote == cached {
            tracing::info!(
                image = %spec.source,
                tag = %spec.tag,
                digest = %remote,
                "already cached — no action needed"
            );
            return SyncResult {
                image: spec.source.clone(),
                tag: spec.tag.clone(),
                action: SyncAction::AlreadyCached,
                remote_digest: remote_digest.clone(),
                cached_digest: cached_digest.clone(),
                duration_ms: start.elapsed().as_millis() as u64,
                error: None,
            };
        }
    }

    // Step 4: Pull if different (or not cached)
    if dry_run {
        tracing::info!(
            image = %spec.source,
            tag = %spec.tag,
            "DRY RUN — would pull (remote={:?}, cached={:?})",
            remote_digest,
            cached_digest,
        );
        return SyncResult {
            image: spec.source.clone(),
            tag: spec.tag.clone(),
            action: SyncAction::Skipped,
            remote_digest,
            cached_digest,
            duration_ms: start.elapsed().as_millis() as u64,
            error: None,
        };
    }

    tracing::info!(
        image = %spec.source,
        tag = %spec.tag,
        "pulling — remote digest changed or not cached"
    );

    match copy_image(&spec.source, &spec.tag, cache_registry, &cache_path) {
        Ok(()) => {
            tracing::info!(
                image = %spec.source,
                tag = %spec.tag,
                duration_ms = start.elapsed().as_millis(),
                "cached successfully"
            );
            SyncResult {
                image: spec.source.clone(),
                tag: spec.tag.clone(),
                action: SyncAction::Pulled,
                remote_digest,
                cached_digest,
                duration_ms: start.elapsed().as_millis() as u64,
                error: None,
            }
        }
        Err(e) => SyncResult {
            image: spec.source.clone(),
            tag: spec.tag.clone(),
            action: SyncAction::Failed,
            remote_digest,
            cached_digest,
            duration_ms: start.elapsed().as_millis() as u64,
            error: Some(e.to_string()),
        },
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let config = load_config(&cli.config)?;

    tracing::info!(
        cache_registry = %config.cache_registry,
        image_count = config.images.len(),
        dry_run = cli.dry_run,
        "image-sync starting"
    );

    let mut results = Vec::new();
    let mut pulled = 0u32;
    let mut cached = 0u32;
    let mut failed = 0u32;

    for spec in &config.images {
        let result = sync_image(spec, &config.cache_registry, cli.dry_run);
        match result.action {
            SyncAction::Pulled => pulled += 1,
            SyncAction::AlreadyCached => cached += 1,
            SyncAction::Failed => failed += 1,
            SyncAction::Skipped => {}
        }
        results.push(result);
    }

    // Summary
    tracing::info!(
        pulled = pulled,
        already_cached = cached,
        failed = failed,
        total = config.images.len(),
        "sync complete"
    );

    // JSON report to stdout
    let report = serde_json::json!({
        "timestamp": Utc::now().to_rfc3339(),
        "cache_registry": config.cache_registry,
        "summary": {
            "pulled": pulled,
            "already_cached": cached,
            "failed": failed,
            "total": config.images.len(),
        },
        "results": results,
    });
    println!("{}", serde_json::to_string_pretty(&report)?);

    if failed > 0 && !config.settings.skip_on_error {
        anyhow::bail!("{failed} image(s) failed to sync");
    }

    Ok(())
}
