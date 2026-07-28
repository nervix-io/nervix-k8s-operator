use std::collections::BTreeMap;

use k8s_openapi::{
    api::{
        apps::v1::{StatefulSet, StatefulSetSpec},
        core::v1::{
            Container, ContainerPort, EnvVar, EnvVarSource, ExecAction, HTTPGetAction,
            ObjectFieldSelector, PersistentVolumeClaim, PersistentVolumeClaimSpec,
            PodSecurityContext, PodSpec, PodTemplateSpec, Probe, ResourceRequirements,
            SecretKeySelector, SecurityContext, Service, ServicePort, ServiceSpec, VolumeMount,
        },
    },
    apimachinery::pkg::{
        api::resource::Quantity, apis::meta::v1::ObjectMeta, util::intstr::IntOrString,
    },
};
use kube::{Resource, ResourceExt};

use crate::api::{
    LocalAccessSpec, NervixCluster, NervixClusterInitializationPhase, NervixClusterStatus,
    NervixNodeStatus, SecretKeyRef,
};

const APP_NAME: &str = "nervix";
const GRPC_PORT: i32 = 47391;
const GOSSIP_PORT: i32 = 47392;
const CLUSTER_API_PORT: i32 = 47393;
const INTERCONNECT_PORT: i32 = 47395;
const HTTP_PORT: i32 = 8080;
const HTTPS_PORT: i32 = 8443;
const OBSERVABILITY_PORT: i32 = 9090;
const WEB_CONSOLE_PORT: i32 = 47420;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum StatefulSetMode {
    Initializing,
    RemovingCredentials,
    Running,
}

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
                service_port("web-console", WEB_CONSOLE_PORT),
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
                service_port("web-console", WEB_CONSOLE_PORT),
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
                    "web-console",
                    WEB_CONSOLE_PORT,
                    access.bootstrap_web_console_node_port,
                ),
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
                    "web-console",
                    WEB_CONSOLE_PORT,
                    access.first_node_web_console_node_port + ordinal,
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

pub fn stateful_set(cluster: &NervixCluster, mode: StatefulSetMode) -> StatefulSet {
    let name = cluster.name_any();
    let spec = &cluster.spec;
    let labels = component_labels(cluster, "application");
    let selector = selector_labels(cluster);
    let replicas = match mode {
        StatefulSetMode::Initializing | StatefulSetMode::RemovingCredentials => 1,
        StatefulSetMode::Running => spec.normalized_replicas(),
    };

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
                    containers: vec![container(cluster, mode)],
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

pub fn status(
    cluster: &NervixCluster,
    ready_replicas: i32,
    initialization_phase: Option<NervixClusterInitializationPhase>,
) -> NervixClusterStatus {
    let namespace = cluster.namespace().unwrap_or_else(|| "default".to_string());
    let replicas = cluster.spec.normalized_replicas();

    NervixClusterStatus {
        observed_generation: cluster.metadata.generation,
        initialization_phase,
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
    let web_console = if let Some(access) = enabled_local_access(cluster) {
        format!(
            "$(HOST_IP):{}",
            access.first_node_web_console_node_port + ordinal
        )
    } else {
        format!("{fqdn}:{WEB_CONSOLE_PORT}")
    };

    NervixNodeStatus {
        name: pod,
        ordinal,
        grpc_advertise_address: grpc,
        web_console_advertise_address: web_console,
        cluster_advertise_address: format!("{fqdn}:{GOSSIP_PORT}"),
        cluster_api_advertise_address: format!("{fqdn}:{CLUSTER_API_PORT}"),
        interconnect_advertise_address: format!("{fqdn}:{INTERCONNECT_PORT}"),
    }
}

fn container(cluster: &NervixCluster, mode: StatefulSetMode) -> Container {
    let spec = &cluster.spec;
    let mut env = vec![
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
    ];
    if mode == StatefulSetMode::Initializing
        && let Some(secret_ref) = &spec.initial_default_user_password_secret_ref
    {
        env.push(secret_env("NERVIX_INIT_DEFAULT_USER_PASSWORD", secret_ref));
    }

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
        env: Some(env),
        ports: Some(vec![
            container_port("grpc", GRPC_PORT, "TCP"),
            container_port("gossip", GOSSIP_PORT, "UDP"),
            container_port("cluster-api", CLUSTER_API_PORT, "TCP"),
            container_port("interconnect", INTERCONNECT_PORT, "TCP"),
            container_port("http", HTTP_PORT, "TCP"),
            container_port("https", HTTPS_PORT, "TCP"),
            container_port("web-console", WEB_CONSOLE_PORT, "TCP"),
            container_port("observability", OBSERVABILITY_PORT, "TCP"),
        ]),
        readiness_probe: Some(match mode {
            StatefulSetMode::Initializing => initial_password_probe(),
            StatefulSetMode::RemovingCredentials | StatefulSetMode::Running => {
                http_probe("/readyz", 5, 6)
            }
        }),
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
    let web_console_advertise = if let Some(access) = enabled_local_access(cluster) {
        format!(
            "\"${{HOST_IP}}:$(({} + ordinal))\"",
            access.first_node_web_console_node_port
        )
    } else {
        "\"${pod_fqdn}:47420\"".to_string()
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

exec /usr/local/bin/nervix-server \
  --addr 0.0.0.0:47391 \
  --grpc-mode http \
  --grpc-advertise-addr {grpc_advertise} \
  --http-listen-addr 0.0.0.0:8080 \
  --https-listen-addr 0.0.0.0:8443 \
  --observability-listen-addr 0.0.0.0:9090 \
  --web-console-listen-addr 0.0.0.0:47420 \
  --web-console-advertise-addr {web_console_advertise} \
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

fn secret_env(name: &str, secret_ref: &SecretKeyRef) -> EnvVar {
    EnvVar {
        name: name.to_string(),
        value_from: Some(EnvVarSource {
            secret_key_ref: Some(SecretKeySelector {
                name: secret_ref.name.clone(),
                key: secret_ref.key.clone(),
                optional: Some(false),
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn initial_password_probe() -> Probe {
    Probe {
        exec: Some(ExecAction {
            command: Some(vec![
                "/bin/sh".to_string(),
                "-ec".to_string(),
                "NERVIX_PASSWORD=\"${NERVIX_INIT_DEFAULT_USER_PASSWORD}\" \
exec /usr/local/bin/nervix-cli \
--server http://127.0.0.1:47391 \
--command 'SHOW CLUSTER STATUS;' >/dev/null"
                    .to_string(),
            ]),
        }),
        period_seconds: Some(5),
        failure_threshold: Some(24),
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
        let stateful_set = stateful_set(&cluster, StatefulSetMode::Running);

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
        assert_service_port(&bootstrap, "grpc", GRPC_PORT, Some(31390));
        assert_service_port(&bootstrap, "web-console", WEB_CONSOLE_PORT, Some(31420));

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
        assert!(script.contains("--web-console-listen-addr 0.0.0.0:47420"));
        assert!(
            script.contains("--web-console-advertise-addr \"${HOST_IP}:$((31421 + ordinal))\"")
        );
    }

    #[test]
    fn initialization_uses_one_replica_and_secret_backed_authenticated_readiness() {
        let stateful_set = stateful_set(&cluster(), StatefulSetMode::Initializing);
        let spec = stateful_set.spec.expect("stateful set spec exists");
        assert_eq!(spec.replicas, Some(1));

        let container = &spec.template.spec.expect("pod spec exists").containers[0];
        let password_env = container
            .env
            .as_ref()
            .and_then(|env| {
                env.iter()
                    .find(|env| env.name == "NERVIX_INIT_DEFAULT_USER_PASSWORD")
            })
            .expect("initial password environment variable exists");
        let secret_ref = password_env
            .value_from
            .as_ref()
            .and_then(|source| source.secret_key_ref.as_ref())
            .expect("initial password comes from a Secret");
        assert_eq!(secret_ref.name, "nervix-initial-password");
        assert_eq!(secret_ref.key, "password");

        let probe_command = container
            .readiness_probe
            .as_ref()
            .and_then(|probe| probe.exec.as_ref())
            .and_then(|action| action.command.as_ref())
            .expect("initialization uses an exec readiness probe")
            .join(" ");
        assert!(probe_command.contains("/usr/local/bin/nervix-cli"));
        assert!(probe_command.contains("SHOW CLUSTER STATUS;"));
        assert!(probe_command.contains("NERVIX_PASSWORD=\"${NERVIX_INIT_DEFAULT_USER_PASSWORD}\""));
    }

    #[test]
    fn credentials_are_removed_from_one_replica_before_normal_scale_up() {
        let cluster = cluster();
        let removing_credentials = stateful_set(&cluster, StatefulSetMode::RemovingCredentials);
        let removing_spec = removing_credentials
            .spec
            .expect("credential-removal StatefulSet spec exists");
        assert_eq!(removing_spec.replicas, Some(1));
        assert_no_initial_password(&removing_spec.template);
        assert!(
            removing_spec.template.spec.unwrap().containers[0]
                .readiness_probe
                .as_ref()
                .is_some_and(|probe| probe.http_get.is_some())
        );

        let running = stateful_set(&cluster, StatefulSetMode::Running);
        let running_spec = running.spec.expect("running StatefulSet spec exists");
        assert_eq!(running_spec.replicas, Some(3));
        assert_no_initial_password(&running_spec.template);
    }

    #[test]
    fn status_includes_per_node_web_console_advertise_address() {
        let status = status(
            &cluster(),
            3,
            Some(NervixClusterInitializationPhase::Initialized),
        );

        assert_eq!(
            status.nodes[0].web_console_advertise_address,
            "$(HOST_IP):31421"
        );
        assert_eq!(
            status.nodes[2].web_console_advertise_address,
            "$(HOST_IP):31423"
        );
        assert_eq!(
            status.initialization_phase,
            Some(NervixClusterInitializationPhase::Initialized)
        );
    }

    fn cluster() -> NervixCluster {
        NervixCluster::new(
            "nervix",
            NervixClusterSpec {
                image: "ghcr.io/nervix-io/nervix:test".to_string(),
                replicas: 3,
                cluster_id: Some("nervix-kube".to_string()),
                initial_default_user_password_secret_ref: Some(SecretKeyRef {
                    name: "nervix-initial-password".to_string(),
                    key: "password".to_string(),
                }),
                storage: "5Gi".to_string(),
                log_filter: None,
                local_access: Some(LocalAccessSpec {
                    enabled: true,
                    bootstrap_grpc_node_port: 31390,
                    bootstrap_web_console_node_port: 31420,
                    bootstrap_observability_node_port: 31090,
                    first_node_grpc_node_port: 31391,
                    first_node_web_console_node_port: 31421,
                    first_node_observability_node_port: 31091,
                }),
                resources: Default::default(),
            },
        )
    }

    fn assert_service_port(service: &Service, name: &str, port: i32, node_port: Option<i32>) {
        let service_port = service
            .spec
            .as_ref()
            .and_then(|spec| spec.ports.as_ref())
            .and_then(|ports| {
                ports
                    .iter()
                    .find(|service_port| service_port.name.as_deref() == Some(name))
            })
            .expect("service port exists");

        assert_eq!(service_port.port, port);
        assert_eq!(service_port.node_port, node_port);
    }

    fn assert_no_initial_password(template: &PodTemplateSpec) {
        let container = &template.spec.as_ref().expect("pod spec exists").containers[0];
        assert!(container.env.as_ref().is_none_or(|env| {
            env.iter()
                .all(|env| env.name != "NERVIX_INIT_DEFAULT_USER_PASSWORD")
        }));
    }
}
