# Nervix Kubernetes Operator Helm Chart

## Install From Git

The chart is distributed in the operator repository rather than as a versioned
chart release. Clone the repository and install its bundled chart:

```sh
git clone --depth 1 https://github.com/nervix-io/nervix-k8s-operator.git

helm upgrade --install nervix-k8s-operator \
  ./nervix-k8s-operator/charts/nervix-k8s-operator \
  --namespace nervix-system \
  --create-namespace
```

The chart currently defaults the operator image tag to `latest`. The smoke-test
`NervixCluster` uses `ghcr.io/nervix-io/nervix:debian-latest`, which is the
current rolling Nervix image tag while the project has no stable image releases.

With local access enabled, the shared entry service exposes gRPC on NodePort
`31390` and the web console on NodePort `31420`. Per-node NodePorts are used as
the advertised direct endpoints after the server redirects clients to the leader.

New clusters can initialize the `default` user from a Secret in the cluster
namespace:

```yaml
spec:
  initialDefaultUserPasswordSecretRef:
    name: nervix-initial-password
    key: password
```

The operator exposes that Secret to a single bootstrap pod, verifies the user by
authentication, restarts that pod without the credential, and only then scales
the StatefulSet to the requested replica count.

The chart installs the `NervixCluster` CRD from `crds/` and deploys the
cluster-wide operator RBAC by default. That RBAC lets the operator get, create
and patch Secrets, which hold each cluster's interconnect CA and its nodes'
interconnect certificates.
