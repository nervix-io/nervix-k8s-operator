# Nervix Kubernetes Operator

First-pass Rust operator for managing Nervix clusters with kube-rs.

The operator watches namespaced `NervixCluster` resources across the Kubernetes
cluster. Each custom resource owns one Nervix application cluster and reconciles:

- a headless service for stable StatefulSet pod DNS
- a ClusterIP service for in-cluster clients
- optional bootstrap and per-node NodePort services for local access
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

The image in `deploy/operator.yaml` is a placeholder until the operator is
published. For local clusters, replace it with an image you build and load into
the cluster.

## Example

```yaml
apiVersion: nervix.io/v1alpha1
kind: NervixCluster
metadata:
  name: nervix
  namespace: nervix
spec:
  image: ghcr.io/nervix-io/nervix:20260505051615-debian
  replicas: 3
  clusterId: nervix-kube
  localAccess:
    enabled: true
```

Multiple Nervix clusters are controlled by creating multiple `NervixCluster`
objects, usually in separate namespaces or with distinct names in the same
namespace.
