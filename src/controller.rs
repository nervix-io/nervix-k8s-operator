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

use crate::{
    api::{NervixCluster, NervixClusterInitializationPhase},
    manifests::{self, StatefulSetMode},
};

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

    let initialization_phase = initialization_phase(&cluster);
    let stateful_set_mode = stateful_set_mode(initialization_phase);
    let stateful_set = manifests::stateful_set(&cluster, stateful_set_mode);
    let stateful_set_name = stateful_set.name_any();
    let desired_stateful_set_replicas = stateful_set
        .spec
        .as_ref()
        .and_then(|spec| spec.replicas)
        .unwrap_or_default();
    stateful_sets
        .patch(&stateful_set_name, &apply, &Patch::Apply(&stateful_set))
        .await?;

    let observed_stateful_set = stateful_sets.get(&stateful_set_name).await?;
    let ready_replicas = observed_stateful_set
        .status
        .as_ref()
        .and_then(|status| status.ready_replicas)
        .unwrap_or_default();

    if stateful_set_rollout_complete(&observed_stateful_set, desired_stateful_set_replicas) {
        match initialization_phase {
            Some(NervixClusterInitializationPhase::Initializing) => {
                patch_status(
                    &clusters,
                    &cluster,
                    ready_replicas,
                    Some(NervixClusterInitializationPhase::RemovingCredentials),
                )
                .await?;
                apply_stateful_set(
                    &stateful_sets,
                    &apply,
                    manifests::stateful_set(&cluster, StatefulSetMode::RemovingCredentials),
                )
                .await?;
                return Ok(Action::requeue(Duration::from_secs(2)));
            }
            Some(NervixClusterInitializationPhase::RemovingCredentials) => {
                patch_status(
                    &clusters,
                    &cluster,
                    ready_replicas,
                    Some(NervixClusterInitializationPhase::Initialized),
                )
                .await?;
                apply_stateful_set(
                    &stateful_sets,
                    &apply,
                    manifests::stateful_set(&cluster, StatefulSetMode::Running),
                )
                .await?;
                return Ok(Action::requeue(Duration::from_secs(2)));
            }
            Some(NervixClusterInitializationPhase::Initialized) | None => {}
        }
    }

    patch_status(&clusters, &cluster, ready_replicas, initialization_phase).await?;

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

async fn apply_stateful_set(
    stateful_sets: &Api<StatefulSet>,
    apply: &PatchParams,
    stateful_set: StatefulSet,
) -> Result<(), Error> {
    stateful_sets
        .patch(
            &stateful_set.name_any(),
            apply,
            &Patch::Apply(&stateful_set),
        )
        .await?;
    Ok(())
}

async fn patch_status(
    clusters: &Api<NervixCluster>,
    cluster: &NervixCluster,
    ready_replicas: i32,
    initialization_phase: Option<NervixClusterInitializationPhase>,
) -> Result<(), Error> {
    let status = manifests::status(cluster, ready_replicas, initialization_phase);
    clusters
        .patch_status(
            &cluster.name_any(),
            &PatchParams::default(),
            &Patch::Merge(json!({ "status": status })),
        )
        .await?;
    Ok(())
}

fn initialization_phase(cluster: &NervixCluster) -> Option<NervixClusterInitializationPhase> {
    match cluster
        .status
        .as_ref()
        .and_then(|status| status.initialization_phase)
    {
        Some(NervixClusterInitializationPhase::RemovingCredentials) => {
            Some(NervixClusterInitializationPhase::RemovingCredentials)
        }
        Some(NervixClusterInitializationPhase::Initialized) => {
            Some(NervixClusterInitializationPhase::Initialized)
        }
        Some(NervixClusterInitializationPhase::Initializing) | None
            if cluster
                .spec
                .initial_default_user_password_secret_ref
                .is_some() =>
        {
            Some(NervixClusterInitializationPhase::Initializing)
        }
        Some(NervixClusterInitializationPhase::Initializing) | None => None,
    }
}

fn stateful_set_mode(
    initialization_phase: Option<NervixClusterInitializationPhase>,
) -> StatefulSetMode {
    match initialization_phase {
        Some(NervixClusterInitializationPhase::Initializing) => StatefulSetMode::Initializing,
        Some(NervixClusterInitializationPhase::RemovingCredentials) => {
            StatefulSetMode::RemovingCredentials
        }
        Some(NervixClusterInitializationPhase::Initialized) | None => StatefulSetMode::Running,
    }
}

fn stateful_set_rollout_complete(stateful_set: &StatefulSet, desired_replicas: i32) -> bool {
    let Some(status) = &stateful_set.status else {
        return false;
    };
    let generation = stateful_set.metadata.generation.unwrap_or_default();

    status.observed_generation.unwrap_or_default() >= generation
        && status.replicas == desired_replicas
        && status.ready_replicas == Some(desired_replicas)
        && status.updated_replicas == Some(desired_replicas)
        && status.current_revision.is_some()
        && status.current_revision == status.update_revision
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

#[cfg(test)]
mod tests {
    use k8s_openapi::{
        api::apps::v1::StatefulSetStatus, apimachinery::pkg::apis::meta::v1::ObjectMeta,
    };

    use crate::api::{NervixClusterSpec, NervixClusterStatus, SecretKeyRef};

    use super::*;

    #[test]
    fn initialization_phase_starts_only_when_a_password_secret_is_configured() {
        let mut cluster = cluster();
        assert_eq!(initialization_phase(&cluster), None);
        assert_eq!(
            stateful_set_mode(initialization_phase(&cluster)),
            StatefulSetMode::Running
        );

        cluster.spec.initial_default_user_password_secret_ref = Some(SecretKeyRef {
            name: "nervix-initial-password".to_string(),
            key: "password".to_string(),
        });
        assert_eq!(
            initialization_phase(&cluster),
            Some(NervixClusterInitializationPhase::Initializing)
        );
        assert_eq!(
            stateful_set_mode(initialization_phase(&cluster)),
            StatefulSetMode::Initializing
        );
    }

    #[test]
    fn credential_removal_continues_if_the_secret_reference_is_removed() {
        let mut cluster = cluster();
        cluster.status = Some(NervixClusterStatus {
            initialization_phase: Some(NervixClusterInitializationPhase::RemovingCredentials),
            ..Default::default()
        });

        assert_eq!(
            initialization_phase(&cluster),
            Some(NervixClusterInitializationPhase::RemovingCredentials)
        );
        assert_eq!(
            stateful_set_mode(initialization_phase(&cluster)),
            StatefulSetMode::RemovingCredentials
        );
    }

    #[test]
    fn rollout_is_complete_only_after_the_new_ready_revision_is_observed() {
        let mut stateful_set = StatefulSet {
            metadata: ObjectMeta {
                generation: Some(4),
                ..Default::default()
            },
            status: Some(StatefulSetStatus {
                observed_generation: Some(4),
                replicas: 1,
                ready_replicas: Some(1),
                updated_replicas: Some(1),
                current_revision: Some("nervix-abc".to_string()),
                update_revision: Some("nervix-abc".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };

        assert!(stateful_set_rollout_complete(&stateful_set, 1));

        stateful_set.status.as_mut().unwrap().update_revision = Some("nervix-def".to_string());
        assert!(!stateful_set_rollout_complete(&stateful_set, 1));

        stateful_set.status.as_mut().unwrap().update_revision = Some("nervix-abc".to_string());
        stateful_set.status.as_mut().unwrap().observed_generation = Some(3);
        assert!(!stateful_set_rollout_complete(&stateful_set, 1));
    }

    fn cluster() -> NervixCluster {
        NervixCluster::new(
            "nervix",
            NervixClusterSpec {
                image: "ghcr.io/nervix-io/nervix:test".to_string(),
                replicas: 3,
                cluster_id: None,
                initial_default_user_password_secret_ref: None,
                storage: "5Gi".to_string(),
                log_filter: None,
                local_access: None,
                resources: Default::default(),
            },
        )
    }
}
