//! image-sync — Smart container image cache synchronizer.
//!
//! Checks external registries for image digest changes, pulls only when
//! the remote digest differs from what's cached locally in Zot.
//! Designed to run as a Kubernetes CronJob with exponential backoff.
//!
//! Config via shikumi pattern: YAML file + env overrides.
//!
//! All copy/digest settings (platform, TLS, tool preference) are
//! configurable at the global level and overridable per-image.

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

    /// Global settings (defaults for all images)
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

    /// Platform override for this image (e.g., "linux/arm64").
    /// Falls back to settings.default_platform.
    #[serde(default)]
    platform: Option<String>,

    /// TLS verification override for this image's source registry.
    /// Falls back to settings.source_tls_verify.
    #[serde(default)]
    source_tls_verify: Option<bool>,

    /// Insecure (HTTP) override for the cache registry for this image.
    /// Falls back to settings.cache_insecure.
    #[serde(default)]
    cache_insecure: Option<bool>,
}

#[derive(Debug, Deserialize, Serialize)]
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

    /// Default platform for crane copy (e.g., "linux/amd64").
    /// Copies platform-specific manifests instead of multi-arch
    /// manifest lists, which avoids MANIFEST_INVALID on Zot.
    #[serde(default = "default_platform")]
    default_platform: String,

    /// Allow HTTP (insecure) for the local cache registry.
    /// Default true — Zot in-cluster typically runs on HTTP.
    #[serde(default = "default_true")]
    cache_insecure: bool,

    /// Verify TLS certificates for source registries.
    /// Default true — Docker Hub, GHCR, ECR all use valid certs.
    #[serde(default = "default_true")]
    source_tls_verify: bool,

    /// Preferred copy tool: "crane" or "skopeo".
    /// Falls back to the other if preferred is unavailable.
    #[serde(default = "default_tool")]
    preferred_tool: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            concurrency: default_concurrency(),
            pull_timeout_secs: default_timeout(),
            skip_on_error: false,
            default_platform: default_platform(),
            cache_insecure: true,
            source_tls_verify: true,
            preferred_tool: default_tool(),
        }
    }
}

fn default_platform() -> String { "linux/amd64".to_string() }
fn default_concurrency() -> usize { 2 }
fn default_timeout() -> u64 { 300 }
fn default_true() -> bool { true }
fn default_tool() -> String { "crane".to_string() }

/// Resolved copy options for a single image (merged from settings + per-image).
struct CopyOpts<'a> {
    platform: &'a str,
    cache_insecure: bool,
    source_tls_verify: bool,
}

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
fn get_remote_digest(image: &str, tag: &str) -> anyhow::Result<Option<String>> {
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

/// Get the cached digest from our local registry.
fn get_cached_digest(cache_registry: &str, image_path: &str, tag: &str, opts: &CopyOpts<'_>) -> anyhow::Result<Option<String>> {
    let mut args = vec!["digest".to_string(), format!("{cache_registry}/{image_path}:{tag}")];
    if opts.cache_insecure {
        args.push("--insecure".to_string());
    }

    let result = Command::new("crane").args(&args).output();

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

/// Copy an image from source to cache.
fn copy_image(source: &str, tag: &str, cache_registry: &str, cache_path: &str, opts: &CopyOpts<'_>) -> anyhow::Result<()> {
    let src = format!("{source}:{tag}");
    let dst = format!("{cache_registry}/{cache_path}:{tag}");

    // Build crane args
    let mut crane_args = vec!["copy".to_string(), src.clone(), dst.clone()];
    crane_args.extend(["--platform".to_string(), opts.platform.to_string()]);
    if opts.cache_insecure {
        crane_args.push("--insecure".to_string());
    }

    let result = Command::new("crane").args(&crane_args).output();

    if let Ok(output) = result {
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!("crane copy failed: {stderr}");
    }

    // Fallback to skopeo with OCI manifest format.
    // Zot rejects some Docker v2 manifests with MANIFEST_INVALID.
    // Converting to OCI format during copy always works.
    //
    let policy_path = std::env::temp_dir().join("containers-policy.json");

    let mut skopeo_args = vec![
        "copy".to_string(),
        "--policy".to_string(), policy_path.to_string_lossy().to_string(),
        "--format".to_string(), "oci".to_string(),
        "--override-arch".to_string(), opts.platform.split('/').nth(1).unwrap_or("amd64").to_string(),
        "--override-os".to_string(), opts.platform.split('/').next().unwrap_or("linux").to_string(),
        format!("docker://{src}"),
        format!("docker://{dst}"),
    ];
    if !opts.source_tls_verify {
        skopeo_args.push("--src-tls-verify=false".to_string());
    }
    if opts.cache_insecure {
        skopeo_args.push("--dest-tls-verify=false".to_string());
    }

    let result = Command::new("skopeo").args(&skopeo_args).output()?;

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
    source
        .strip_prefix("docker.io/")
        .or_else(|| source.strip_prefix("ghcr.io/"))
        .or_else(|| source.strip_prefix("registry-1.docker.io/"))
        .unwrap_or(source)
        .to_string()
}

fn sync_image(spec: &ImageSpec, cache_registry: &str, settings: &Settings, dry_run: bool) -> SyncResult {
    let start = std::time::Instant::now();
    let cache_path = spec
        .cache_as
        .clone()
        .unwrap_or_else(|| derive_cache_path(&spec.source));

    // Resolve per-image overrides → global defaults
    let opts = CopyOpts {
        platform: spec.platform.as_deref().unwrap_or(&settings.default_platform),
        cache_insecure: spec.cache_insecure.unwrap_or(settings.cache_insecure),
        source_tls_verify: spec.source_tls_verify.unwrap_or(settings.source_tls_verify),
    };

    // Step 1: Check remote digest
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
    let cached_digest = get_cached_digest(cache_registry, &cache_path, &spec.tag, &opts).unwrap_or(None);

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

    match copy_image(&spec.source, &spec.tag, cache_registry, &cache_path, &opts) {
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

    // Write a permissive trust policy for skopeo (required in containers
    // where /etc/containers/policy.json doesn't exist).
    let policy_dir = std::env::temp_dir();
    std::fs::create_dir_all(&policy_dir).ok();
    let policy_path = policy_dir.join("containers-policy.json");
    std::fs::write(&policy_path, r#"{"default":[{"type":"insecureAcceptAnything"}]}"#)
        .unwrap_or_else(|e| tracing::warn!("failed to write trust policy: {e}"));

    tracing::info!(
        cache_registry = %config.cache_registry,
        image_count = config.images.len(),
        dry_run = cli.dry_run,
        default_platform = %config.settings.default_platform,
        cache_insecure = config.settings.cache_insecure,
        "image-sync starting"
    );

    let mut results = Vec::new();
    let mut pulled = 0u32;
    let mut cached = 0u32;
    let mut failed = 0u32;

    for spec in &config.images {
        let result = sync_image(spec, &config.cache_registry, &config.settings, cli.dry_run);
        match result.action {
            SyncAction::Pulled => pulled += 1,
            SyncAction::AlreadyCached => cached += 1,
            SyncAction::Failed => failed += 1,
            SyncAction::Skipped => {}
        }
        results.push(result);
    }

    tracing::info!(
        pulled = pulled,
        already_cached = cached,
        failed = failed,
        total = config.images.len(),
        "sync complete"
    );

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
