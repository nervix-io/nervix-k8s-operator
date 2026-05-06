set shell := ["bash", "-euo", "pipefail", "-c"]

image_repo := "ghcr.io/nervix-io/nervix-k8s-operator"
image_tag := image_repo + ":dev"

fmt:
    cargo fmt

fmt-check:
    cargo fmt --check

lint:
    cargo clippy --all-features --all-targets -- -D warnings

audit:
    cargo audit

test:
    cargo test --all-features --all-targets

validate: fmt lint audit test

validate-ci: fmt-check lint audit test

docker-build tag=image_tag platform="linux/amd64" push="false" cache_from="" cache_to="":
    #!/usr/bin/env bash
    set -euo pipefail
    normalized_platform="{{ platform }}"
    if [[ "${normalized_platform}" == "linux/aarch64" ]]; then
        normalized_platform="linux/arm64"
    fi
    output_flag="--load"
    if [[ "{{ push }}" == "true" ]]; then
        output_flag="--push"
    fi
    cache_from_flag=""
    if [[ -n "{{ cache_from }}" ]]; then
        cache_from_flag="--cache-from={{ cache_from }}"
    fi
    cache_to_flag=""
    if [[ -n "{{ cache_to }}" ]]; then
        cache_to_flag="--cache-to={{ cache_to }}"
    fi
    docker buildx build \
        --progress=plain \
        --platform "${normalized_platform}" \
        ${cache_from_flag} \
        ${cache_to_flag} \
        -t "{{ tag }}" \
        "${output_flag}" \
        .

docker-build-local tag=image_tag:
    docker build -t "{{ tag }}" .
