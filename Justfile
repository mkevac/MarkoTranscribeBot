# MarkoTranscribeBot - Task Runner
# Use `just <recipe>` or simply `just` to list all available recipes

# Settings
set dotenv-load
set shell := ["bash", "-uc"]

# Variables
image_name := "mkevac/markotranscribebot"
platforms := "linux/amd64,linux/arm64"
binary_name := "markotranscribebot"
git_tag := `git describe --tags --exact-match 2>/dev/null | sed 's/^v//' || echo "latest"`
has_tag := `git describe --tags --exact-match >/dev/null 2>&1 && echo "true" || echo "false"`

# Default recipe - list all available recipes
default:
    @just --list

# === Build & Test ===

# Build the release binary
build:
    cargo build --release

# Run tests (media tests need ffmpeg installed)
test:
    cargo test

# Run clippy with warnings as errors, and check formatting
lint:
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings

# Format the code
fmt:
    cargo fmt

# Remove build artifacts
clean:
    cargo clean

# === Docker ===

# Build Docker image locally with git tag detection
docker:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ "{{has_tag}}" = "true" ]; then
        TAG="{{git_tag}}"
        echo "Building Docker image with tag: $TAG"
        docker buildx build -t {{image_name}}:$TAG -t {{image_name}}:latest --load .
    else
        echo "No Git tag found. Building Docker image with 'latest' tag."
        docker buildx build -t {{image_name}}:latest --load .
    fi

# Build and push Docker image for multiple platforms (requires git tag)
push:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ "{{has_tag}}" != "true" ]; then
        echo "ERROR: Cannot push without a git tag on current commit"
        echo "Create a tag first with: just bump"
        exit 1
    fi
    TAG="{{git_tag}}"
    echo "Building and pushing Docker image with tag: $TAG"
    docker buildx build --platform {{platforms}} -t {{image_name}}:$TAG -t {{image_name}}:latest --push .

# === Docker Compose ===

# Start services
run:
    docker compose up -d

# Stop services
stop:
    docker compose down

# View logs (optionally specify service name)
logs service="":
    docker compose logs -f {{service}}

# === Local Development ===

# Run locally against the cloud Bot API (20 MB download limit)
run-local:
    DATA_DIR=./data BOT_API_URL=https://api.telegram.org cargo run

# Log the bot out of the cloud Bot API (required once before using the local server)
logout-cloud:
    curl -s "https://api.telegram.org/bot${TELEGRAM_BOT_API_TOKEN}/logOut"; echo

# === Versioning ===

# Bump minor version and create new git tag (e.g., v0.1.0 → v0.2.0)
bump:
    #!/usr/bin/env bash
    set -euo pipefail
    LATEST_TAG=$(git describe --tags --abbrev=0 2>/dev/null || echo "")
    if [ -z "$LATEST_TAG" ]; then
        echo "ERROR: No existing tags found"
        echo "Create the first tag manually: git tag v0.1.0"
        exit 1
    fi
    VERSION="${LATEST_TAG#v}"
    IFS='.' read -r MAJOR MINOR PATCH <<< "$VERSION"
    if ! [[ "$MAJOR" =~ ^[0-9]+$ ]] || ! [[ "$MINOR" =~ ^[0-9]+$ ]]; then
        echo "ERROR: Invalid tag format: $LATEST_TAG"
        exit 1
    fi
    NEW_TAG="v${MAJOR}.$((MINOR + 1)).0"
    if git rev-parse "$NEW_TAG" >/dev/null 2>&1; then
        echo "ERROR: Tag $NEW_TAG already exists"
        exit 1
    fi
    git tag "$NEW_TAG"
    echo "Created tag $NEW_TAG (from $LATEST_TAG)"
    echo "Next: git push origin $NEW_TAG && just push"

# === Info ===

# Show git tag and version information
version:
    @echo "Current commit tag: {{git_tag}}"
    @echo "Has tag: {{has_tag}}"
    @echo "Latest tag: $(git describe --tags --abbrev=0 2>/dev/null || echo 'none')"
    @echo "Image name: {{image_name}}"
