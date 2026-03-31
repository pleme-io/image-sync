# image-sync

Smart container image cache synchronizer -- pulls from external registries only
when the remote digest differs from what is cached locally. Designed to run as a
Kubernetes CronJob alongside a Zot registry to eliminate Docker Hub rate limits
during scale testing.

## How It Works

```
For each image in config:
  1. crane digest <source>:<tag>          # HEAD request only, no pull
  2. crane digest <cache>/<path>:<tag>    # check local Zot
  3. If digests differ (or not cached):
       crane copy <source>:<tag> <cache>/<path>:<tag>
  4. If identical: skip (no API calls wasted)
```

This digest-first approach means the CronJob can run frequently (every 10 minutes)
without wasting Docker Hub API quota on images that have not changed.

## Config Format

YAML config loaded from `--config` flag, `IMAGE_SYNC_CONFIG` env var, or
`/etc/image-sync/config.yaml` (default for K8s CronJob mount).

```yaml
cache_registry: "image-cache.image-cache.svc.cluster.local:5000"

images:
  - source: "docker.io/akeyless/k8s-secrets-sidecar"
    tag: "0.35.1"
  - source: "docker.io/akeyless/k8s-webhook-server"
    tag: "latest"
  - source: "docker.io/library/nginx"
    tag: "1.27-alpine"
  - source: "ghcr.io/project-zot/zot-linux-amd64"
    tag: "v2.1.2"
    cache_as: "project-zot/zot-linux-amd64"  # optional path override

settings:
  concurrency: 2
  pull_timeout_secs: 300
  skip_on_error: false  # set true to not fail the CronJob on partial failures
```

## Docker Hub Authentication

For Docker Hub images, mount a Docker config secret with your credentials:

```yaml
# K8s CronJob env
- name: DOCKER_CONFIG
  value: /tmp/docker

# Volume mount
volumes:
  - name: docker-auth
    secret:
      secretName: dockerhub-auth
```

The `dockerhub-auth` secret should contain a `.dockerconfigjson` file with a
Docker Hub personal access token. Without auth, Docker Hub allows only 100
pulls per 6 hours from anonymous IPs.

## Output

Writes a JSON report to stdout:

```json
{
  "timestamp": "2026-03-30T...",
  "cache_registry": "image-cache.image-cache.svc.cluster.local:5000",
  "summary": {
    "pulled": 1,
    "already_cached": 4,
    "failed": 0,
    "total": 5
  },
  "results": [
    {
      "image": "docker.io/akeyless/k8s-secrets-sidecar",
      "tag": "0.35.1",
      "action": "already_cached",
      "remote_digest": "sha256:abc...",
      "cached_digest": "sha256:abc...",
      "duration_ms": 320,
      "error": null
    }
  ]
}
```

## Container Image

Published at `ghcr.io/pleme-io/image-sync` (multi-arch: amd64, arm64).
Built with substrate's `rust-tool-image-flake.nix` pattern -- the Docker image
includes `crane` on PATH for registry operations.

```sh
# Build locally
nix build

# Build and push Docker images
nix run .#release

# Run locally
nix run -- --config config.yaml --dry-run
```

## Runtime Dependencies

- **crane** (included in Docker image via `extraContents`)
- **skopeo** (fallback, used if crane fails)
- Network access to source registries and the cache registry

## Kubernetes Deployment

Runs as a CronJob in the `image-cache` namespace. See
`k8s/clusters/scale-test/infrastructure/image-sync/` for the full manifest set:

- `cronjob.yaml` -- runs every 10 minutes with `Forbid` concurrency
- `configmap.yaml` -- image-sync YAML config
- `dockerhub-secret.enc.yaml` -- SOPS-encrypted Docker Hub credentials
- `job-initial-sync.yaml` -- one-shot Job for initial cache population
