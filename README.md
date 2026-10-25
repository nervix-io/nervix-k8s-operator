# Nervix Kubernetes Operator

First-pass Rust operator for managing Nervix clusters with kube-rs.

The operator watches namespaced `NervixCluster` resources across the Kubernetes
cluster. Each custom resource owns one Nervix application cluster and reconciles:

- a headless service for stable StatefulSet pod DNS
- a ClusterIP service for in-cluster clients
- optional bootstrap and per-node NodePort services for gRPC and web console local access
- one-time, Secret-backed initialization of the `default` user
- the cluster's interconnect CA and a certificate for every node, in two Secrets
- a StatefulSet with one PVC per Nervix node
- status with desired/ready replica counts and per-node advertised addresses

This mirrors the static manifest `kube/resources/nervix.yaml` in the
[Nervix repository](https://github.com/nervix-io/nervix), but makes the cluster
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

## Node resources

Each Nervix node checks its memory limit and its data volume before it starts,
in its [startup preflight](https://docs.nervix.io/snapshot/capacity-planning.html#what-a-node-needs-to-start),
and refuses to start when they cannot hold its configuration:

- its memory limit must hold the node's memory budgets, about 2.3 GiB with the
  default settings;
- its data volume must be at least as large as each of its storage quotas,
  16 GiB for extracted resources by default.

The defaults meet both checks: `resources.memoryLimit` is `3Gi`, with a
`resources.memoryRequest` of `512Mi`, and `storage` is `20Gi`. With a smaller
memory limit or volume every pod exits, and the refusal names the check and the
values involved. It is the end of the pod's log, and `kubectl describe pod`
shows it as the container's last termination message.

## Interconnect certificates

Nervix nodes authenticate their node-to-node traffic with mutual TLS. For each
`NervixCluster`, the operator generates a CA and keeps it in the Secret
`<name>-interconnect-ca`, which only the operator reads. It issues every node a
certificate signed by that CA into the Secret `<name>-interconnect-tls`, which
the pods mount read-only. Each certificate names its node in the identity URI
`nervix://cluster/<cluster-id>/node/node-<n>` and names the pod's host name in
the headless service, which the node advertises to its peers. The operator's
ClusterRole therefore lets it get, create and patch Secrets.

A node certificate is valid for a year. The operator issues it again 30 days
before it expires, or as soon as it no longer names its node or was signed by
another CA, and nodes reload changed credentials without a restart. The CA is
valid for ten years. Deleting `<name>-interconnect-ca` makes the operator
generate a new CA and issue every node a new certificate. Until every node has
reloaded its credentials, nodes that trust different CAs refuse each other's
connections.
