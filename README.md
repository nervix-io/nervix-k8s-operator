# Nervix Kubernetes Operator

First-pass Rust operator for managing Nervix clusters with kube-rs.

The operator watches namespaced `NervixCluster` resources across the Kubernetes
cluster. Each custom resource owns one Nervix application cluster and reconciles:

- a headless service for stable StatefulSet pod DNS
- a ClusterIP service for in-cluster clients
- optional bootstrap and per-node NodePort services for gRPC and web console local access
- one-time, Secret-backed initialization of the `default` user
- a StatefulSet with one PVC per Nervix node
- status with desired/ready replica counts and per-node advertised addresses

This mirrors the current static manifest shape in
`/home/glebpom/personal/nervix/kube/resources/nervix.yaml`, but makes the cluster
name, image, replica count, storage, resource requests, and NodePorts declarative.

## Local development

```sh
cargo check
cargo run -- crd
cargo run -- run
```

Install the CRD and starter RBAC/deployment:

```sh
kubectl apply -f deploy/crd.yaml
kubectl apply -f deploy/operator.yaml
kubectl apply -f examples/nervix-cluster.yaml
```

Or install the operator with Helm:

```sh
helm upgrade --install nervix-k8s-operator ./charts/nervix-k8s-operator \
  --namespace nervix-system \
  --create-namespace
```

The default operator image is `ghcr.io/nervix-io/nervix-k8s-operator:latest`.
For minikube smoke tests, `just minikube-test` pulls that image and loads it
into the cluster before installing the chart.

With local access enabled, the shared entry service exposes gRPC on NodePort
`31390` and the web console on NodePort `31420`. Per-node NodePorts are used as
the advertised direct endpoints after the server redirects clients to the leader.

## Example

```yaml
apiVersion: nervix.io/v1alpha1
kind: NervixCluster
metadata:
  name: nervix
  namespace: nervix
spec:
  image: ghcr.io/nervix-io/nervix:debian-latest
  replicas: 3
  clusterId: nervix-kube
  initialDefaultUserPasswordSecretRef:
    name: nervix-initial-password
    key: password
  localAccess:
    enabled: true
```

Create `nervix-initial-password` in the same namespace before creating the
cluster. The example manifest includes a development Secret whose password is
`nervix`; replace it for any non-development deployment.

For a new cluster with `initialDefaultUserPasswordSecretRef`, the operator
starts only pod 0 with `NERVIX_INIT_DEFAULT_USER_PASSWORD`. A bootstrap-only
readiness probe authenticates with that password, proving that Nervix committed
the `default` user. The operator then restarts pod 0 without the password,
waits for the clean revision to become ready, and only then scales to
`spec.replicas`. The initialization phase is exposed as
`status.initializationPhase`.

Multiple Nervix clusters are controlled by creating multiple `NervixCluster`
objects, usually in separate namespaces or with distinct names in the same
namespace.
