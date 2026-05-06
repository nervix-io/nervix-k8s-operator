# Nervix Kubernetes Operator Helm Chart

Install the operator:

```sh
helm upgrade --install nervix-k8s-operator ./charts/nervix-k8s-operator \
  --namespace nervix-system \
  --create-namespace
```

Install a specific published image:

```sh
helm upgrade --install nervix-k8s-operator ./charts/nervix-k8s-operator \
  --namespace nervix-system \
  --create-namespace \
  --set image.tag=latest
```

The chart installs the `NervixCluster` CRD from `crds/` and deploys the
cluster-wide operator RBAC by default.
