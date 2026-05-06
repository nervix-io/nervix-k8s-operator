use std::collections::BTreeMap;

use k8s_openapi::{
    api::{
        apps::v1::{StatefulSet, StatefulSetSpec},
        core::v1::{
            Container, ContainerPort, EnvVar, EnvVarSource, HTTPGetAction, ObjectFieldSelector,
            PersistentVolumeClaim, PersistentVolumeClaimSpec, PodSecurityContext, PodSpec,
            PodTemplateSpec, Probe, ResourceRequirements, SecurityContext, Service, ServicePort,
            ServiceSpec, VolumeMount,
        },
    },
    apimachinery::pkg::{
        api::resource::Quantity, apis::meta::v1::ObjectMeta, util::intstr::IntOrString,
    },
};
use kube::{Resource, ResourceExt};

use crate::api::{LocalAccessSpec, NervixCluster, NervixClusterStatus, NervixNodeStatus};

const APP_NAME: &str = "nervix";
const GRPC_PORT: i32 = 47391;
const GOSSIP_PORT: i32 = 47392;
const CLUSTER_API_PORT: i32 = 47393;
const INTERCONNECT_PORT: i32 = 47395;
const HTTP_PORT: i32 = 8080;
const HTTPS_PORT: i32 = 8443;
const OBSERVABILITY_PORT: i32 = 9090;

pub fn headless_service(cluster: &NervixCluster) -> Service {
    Service {
        metadata: child_metadata(
            cluster,
            &headless_service_name(cluster),
            component_labels(cluster, "application"),
        ),
        spec: Some(ServiceSpec {
            cluster_ip: Some("None".to_string()),
            publish_not_ready_addresses: Some(true),
            selector: Some(selector_labels(cluster)),
            ports: Some(vec![
                service_port("grpc", GRPC_PORT),
                service_port("gossip", GOSSIP_PORT).with_protocol("UDP"),
                service_port("cluster-api", CLUSTER_API_PORT),
                service_port("interconnect", INTERCONNECT_PORT),
                service_port("http", HTTP_PORT),
                service_port("https", HTTPS_PORT),
                service_port("observability", OBSERVABILITY_PORT),
            ]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

pub fn cluster_service(cluster: &NervixCluster) -> Service {
    Service {
        metadata: child_metadata(
            cluster,
            &cluster.name_any(),
            component_labels(cluster, "application"),
        ),
        spec: Some(ServiceSpec {
            type_: Some("ClusterIP".to_string()),
            selector: Some(selector_labels(cluster)),
            ports: Some(vec![
                service_port("grpc", GRPC_PORT),
                service_port("cluster-api", CLUSTER_API_PORT),
                service_port("interconnect", INTERCONNECT_PORT),
                service_port("http", HTTP_PORT),
                service_port("https", HTTPS_PORT),
                service_port("observability", OBSERVABILITY_PORT),
            ]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

pub fn bootstrap_local_service(cluster: &NervixCluster, access: &LocalAccessSpec) -> Service {
    let name = local_service_name(cluster);
    Service {
        metadata: child_metadata(cluster, &name, component_labels(cluster, "local-access")),
        spec: Some(ServiceSpec {
            type_: Some("NodePort".to_string()),
            selector: Some(selector_labels(cluster)),
            ports: Some(vec![
                node_port("grpc", GRPC_PORT, access.bootstrap_grpc_node_port),
                node_port(
                    "observability",
                    OBSERVABILITY_PORT,
                    access.bootstrap_observability_node_port,
                ),
            ]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

pub fn node_local_service(
    cluster: &NervixCluster,
    access: &LocalAccessSpec,
    ordinal: i32,
) -> Service {
    let pod_name = pod_name(cluster, ordinal);
    let mut selector = BTreeMap::new();
    selector.insert(
        "statefulset.kubernetes.io/pod-name".to_string(),
        pod_name.clone(),
    );

    Service {
        metadata: child_metadata(
            cluster,
            &node_local_service_name(cluster, ordinal),
            component_labels(cluster, "local-access"),
        ),
        spec: Some(ServiceSpec {
            type_: Some("NodePort".to_string()),
            selector: Some(selector),
            ports: Some(vec![
                node_port(
                    "grpc",
                    GRPC_PORT,
                    access.first_node_grpc_node_port + ordinal,
                ),
                node_port(
                    "observability",
                    OBSERVABILITY_PORT,
                    access.first_node_observability_node_port + ordinal,
                ),
            ]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

pub fn stateful_set(cluster: &NervixCluster) -> StatefulSet {
    let name = cluster.name_any();
    let spec = &cluster.spec;
    let labels = component_labels(cluster, "application");
    let selector = selector_labels(cluster);
    let replicas = spec.normalized_replicas();

    StatefulSet {
        metadata: child_metadata(cluster, &name, labels.clone()),
        spec: Some(StatefulSetSpec {
            service_name: Some(headless_service_name(cluster)),
            replicas: Some(replicas),
            pod_management_policy: Some("OrderedReady".to_string()),
            selector: k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector {
                match_labels: Some(selector),
                ..Default::default()
            },
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(labels),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    security_context: Some(PodSecurityContext {
                        fs_group: Some(10001),
                        fs_group_change_policy: Some("OnRootMismatch".to_string()),
                        ..Default::default()
                    }),
                    termination_grace_period_seconds: Some(30),
                    containers: vec![container(cluster)],
                    ..Default::default()
                }),
            },
            volume_claim_templates: Some(vec![PersistentVolumeClaim {
                metadata: ObjectMeta {
                    name: Some("data".to_string()),
                    labels: Some(component_labels(cluster, "application")),
                    ..Default::default()
                },
                spec: Some(PersistentVolumeClaimSpec {
                    access_modes: Some(vec!["ReadWriteOnce".to_string()]),
                    resources: Some(k8s_openapi::api::core::v1::VolumeResourceRequirements {
                        requests: Some(quantity_map([("storage", &spec.storage)])),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

pub fn status(cluster: &NervixCluster, ready_replicas: i32) -> NervixClusterStatus {
    let namespace = cluster.namespace().unwrap_or_else(|| "default".to_string());
    let replicas = cluster.spec.normalized_replicas();

    NervixClusterStatus {
        observed_generation: cluster.metadata.generation,
        ready_replicas,
        replicas,
        nodes: (0..replicas)
            .map(|ordinal| node_status(&namespace, cluster, ordinal))
            .collect(),
    }
}

fn node_status(namespace: &str, cluster: &NervixCluster, ordinal: i32) -> NervixNodeStatus {
    let pod = pod_name(cluster, ordinal);
    let fqdn = format!(
        "{pod}.{}.{}.svc.cluster.local",
        headless_service_name(cluster),
        namespace
    );
    let grpc = if let Some(access) = enabled_local_access(cluster) {
        format!("$(HOST_IP):{}", access.first_node_grpc_node_port + ordinal)
    } else {
        format!("{fqdn}:{GRPC_PORT}")
    };

    NervixNodeStatus {
        name: pod,
        ordinal,
        grpc_advertise_address: grpc,
        cluster_advertise_address: format!("{fqdn}:{GOSSIP_PORT}"),
        cluster_api_advertise_address: format!("{fqdn}:{CLUSTER_API_PORT}"),
        interconnect_advertise_address: format!("{fqdn}:{INTERCONNECT_PORT}"),
    }
}

fn container(cluster: &NervixCluster) -> Container {
    let spec = &cluster.spec;
    Container {
        name: APP_NAME.to_string(),
        image: Some(spec.image.clone()),
        image_pull_policy: Some("IfNotPresent".to_string()),
        security_context: Some(SecurityContext {
            allow_privilege_escalation: Some(false),
            run_as_group: Some(10001),
            run_as_non_root: Some(true),
            run_as_user: Some(10001),
            ..Default::default()
        }),
        command: Some(vec![
            "/bin/sh".to_string(),
            "-ec".to_string(),
            startup_script(cluster),
        ]),
        env: Some(vec![
            field_env("POD_NAMESPACE", "metadata.namespace"),
            field_env("HOST_IP", "status.hostIP"),
            EnvVar {
                name: "RUST_LOG".to_string(),
                value: Some(
                    spec.log_filter
                        .clone()
                        .unwrap_or_else(|| "info,nervix=info".to_string()),
                ),
                ..Default::default()
            },
        ]),
        ports: Some(vec![
            container_port("grpc", GRPC_PORT, "TCP"),
            container_port("gossip", GOSSIP_PORT, "UDP"),
            container_port("cluster-api", CLUSTER_API_PORT, "TCP"),
            container_port("interconnect", INTERCONNECT_PORT, "TCP"),
            container_port("http", HTTP_PORT, "TCP"),
            container_port("https", HTTPS_PORT, "TCP"),
            container_port("observability", OBSERVABILITY_PORT, "TCP"),
        ]),
        readiness_probe: Some(http_probe("/readyz", 5, 6)),
        liveness_probe: Some(http_probe("/livez", 10, 3)),
        startup_probe: Some(http_probe("/livez", 5, 24)),
        resources: Some(ResourceRequirements {
            requests: Some(quantity_map([
                ("cpu", &spec.resources.cpu_request),
                ("memory", &spec.resources.memory_request),
            ])),
            limits: Some(quantity_map([("memory", &spec.resources.memory_limit)])),
            ..Default::default()
        }),
        volume_mounts: Some(vec![VolumeMount {
            name: "data".to_string(),
            mount_path: "/var/lib/nervix".to_string(),
            ..Default::default()
        }]),
        ..Default::default()
    }
}

fn startup_script(cluster: &NervixCluster) -> String {
    let name = cluster.name_any();
    let cluster_id = cluster.spec.resolved_cluster_id(&name);
    let replicas = cluster.spec.normalized_replicas();
    let headless = headless_service_name(cluster);
    let grpc_advertise = if let Some(access) = enabled_local_access(cluster) {
        format!(
            "\"${{HOST_IP}}:$(({} + ordinal))\"",
            access.first_node_grpc_node_port
        )
    } else {
        "\"${pod_fqdn}:47391\"".to_string()
    };

    format!(
        r#"ordinal="${{HOSTNAME##*-}}"
node_number=$((ordinal + 1))
node_id="node-${{node_number}}"
pod_fqdn="${{HOSTNAME}}.{headless}.${{POD_NAMESPACE}}.svc.cluster.local"
if [ "${{ordinal}}" = "0" ]; then
  bootstrap_args="--allow-bootstrap"
else
  bootstrap_args="--cluster-bootstrap-host {name}-0.{headless}.${{POD_NAMESPACE}}.svc.cluster.local:47392"
fi

exec /usr/local/bin/nervix \
  --addr 0.0.0.0:47391 \
  --grpc-mode http \
  --grpc-advertise-addr {grpc_advertise} \
  --http-listen-addr 0.0.0.0:8080 \
  --https-listen-addr 0.0.0.0:8443 \
  --observability-listen-addr 0.0.0.0:9090 \
  --cluster-id {cluster_id} \
  --node-id "${{node_id}}" \
  --cluster-listen-addr 0.0.0.0:47392 \
  --cluster-advertise-addr "${{pod_fqdn}}:47392" \
  --cluster-api-mode http \
  --cluster-api-listen-addr 0.0.0.0:47393 \
  --cluster-api-advertise-addr "${{pod_fqdn}}:47393" \
  --interconnect-mode http \
  --interconnect-listen-addr 0.0.0.0:47395 \
  --interconnect-advertise-addr "${{pod_fqdn}}:47395" \
  --replica-count {replicas} \
  --db-path /var/lib/nervix/db \
  ${{bootstrap_args}}
"#
    )
}

fn enabled_local_access(cluster: &NervixCluster) -> Option<&LocalAccessSpec> {
    cluster
        .spec
        .local_access
        .as_ref()
        .filter(|access| access.enabled)
}

fn child_metadata(
    cluster: &NervixCluster,
    name: &str,
    labels: BTreeMap<String, String>,
) -> ObjectMeta {
    ObjectMeta {
        name: Some(name.to_string()),
        namespace: cluster.namespace(),
        labels: Some(labels),
        owner_references: cluster.controller_owner_ref(&()).map(|owner| vec![owner]),
        ..Default::default()
    }
}

fn selector_labels(cluster: &NervixCluster) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::new();
    labels.insert("app.kubernetes.io/name".to_string(), APP_NAME.to_string());
    labels.insert("app.kubernetes.io/instance".to_string(), cluster.name_any());
    labels
}

fn component_labels(cluster: &NervixCluster, component: &str) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::new();
    labels.insert("app.kubernetes.io/name".to_string(), APP_NAME.to_string());
    labels.insert("app.kubernetes.io/instance".to_string(), cluster.name_any());
    labels.insert(
        "app.kubernetes.io/component".to_string(),
        component.to_string(),
    );
    labels.insert(
        "app.kubernetes.io/managed-by".to_string(),
        "nervix-k8s-operator".to_string(),
    );
    labels
}

fn service_port(name: &str, port: i32) -> ServicePort {
    ServicePort {
        name: Some(name.to_string()),
        port,
        target_port: Some(IntOrString::String(name.to_string())),
        protocol: Some("TCP".to_string()),
        ..Default::default()
    }
}

trait ServicePortExt {
    fn with_protocol(self, protocol: &str) -> Self;
}

impl ServicePortExt for ServicePort {
    fn with_protocol(mut self, protocol: &str) -> Self {
        self.protocol = Some(protocol.to_string());
        self
    }
}

fn node_port(name: &str, port: i32, node_port: i32) -> ServicePort {
    ServicePort {
        node_port: Some(node_port),
        ..service_port(name, port)
    }
}

fn container_port(name: &str, port: i32, protocol: &str) -> ContainerPort {
    ContainerPort {
        name: Some(name.to_string()),
        container_port: port,
        protocol: Some(protocol.to_string()),
        ..Default::default()
    }
}

fn field_env(name: &str, field_path: &str) -> EnvVar {
    EnvVar {
        name: name.to_string(),
        value_from: Some(EnvVarSource {
            field_ref: Some(ObjectFieldSelector {
                api_version: Some("v1".to_string()),
                field_path: field_path.to_string(),
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn http_probe(path: &str, period_seconds: i32, failure_threshold: i32) -> Probe {
    Probe {
        http_get: Some(HTTPGetAction {
            path: Some(path.to_string()),
            port: IntOrString::String("observability".to_string()),
            ..Default::default()
        }),
        period_seconds: Some(period_seconds),
        failure_threshold: Some(failure_threshold),
        ..Default::default()
    }
}

fn quantity_map<const N: usize>(items: [(&str, &String); N]) -> BTreeMap<String, Quantity> {
    items
        .into_iter()
        .map(|(key, value)| (key.to_string(), Quantity(value.clone())))
        .collect()
}

fn headless_service_name(cluster: &NervixCluster) -> String {
    format!("{}-headless", cluster.name_any())
}

fn local_service_name(cluster: &NervixCluster) -> String {
    format!("{}-local", cluster.name_any())
}

fn node_local_service_name(cluster: &NervixCluster, ordinal: i32) -> String {
    format!("{}-local-node-{}", cluster.name_any(), ordinal + 1)
}

fn pod_name(cluster: &NervixCluster, ordinal: i32) -> String {
    format!("{}-{ordinal}", cluster.name_any())
}

#[cfg(test)]
mod tests {
    use kube::ResourceExt;

    use crate::api::{LocalAccessSpec, NervixClusterSpec};

    use super::*;

    #[test]
    fn stateful_set_pod_labels_satisfy_service_selector() {
        let cluster = cluster();
        let service = cluster_service(&cluster);
        let stateful_set = stateful_set(&cluster);

        let selector = service.spec.unwrap().selector.unwrap();
        let pod_labels = stateful_set
            .spec
            .unwrap()
            .template
            .metadata
            .unwrap()
            .labels
            .unwrap();

        for (key, value) in selector {
            assert_eq!(pod_labels.get(&key), Some(&value));
        }
    }

    #[test]
    fn local_access_creates_one_bootstrap_and_one_service_per_node() {
        let cluster = cluster();
        let access = cluster.spec.local_access.as_ref().unwrap();

        let bootstrap = bootstrap_local_service(&cluster, access);
        assert_eq!(bootstrap.name_any(), "nervix-local");

        let node_services = (0..cluster.spec.normalized_replicas())
            .map(|ordinal| node_local_service(&cluster, access, ordinal).name_any())
            .collect::<Vec<_>>();

        assert_eq!(
            node_services,
            vec![
                "nervix-local-node-1".to_string(),
                "nervix-local-node-2".to_string(),
                "nervix-local-node-3".to_string()
            ]
        );
    }

    #[test]
    fn startup_script_bootstraps_only_first_node() {
        let script = startup_script(&cluster());

        assert!(script.contains("--allow-bootstrap"));
        assert!(script.contains("--cluster-bootstrap-host nervix-0.nervix-headless.${POD_NAMESPACE}.svc.cluster.local:47392"));
        assert!(script.contains("--replica-count 3"));
    }

    fn cluster() -> NervixCluster {
        NervixCluster::new(
            "nervix",
            NervixClusterSpec {
                image: "ghcr.io/nervix-io/nervix:test".to_string(),
                replicas: 3,
                cluster_id: Some("nervix-kube".to_string()),
                storage: "5Gi".to_string(),
                log_filter: None,
                local_access: Some(LocalAccessSpec {
                    enabled: true,
                    bootstrap_grpc_node_port: 31390,
                    bootstrap_observability_node_port: 31090,
                    first_node_grpc_node_port: 31391,
                    first_node_observability_node_port: 31091,
                }),
                resources: Default::default(),
            },
        )
    }
}
