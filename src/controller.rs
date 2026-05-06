use std::{collections::BTreeSet, sync::Arc, time::Duration};

use futures::StreamExt;
use k8s_openapi::api::{apps::v1::StatefulSet, core::v1::Service};
use kube::{
    Api, Client, ResourceExt,
    api::{ListParams, Patch, PatchParams},
    runtime::{Controller, controller::Action, watcher::Config},
};
use serde_json::json;
use thiserror::Error;
use tracing::{error, info, instrument};

use crate::{api::NervixCluster, manifests};

#[derive(Clone)]
struct Context {
    client: Client,
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("missing namespace for NervixCluster {0}")]
    MissingNamespace(String),
    #[error(transparent)]
    Kube(#[from] kube::Error),
}

pub async fn run(client: Client) -> anyhow::Result<()> {
    let clusters = Api::<NervixCluster>::all(client.clone());
    let services = Api::<Service>::all(client.clone());
    let stateful_sets = Api::<StatefulSet>::all(client.clone());
    let context = Arc::new(Context { client });

    Controller::new(clusters, Config::default())
        .owns(services, Config::default())
        .owns(stateful_sets, Config::default())
        .run(reconcile, error_policy, context)
        .for_each(|result| async move {
            match result {
                Ok((object, _action)) => {
                    info!(cluster = %object.name, "reconciled NervixCluster");
                }
                Err(error) => {
                    error!(%error, "reconcile failed");
                }
            }
        })
        .await;

    Ok(())
}

#[instrument(skip(cluster, context), fields(cluster = %cluster.name_any()))]
async fn reconcile(cluster: Arc<NervixCluster>, context: Arc<Context>) -> Result<Action, Error> {
    let namespace = cluster
        .namespace()
        .ok_or_else(|| Error::MissingNamespace(cluster.name_any()))?;
    let client = context.client.clone();
    let services = Api::<Service>::namespaced(client.clone(), &namespace);
    let stateful_sets = Api::<StatefulSet>::namespaced(client.clone(), &namespace);
    let clusters = Api::<NervixCluster>::namespaced(client, &namespace);
    let apply = PatchParams::apply("nervix-k8s-operator").force();

    apply_service(&services, &apply, manifests::headless_service(&cluster)).await?;
    apply_service(&services, &apply, manifests::cluster_service(&cluster)).await?;

    if let Some(access) = cluster
        .spec
        .local_access
        .as_ref()
        .filter(|access| access.enabled)
    {
        let mut desired_local_services = BTreeSet::new();
        desired_local_services.insert(format!("{}-local", cluster.name_any()));

        apply_service(
            &services,
            &apply,
            manifests::bootstrap_local_service(&cluster, access),
        )
        .await?;

        for ordinal in 0..cluster.spec.normalized_replicas() {
            tokio::task::consume_budget().await;
            let service = manifests::node_local_service(&cluster, access, ordinal);
            desired_local_services.insert(service.name_any());
            apply_service(&services, &apply, service).await?;
        }

        prune_local_services(&services, &cluster, &desired_local_services).await?;
    } else {
        prune_local_services(&services, &cluster, &BTreeSet::new()).await?;
    }

    let stateful_set = manifests::stateful_set(&cluster);
    let stateful_set_name = stateful_set.name_any();
    stateful_sets
        .patch(&stateful_set_name, &apply, &Patch::Apply(&stateful_set))
        .await?;

    let ready_replicas = stateful_sets
        .get(&stateful_set_name)
        .await
        .ok()
        .and_then(|set| set.status)
        .and_then(|status| status.ready_replicas)
        .unwrap_or_default();
    let status = manifests::status(&cluster, ready_replicas);

    clusters
        .patch_status(
            &cluster.name_any(),
            &PatchParams::default(),
            &Patch::Merge(json!({ "status": status })),
        )
        .await?;

    Ok(Action::requeue(Duration::from_secs(30)))
}

fn error_policy(_cluster: Arc<NervixCluster>, error: &Error, _context: Arc<Context>) -> Action {
    error!(%error, "scheduling retry after reconcile error");
    Action::requeue(Duration::from_secs(10))
}

async fn apply_service(
    services: &Api<Service>,
    apply: &PatchParams,
    service: Service,
) -> Result<(), Error> {
    services
        .patch(&service.name_any(), apply, &Patch::Apply(&service))
        .await?;
    Ok(())
}

async fn delete_service_if_exists(services: &Api<Service>, name: &str) -> Result<(), Error> {
    match services.delete(name, &Default::default()).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(error)) if error.code == 404 => Ok(()),
        Err(error) => Err(error.into()),
    }
}

async fn prune_local_services(
    services: &Api<Service>,
    cluster: &NervixCluster,
    desired: &BTreeSet<String>,
) -> Result<(), Error> {
    let params = ListParams::default().labels(&format!(
        "app.kubernetes.io/instance={},app.kubernetes.io/component=local-access",
        cluster.name_any()
    ));

    for service in services.list(&params).await? {
        tokio::task::consume_budget().await;
        let name = service.name_any();
        if !desired.contains(&name) {
            delete_service_if_exists(services, &name).await?;
        }
    }

    Ok(())
}
