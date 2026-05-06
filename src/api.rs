use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, CustomResource, Deserialize, JsonSchema, PartialEq, Serialize)]
#[kube(
    group = "nervix.io",
    version = "v1alpha1",
    kind = "NervixCluster",
    plural = "nervixclusters",
    namespaced,
    status = "NervixClusterStatus",
    shortname = "nvx",
    derive = "PartialEq",
    printcolumn = r#"{"name":"Replicas","type":"integer","jsonPath":".spec.replicas"}"#,
    printcolumn = r#"{"name":"Ready","type":"integer","jsonPath":".status.readyReplicas"}"#,
    printcolumn = r#"{"name":"Image","type":"string","jsonPath":".spec.image"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct NervixClusterSpec {
    pub image: String,
    #[serde(default = "default_replicas")]
    pub replicas: i32,
    #[serde(default)]
    pub cluster_id: Option<String>,
    #[serde(default = "default_storage")]
    pub storage: String,
    #[serde(default)]
    pub log_filter: Option<String>,
    #[serde(default)]
    pub local_access: Option<LocalAccessSpec>,
    #[serde(default)]
    pub resources: ResourceSpec,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalAccessSpec {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_bootstrap_grpc_node_port")]
    pub bootstrap_grpc_node_port: i32,
    #[serde(default = "default_bootstrap_observability_node_port")]
    pub bootstrap_observability_node_port: i32,
    #[serde(default = "default_first_node_grpc_node_port")]
    pub first_node_grpc_node_port: i32,
    #[serde(default = "default_first_node_observability_node_port")]
    pub first_node_observability_node_port: i32,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceSpec {
    #[serde(default = "default_cpu_request")]
    pub cpu_request: String,
    #[serde(default = "default_memory_request")]
    pub memory_request: String,
    #[serde(default = "default_memory_limit")]
    pub memory_limit: String,
}

impl Default for ResourceSpec {
    fn default() -> Self {
        Self {
            cpu_request: default_cpu_request(),
            memory_request: default_memory_request(),
            memory_limit: default_memory_limit(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NervixClusterStatus {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    #[serde(default)]
    pub ready_replicas: i32,
    #[serde(default)]
    pub replicas: i32,
    #[serde(default)]
    pub nodes: Vec<NervixNodeStatus>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NervixNodeStatus {
    pub name: String,
    pub ordinal: i32,
    pub grpc_advertise_address: String,
    pub cluster_advertise_address: String,
    pub cluster_api_advertise_address: String,
    pub interconnect_advertise_address: String,
}

impl NervixClusterSpec {
    pub fn normalized_replicas(&self) -> i32 {
        self.replicas.max(1)
    }

    pub fn resolved_cluster_id(&self, name: &str) -> String {
        self.cluster_id.clone().unwrap_or_else(|| name.to_string())
    }
}

pub fn default_replicas() -> i32 {
    3
}

fn default_true() -> bool {
    true
}

fn default_storage() -> String {
    "5Gi".to_string()
}

fn default_cpu_request() -> String {
    "250m".to_string()
}

fn default_memory_request() -> String {
    "512Mi".to_string()
}

fn default_memory_limit() -> String {
    "2Gi".to_string()
}

fn default_bootstrap_grpc_node_port() -> i32 {
    31390
}

fn default_bootstrap_observability_node_port() -> i32 {
    31090
}

fn default_first_node_grpc_node_port() -> i32 {
    31391
}

fn default_first_node_observability_node_port() -> i32 {
    31091
}
