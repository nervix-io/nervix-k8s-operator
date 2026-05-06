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

minikube-start profile="nervix-operator-test" kubernetes_version="v1.34.0":
    minikube start -p "{{ profile }}" --driver=docker --kubernetes-version="{{ kubernetes_version }}"

minikube-load-operator profile="nervix-operator-test" tag=image_tag:
    minikube -p "{{ profile }}" image load "{{ tag }}"

minikube-deploy profile="nervix-operator-test" tag=image_tag:
    #!/usr/bin/env bash
    set -euo pipefail
    kubectl --context "{{ profile }}" apply -f deploy/crd.yaml
    kubectl --context "{{ profile }}" apply -f deploy/operator.yaml
    kubectl --context "{{ profile }}" -n nervix-system set image deployment/nervix-k8s-operator operator="{{ tag }}"
    kubectl --context "{{ profile }}" -n nervix-system rollout status deployment/nervix-k8s-operator --timeout=180s

minikube-create-cluster profile="nervix-operator-test":
    #!/usr/bin/env bash
    set -euo pipefail
    kubectl --context "{{ profile }}" apply -f examples/nervix-cluster.yaml
    deadline=$((SECONDS + 120))
    until kubectl --context "{{ profile }}" -n nervix get statefulset/nervix >/dev/null 2>&1; do
        if (( SECONDS >= deadline )); then
            echo "statefulset/nervix was not created by the operator within 120s" >&2
            kubectl --context "{{ profile }}" -n nervix get all >&2 || true
            kubectl --context "{{ profile }}" -n nervix-system logs deployment/nervix-k8s-operator --tail=200 >&2 || true
            exit 1
        fi
        sleep 2
    done
    kubectl --context "{{ profile }}" -n nervix rollout status statefulset/nervix --timeout=300s
    kubectl --context "{{ profile }}" -n nervix get nervixcluster nervix -o wide

minikube-cli-check profile="nervix-operator-test" cli_bin="nervix-cli":
    #!/usr/bin/env bash
    set -euo pipefail
    server="http://$(minikube -p "{{ profile }}" ip):31390"
    deadline=$((SECONDS + 120))
    until "{{ cli_bin }}" --server "${server}" --command "SHOW CLUSTER STATUS;"; do
        if (( SECONDS >= deadline )); then
            echo "nervix-cli could not connect to ${server}" >&2
            exit 1
        fi
        sleep 5
    done

minikube-test profile="nervix-operator-test" tag=image_tag cli_bin="nervix-cli":
    just minikube-start "{{ profile }}"
    just docker-build-local "{{ tag }}"
    just minikube-load-operator "{{ profile }}" "{{ tag }}"
    just minikube-deploy "{{ profile }}" "{{ tag }}"
    just minikube-create-cluster "{{ profile }}"
    just minikube-cli-check "{{ profile }}" "{{ cli_bin }}"
