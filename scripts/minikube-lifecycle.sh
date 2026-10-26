#!/usr/bin/env bash
# Lifecycle checks of the example NervixCluster, `nervix` in the namespace `nervix`, in a minikube
# profile where the operator runs.
#
# They hold the operator to its startup contract. No pod receives a cluster parameter as a startup
# option, so the bootstrap pod and every other pod start again after a restart, a recreation on
# their persistent volumes, a rollout and a scale-up, and the cluster keeps the cluster parameters
# it committed, at the same revision. The operator commits the parameters a new cluster requests
# once, also when its initialization is interrupted, and refuses to change them afterwards.
#
#   minikube-lifecycle.sh initialize-interrupted PROFILE MANIFEST
#       Creates the cluster from MANIFEST and interrupts its initialization once the bootstrap node
#       has created the cluster: it stops the operator, recreates pod 0 on its persistent volume
#       and starts the operator again, which resumes the initialization.
#   minikube-lifecycle.sh restarts PROFILE CLI PASSWORD
#       Checks the initialized cluster with CLI, as the default user with PASSWORD, across restarts
#       of pod 0 and pod 2, the recreation of pod 1 on its persistent volume, a rolling restart, a
#       changed parameter request and a scale-up by one node.

set -euo pipefail

readonly namespace=nervix
readonly cluster=nervix
readonly operator_namespace=nervix-system
readonly operator=deployment/nervix-k8s-operator
readonly operator_pods=app.kubernetes.io/name=nervix-k8s-operator
readonly condition=InitialClusterParametersCommitted
readonly committed_line='^[a-z0-9-]+ committed=[^ ]+ revision=[0-9]+$'

kc() {
    kubectl --context "${profile}" --request-timeout=30s "$@"
}

diagnostics() {
    kc -n "${namespace}" get "nervixcluster/${cluster}" -o yaml || true
    kc -n "${namespace}" get pods,statefulsets,pvc -o wide || true
    kc -n "${namespace}" describe pods || true
    kc -n "${operator_namespace}" logs "${operator}" --tail=200 || true
}

fail() {
    echo "error: $*" >&2
    diagnostics >&2
    exit 1
}

usage() {
    echo "usage: $0 initialize-interrupted PROFILE MANIFEST | restarts PROFILE CLI PASSWORD" >&2
    exit 2
}

# Runs a command every two seconds until it succeeds, and fails the check after SECONDS.
wait_for() {
    local description="$1"
    local seconds="$2"
    shift 2
    local deadline=$((SECONDS + seconds))
    until "$@"; do
        if ((SECONDS >= deadline)); then
            fail "${description}: not reached within ${seconds}s"
        fi
        sleep 2
    done
    echo "ready: ${description}"
}

cluster_field() {
    kc -n "${namespace}" get "nervixcluster/${cluster}" -o jsonpath="{$1}"
}

phase_is() {
    [[ "$(cluster_field .status.initializationPhase)" == "$1" ]]
}

initialized() {
    phase_is initialized \
        && [[ "$(kc -n "${namespace}" get "statefulset/${cluster}" -o jsonpath='{.spec.replicas}')" \
            == "$(cluster_field .spec.replicas)" ]]
}

condition_is() {
    local status="$1"
    local reason="$2"
    [[ "$(cluster_field ".status.conditions[?(@.type==\"${condition}\")].status")" == "${status}" ]] \
        && [[ "$(cluster_field ".status.conditions[?(@.type==\"${condition}\")].reason")" == "${reason}" ]]
}

condition_absent() {
    [[ -z "$(cluster_field ".status.conditions[?(@.type==\"${condition}\")].type")" ]]
}

operator_stopped() {
    [[ -z "$(kc -n "${operator_namespace}" get pods -l "${operator_pods}" -o name)" ]]
}

stateful_set_field() {
    kc -n "${namespace}" get "statefulset/${cluster}" -o jsonpath="{$1}"
}

pod_field() {
    kc -n "${namespace}" get "pod/$1" -o jsonpath="{$2}"
}

pod_ready() {
    [[ "$(pod_field "$1" '.status.conditions[?(@.type=="Ready")].status' 2>/dev/null)" == "True" ]]
}

pod_replaced() {
    local uid
    uid="$(pod_field "$1" .metadata.uid 2>/dev/null)" || return 1
    [[ -n "${uid}" && "${uid}" != "$2" ]]
}

container_restarted() {
    local restarts
    restarts="$(pod_field "$1" '.status.containerStatuses[0].restartCount')" || return 1
    [[ "$(pod_field "$1" .metadata.uid)" == "$2" ]] \
        && ((${restarts:-0} > $3)) \
        && pod_ready "$1"
}

claim_uid() {
    kc -n "${namespace}" get "pvc/data-$1" -o jsonpath='{.metadata.uid}'
}

replicas() {
    cluster_field .spec.replicas
}

# Every pod's UID, restart count and persistent volume claim UID, one pod a line.
pod_identities() {
    local ordinal
    for ((ordinal = 0; ordinal < $(replicas); ordinal++)); do
        local pod="${cluster}-${ordinal}"
        echo "${pod} $(pod_field "${pod}" .metadata.uid) \
$(pod_field "${pod}" '.status.containerStatuses[0].restartCount') $(claim_uid "${pod}")"
    done
}

cli_command() {
    "${cli}" --server "http://${address}:31390" --password "${password}" --command "$1"
}

# What the CLI prints for a statement, or a failure when it reports an error.
answered() {
    local output
    output="$(cli_command "$1" 2>&1)" || return 1
    if grep -Eq '^(error|Error):' <<<"${output}"; then
        return 1
    fi
    printf '%s\n' "${output}"
}

# A leader and one voter for each replica in `SHOW CLUSTER STATUS`.
cluster_ready() {
    local status
    status="$(answered 'SHOW CLUSTER STATUS;')" || return 1
    local raft
    raft="$(sed -n '/^\[raft\]$/,/^\[interconnect\]$/p' <<<"${status}")"
    local leader
    leader="$(sed -n 's/^raft\.current_leader: \([^ ]*\)$/\1/p' <<<"${raft}")"
    local voters
    voters="$(sed -n 's/^- \([^ ]*\) \[voter\] [^ ]*$/\1/p' <<<"${raft}" | sort -u)"
    [[ -n "${leader}" ]] \
        && [[ "$(grep -c . <<<"${voters}")" -eq "$(replicas)" ]] \
        && grep -Fxq "${leader}" <<<"${voters}"
}

# The `<parameter> committed=<value> revision=<revision>` lines of `DESCRIBE CLUSTER`, sorted.
committed_parameters() {
    local description
    description="$(answered 'DESCRIBE CLUSTER;')" || return 1
    { grep -E "${committed_line}" <<<"${description}" || true; } | sort
}

# Whether every node reports every committed parameter as running at its committed value and
# revision.
nodes_run_committed_parameters() {
    local description
    description="$(answered 'DESCRIBE CLUSTER;')" || return 1
    local nodes
    nodes="$(replicas)"
    local parameter committed revision node
    while read -r parameter committed revision; do
        for ((node = 1; node <= nodes; node++)); do
            grep -Fxq "node=node-${node} ${parameter} running=${committed#committed=} ${revision}" \
                <<<"${description}" || return 1
        done
    done < <(grep -E "${committed_line}" <<<"${description}" || true)
}

# Whether the cluster committed every value the NervixCluster requests.
requested_parameters_committed() {
    local replica_count
    replica_count="$(cluster_field .spec.initialClusterParameters.replicaCount)"
    if [[ -z "${replica_count}" ]]; then
        return 0
    fi
    committed_parameters | grep -Eq "^replica-count committed=${replica_count} revision=[0-9]+$"
}

# Waits for the cluster to serve with every node, and checks that it kept the committed
# parameters it reported first, at the same revision.
expect_committed_parameters_kept() {
    local step="$1"
    kc -n "${namespace}" rollout status "statefulset/${cluster}" --timeout=300s
    wait_for "${step}: a leader and $(replicas) voters" 180 cluster_ready
    wait_for "${step}: every node runs the committed parameters" 120 nodes_run_committed_parameters
    local now
    now="$(committed_parameters)" || fail "${step}: DESCRIBE CLUSTER failed"
    if [[ "${now}" != "${committed}" ]]; then
        fail "${step} changed the committed parameters from
${committed}
to
${now}"
    fi
    echo "kept the committed parameters: ${step}"
}

restart_container() {
    local pod="$1"
    local uid
    uid="$(pod_field "${pod}" .metadata.uid)"
    local restarts
    restarts="$(pod_field "${pod}" '.status.containerStatuses[0].restartCount')"
    # The node stops gracefully on SIGTERM and ends the exec session with its container.
    kc -n "${namespace}" exec "${pod}" -c nervix -- /bin/sh -c 'kill -TERM 1' || true
    wait_for "the container of ${pod} restarts in place and is ready" 300 \
        container_restarted "${pod}" "${uid}" "${restarts}"
    expect_committed_parameters_kept "restart the container of ${pod}"
}

recreate_pod() {
    local pod="$1"
    local uid
    uid="$(pod_field "${pod}" .metadata.uid)"
    local claim
    claim="$(claim_uid "${pod}")"
    kc -n "${namespace}" delete "pod/${pod}" --wait=true --timeout=180s
    wait_for "${pod} is recreated" 120 pod_replaced "${pod}" "${uid}"
    wait_for "${pod} is ready" 300 pod_ready "${pod}"
    [[ "$(claim_uid "${pod}")" == "${claim}" ]] \
        || fail "${pod} was recreated without its persistent volume claim"
    expect_committed_parameters_kept "recreate ${pod} on its persistent volume"
}

roll_stateful_set() {
    local before
    before="$(pod_identities)"
    kc -n "${namespace}" rollout restart "statefulset/${cluster}"
    kc -n "${namespace}" rollout status "statefulset/${cluster}" --timeout=600s
    local after
    after="$(pod_identities)"
    local pod uid restarts claim
    while read -r pod uid restarts claim; do
        if grep -Eq "^${pod} ${uid} " <<<"${after}"; then
            fail "the rollout did not recreate ${pod}"
        fi
        grep -Eq "^${pod} [^ ]+ [0-9]+ ${claim}$" <<<"${after}" \
            || fail "the rollout recreated ${pod} without its persistent volume claim"
    done <<<"${before}"
    expect_committed_parameters_kept "roll every pod of the StatefulSet"
}

change_request() {
    local requested
    requested="$(cluster_field .spec.initialClusterParameters.replicaCount)"
    local changed=$((${requested:-0} + 1))
    local generation
    generation="$(stateful_set_field .metadata.generation)"
    local revision
    revision="$(stateful_set_field .status.updateRevision)"
    local before
    before="$(pod_identities)"
    kc -n "${namespace}" patch "nervixcluster/${cluster}" --type=merge \
        -p "{\"spec\":{\"initialClusterParameters\":{\"replicaCount\":${changed}}}}"
    wait_for "the operator refuses replicaCount ${changed} after the initialization" 120 \
        condition_is False ChangedAfterInitialization
    [[ "$(stateful_set_field .metadata.generation)" == "${generation}" ]] \
        || fail "the changed request changed the StatefulSet"
    [[ "$(stateful_set_field .status.updateRevision)" == "${revision}" ]] \
        || fail "the changed request rolled the StatefulSet"
    [[ "$(pod_identities)" == "${before}" ]] || fail "the changed request restarted a pod"
    expect_committed_parameters_kept "request replicaCount ${changed} after the initialization"

    if [[ -z "${requested}" ]]; then
        kc -n "${namespace}" patch "nervixcluster/${cluster}" --type=merge \
            -p '{"spec":{"initialClusterParameters":{"replicaCount":null}}}'
        wait_for "the restored request reports no condition" 120 condition_absent
    else
        kc -n "${namespace}" patch "nervixcluster/${cluster}" --type=merge \
            -p "{\"spec\":{\"initialClusterParameters\":{\"replicaCount\":${requested}}}}"
        wait_for "the restored request is reported committed" 120 condition_is True Committed
    fi
}

stateful_set_scaled_to() {
    [[ "$(stateful_set_field .spec.replicas)" == "$1" ]]
}

scale_up() {
    local before
    before="$(pod_identities)"
    local count=$(($(replicas) + 1))
    kc -n "${namespace}" patch "nervixcluster/${cluster}" --type=merge \
        -p "{\"spec\":{\"replicas\":${count}}}"
    wait_for "the StatefulSet scales to ${count}" 120 stateful_set_scaled_to "${count}"
    wait_for "${cluster}-$((count - 1)) joins" 300 pod_ready "${cluster}-$((count - 1))"
    local pod identity
    while read -r pod identity; do
        grep -Fxq "${pod} ${identity}" <<<"$(pod_identities)" \
            || fail "scaling to ${count} restarted ${pod}"
    done <<<"${before}"
    expect_committed_parameters_kept "scale to ${count} nodes"
}

set_scheduling() {
    local node
    for node in $(kc get nodes -o name); do
        kc "$1" "${node}"
    done
}

initialize_interrupted() {
    # The operator commits the requested parameters and removes the initial credential once pod 0
    # is ready. Pod 0 stays unscheduled until the operator has stopped, however long it takes to.
    set_scheduling cordon
    kc apply -f "${manifest}"
    wait_for "the operator creates statefulset/${cluster}" 120 \
        kc -n "${namespace}" get "statefulset/${cluster}"
    wait_for "the operator starts the initialization" 60 phase_is initializing
    kc -n "${operator_namespace}" scale "${operator}" --replicas=0
    wait_for "the operator stops" 120 operator_stopped
    phase_is initializing || fail "the initialization advanced before the operator stopped"
    set_scheduling uncordon
    wait_for "pod ${cluster}-0 creates the cluster and accepts the initial credential" 300 \
        pod_ready "${cluster}-0"

    local uid
    uid="$(pod_field "${cluster}-0" .metadata.uid)"
    local claim
    claim="$(claim_uid "${cluster}-0")"
    kc -n "${namespace}" delete "pod/${cluster}-0" --wait=true --timeout=180s
    wait_for "pod ${cluster}-0 is recreated" 120 pod_replaced "${cluster}-0" "${uid}"
    wait_for "pod ${cluster}-0 starts again on the cluster it created" 300 \
        pod_ready "${cluster}-0"
    [[ "$(claim_uid "${cluster}-0")" == "${claim}" ]] \
        || fail "pod ${cluster}-0 was recreated without its persistent volume claim"
    phase_is initializing || fail "the initialization advanced while the operator was stopped"

    kc -n "${operator_namespace}" scale "${operator}" --replicas=1
    kc -n "${operator_namespace}" rollout status "${operator}" --timeout=180s
    wait_for "the operator resumes and completes the initialization" 300 initialized
    kc -n "${namespace}" rollout status "statefulset/${cluster}" --timeout=300s
}

restarts() {
    address="$(minikube -p "${profile}" ip)"
    wait_for "a leader and $(replicas) voters" 180 cluster_ready
    wait_for "the cluster commits the requested parameters" 120 requested_parameters_committed
    if [[ -n "$(cluster_field .spec.initialClusterParameters.replicaCount)" ]]; then
        wait_for "the request is reported committed" 120 condition_is True Committed
    fi
    wait_for "every node runs the committed parameters" 120 nodes_run_committed_parameters
    committed="$(committed_parameters)" || fail "DESCRIBE CLUSTER failed"
    echo "committed parameters:"
    printf '%s\n' "${committed}"

    restart_container "${cluster}-0"
    restart_container "${cluster}-2"
    recreate_pod "${cluster}-1"
    roll_stateful_set
    change_request
    scale_up
}

command="${1:-}"
case "${command}" in
    initialize-interrupted)
        (($# == 3)) || usage
        profile="$2"
        manifest="$3"
        initialize_interrupted
        ;;
    restarts)
        (($# == 4)) || usage
        profile="$2"
        cli="$3"
        password="$4"
        restarts
        ;;
    *)
        usage
        ;;
esac
