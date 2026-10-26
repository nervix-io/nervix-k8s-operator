//! The cluster parameters a new NervixCluster commits while the operator initializes it.
//!
//! A Nervix cluster commits its cluster parameters once, through Raft, and every node reads them
//! from that record. Startup options can seed them only on the very first start of a new cluster;
//! every later start refuses them, including the bootstrap node's, and `ALTER CLUSTER SET` changes
//! them afterwards. A seed option in the bootstrap pod's template would therefore stop that pod as
//! soon as it restarts after its first start committed the record, and nothing on its volume tells
//! whether that commit happened. So the operator never passes a cluster parameter as a startup
//! option. The bootstrap node commits the server's defaults when it creates the cluster. While the
//! cluster still initializes, the operator reads the committed values with `DESCRIBE CLUSTER` and
//! assigns every requested value that differs with one `ALTER CLUSTER SET`. It runs the image's own
//! `nervix-cli` in the bootstrap pod, authenticated with the initial credential only that pod holds
//! then. The committed values, not a command's reported outcome, decide when the request is
//! committed, so an interrupted or repeated attempt commits each requested value once.
//!
//! Once the cluster is initialized the operator changes no parameter. A request that differs from
//! what it committed is reported, and `ALTER CLUSTER SET` stays the one way to change a running
//! cluster's parameters.

use std::{collections::BTreeMap, fmt, time::Duration};

use k8s_openapi::{
    api::core::v1::Pod,
    apimachinery::pkg::apis::meta::v1::{Condition, Status, Time},
};
use kube::{Api, api::AttachParams};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::{
    api::{ClusterParameters, NervixClusterInitializationPhase},
    manifests,
};

/// The condition reporting whether the requested initial cluster parameters are committed.
pub const CONDITION_TYPE: &str = "InitialClusterParametersCommitted";

/// Lists every committed parameter on a line of its own, as
/// `<parameter> committed=<value> revision=<revision>`.
const DESCRIBE_CLUSTER: &str = "DESCRIBE CLUSTER;";
/// How long one run of `nervix-cli` in the bootstrap pod may take, its connection included.
const CLI_TIMEOUT: Duration = Duration::from_secs(120);
/// How many bytes of each output stream of one run the operator keeps.
const OUTPUT_LIMIT: u64 = 64 * 1024;
/// How many characters of a reason the condition's message repeats.
const REASON_LIMIT: usize = 1024;

/// A cluster parameter the operator can commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Parameter {
    ReplicaCount,
}

impl Parameter {
    /// The keyword `ALTER CLUSTER SET` assigns the parameter by.
    fn keyword(self) -> &'static str {
        match self {
            Parameter::ReplicaCount => "REPLICA_COUNT",
        }
    }

    /// The name `DESCRIBE CLUSTER` reports the parameter's committed value under.
    fn described_name(self) -> &'static str {
        match self {
            Parameter::ReplicaCount => "replica-count",
        }
    }
}

/// One requested value, in the text that `ALTER CLUSTER SET` takes and `DESCRIBE CLUSTER` prints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assignment {
    parameter: Parameter,
    value: String,
}

impl fmt::Display for Assignment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} = {}", self.parameter.keyword(), self.value)
    }
}

impl ClusterParameters {
    /// The requested values, in the order `ALTER CLUSTER SET` assigns them.
    pub fn assignments(&self) -> Vec<Assignment> {
        let mut assignments = Vec::new();
        if let Some(replica_count) = self.replica_count {
            assignments.push(Assignment {
                parameter: Parameter::ReplicaCount,
                value: replica_count.to_string(),
            });
        }
        assignments
    }
}

/// The one statement that assigns every value in `assignments`.
fn alter_statement(assignments: &[Assignment]) -> String {
    let assignments = assignments
        .iter()
        .map(Assignment::to_string)
        .collect::<Vec<_>>();
    format!("ALTER CLUSTER SET {};", assignments.join(", "))
}

/// The assignments as the condition's message names them, or `none`.
fn listed(assignments: &[Assignment]) -> String {
    if assignments.is_empty() {
        return "none".to_string();
    }
    let assignments = assignments
        .iter()
        .map(Assignment::to_string)
        .collect::<Vec<_>>();
    assignments.join(", ")
}

/// The committed values `DESCRIBE CLUSTER` reports, by the parameter names it prints.
#[derive(Debug, Default, PartialEq, Eq)]
struct CommittedParameters(BTreeMap<String, String>);

impl CommittedParameters {
    /// Reads every `<parameter> committed=<value> revision=<revision>` line of a description.
    /// The lines that report one node's running values start with `node=` and have four fields.
    fn parse(description: &str) -> Self {
        let mut values = BTreeMap::new();
        for line in description.lines() {
            let mut fields = line.split_whitespace();
            let (Some(parameter), Some(committed), Some(revision), None) =
                (fields.next(), fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            let Some(value) = committed.strip_prefix("committed=") else {
                continue;
            };
            if !revision.starts_with("revision=") {
                continue;
            }
            values.insert(parameter.to_string(), value.to_string());
        }
        Self(values)
    }

    /// The requested assignments whose value is not the committed one.
    fn uncommitted(&self, requested: &[Assignment]) -> Vec<Assignment> {
        let mut uncommitted = Vec::new();
        for assignment in requested {
            let committed = self.0.get(assignment.parameter.described_name());
            if committed != Some(&assignment.value) {
                uncommitted.push(assignment.clone());
            }
        }
        uncommitted
    }
}

/// What one run of `nervix-cli` printed, and whether it exited successfully.
#[derive(Debug)]
pub struct CliOutput {
    succeeded: bool,
    stdout: String,
    stderr: String,
}

impl CliOutput {
    /// Why the run refused its statement: the lines `nervix-cli` prints for a failed statement, or
    /// the output of a run that exited with an error. `None` when it reported no refusal.
    fn refusal(&self) -> Option<String> {
        let mut refusals = Vec::new();
        for line in self.stdout.lines().chain(self.stderr.lines()) {
            let line = line.trim();
            if line.starts_with("error:") || line.starts_with("Error:") {
                refusals.push(line);
            }
        }
        if !refusals.is_empty() {
            return Some(refusals.join("; "));
        }
        if self.succeeded {
            return None;
        }
        let output = format!("{}\n{}", self.stdout.trim(), self.stderr.trim());
        let output = output.trim();
        if output.is_empty() {
            return Some("nervix-cli exited with an error and printed nothing".to_string());
        }
        Some(format!("nervix-cli exited with an error: {output}"))
    }
}

#[derive(Debug, Error)]
pub enum CliError {
    #[error("could not run nervix-cli in pod {pod}: {source}")]
    Exec {
        pod: String,
        #[source]
        source: Box<kube::Error>,
    },
    #[error("the exec of nervix-cli in pod {pod} attached no {stream}")]
    MissingStream { pod: String, stream: &'static str },
    #[error("could not read the {stream} of nervix-cli in pod {pod}: {source}")]
    Output {
        pod: String,
        stream: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("nervix-cli in pod {pod} did not finish within {timeout:?}")]
    TimedOut { pod: String, timeout: Duration },
}

/// Runs one Plexus statement through `nervix-cli` against the bootstrap node.
pub(crate) trait Cli {
    async fn run(&self, statement: &str) -> Result<CliOutput, CliError>;
}

/// The `nervix-cli` of the bootstrap pod, authenticated as the default user with the initial
/// credential that pod holds while the cluster initializes.
pub struct BootstrapPodCli {
    pods: Api<Pod>,
    pod: String,
}

impl BootstrapPodCli {
    pub fn new(pods: Api<Pod>, pod: String) -> Self {
        Self { pods, pod }
    }

    async fn exec(&self, statement: &str) -> Result<CliOutput, CliError> {
        let parameters = AttachParams::default()
            .container(manifests::CONTAINER_NAME)
            .stdin(false)
            .stdout(true)
            .stderr(true);
        let command = manifests::initial_credential_cli_command(statement);
        let mut process = self
            .pods
            .exec(&self.pod, command, &parameters)
            .await
            .map_err(|source| CliError::Exec {
                pod: self.pod.clone(),
                source: Box::new(source),
            })?;
        let stdout = process.stdout().ok_or_else(|| CliError::MissingStream {
            pod: self.pod.clone(),
            stream: "stdout",
        })?;
        let stderr = process.stderr().ok_or_else(|| CliError::MissingStream {
            pod: self.pod.clone(),
            stream: "stderr",
        })?;
        let status = process
            .take_status()
            .ok_or_else(|| CliError::MissingStream {
                pod: self.pod.clone(),
                stream: "exit status",
            })?;
        // The streams are drained while the process runs, so a full pipe never holds it up.
        let (stdout, stderr, status) =
            tokio::join!(read_bounded(stdout), read_bounded(stderr), status);
        let stdout = stdout.map_err(|source| CliError::Output {
            pod: self.pod.clone(),
            stream: "stdout",
            source,
        })?;
        let stderr = stderr.map_err(|source| CliError::Output {
            pod: self.pod.clone(),
            stream: "stderr",
            source,
        })?;
        Ok(CliOutput {
            succeeded: exited_successfully(status.as_ref()),
            stdout,
            stderr,
        })
    }
}

impl Cli for BootstrapPodCli {
    async fn run(&self, statement: &str) -> Result<CliOutput, CliError> {
        match tokio::time::timeout(CLI_TIMEOUT, self.exec(statement)).await {
            Ok(output) => output,
            Err(_elapsed) => Err(CliError::TimedOut {
                pod: self.pod.clone(),
                timeout: CLI_TIMEOUT,
            }),
        }
    }
}

/// Reads a stream to its end and keeps its first `OUTPUT_LIMIT` bytes.
async fn read_bounded(mut stream: impl AsyncRead + Unpin) -> std::io::Result<String> {
    let mut kept = Vec::new();
    (&mut stream)
        .take(OUTPUT_LIMIT)
        .read_to_end(&mut kept)
        .await?;
    tokio::io::copy(&mut stream, &mut tokio::io::sink()).await?;
    Ok(String::from_utf8_lossy(&kept).into_owned())
}

/// Whether the exec's final status reports a zero exit status.
fn exited_successfully(status: Option<&Status>) -> bool {
    let Some(status) = status else {
        return false;
    };
    status.status.as_deref() == Some("Success")
}

/// Whether the requested initial parameters are committed.
#[derive(Debug, PartialEq, Eq)]
pub enum Commitment {
    /// Every requested value is the committed one.
    Committed,
    /// A requested value is not committed, for the reason given.
    Uncommitted { reason: String },
}

/// Commits every requested value that `DESCRIBE CLUSTER` does not already report as committed,
/// with one `ALTER CLUSTER SET`. The committed values decide the outcome, because a statement
/// whose connection broke may still have been committed.
pub(crate) async fn commit(cli: &impl Cli, requested: &[Assignment]) -> Commitment {
    if requested.is_empty() {
        return Commitment::Committed;
    }
    let committed = match committed_parameters(cli).await {
        Ok(committed) => committed,
        Err(reason) => return Commitment::Uncommitted { reason },
    };
    let uncommitted = committed.uncommitted(requested);
    if uncommitted.is_empty() {
        return Commitment::Committed;
    }

    let statement = alter_statement(&uncommitted);
    let refusal = match cli.run(&statement).await {
        Ok(output) => output.refusal(),
        Err(error) => Some(error.to_string()),
    };
    let committed = match committed_parameters(cli).await {
        Ok(committed) => committed,
        Err(reason) => return Commitment::Uncommitted { reason },
    };
    let uncommitted = committed.uncommitted(requested);
    if uncommitted.is_empty() {
        return Commitment::Committed;
    }
    let reason = match refusal {
        Some(refusal) => format!("{statement} was refused: {refusal}"),
        None => format!(
            "{statement} reported no refusal, but DESCRIBE CLUSTER does not report {} as \
             committed",
            listed(&uncommitted)
        ),
    };
    Commitment::Uncommitted { reason }
}

/// The values `DESCRIBE CLUSTER` reports as committed, or why they could not be read.
async fn committed_parameters(cli: &impl Cli) -> Result<CommittedParameters, String> {
    let output = match cli.run(DESCRIBE_CLUSTER).await {
        Ok(output) => output,
        Err(error) => return Err(format!("could not read the committed parameters: {error}")),
    };
    if let Some(refusal) = output.refusal() {
        return Err(format!(
            "{DESCRIBE_CLUSTER} was refused, so the committed parameters are unknown: {refusal}"
        ));
    }
    Ok(CommittedParameters::parse(&output.stdout))
}

/// What the `InitialClusterParametersCommitted` condition reports.
#[derive(Debug, PartialEq, Eq)]
pub struct ConditionState {
    committed: bool,
    reason: &'static str,
    message: String,
}

/// What the condition reports about `requested`, given the parameters the operator committed while
/// it initialized the cluster and why the latest attempt left them uncommitted. A cluster that
/// requests nothing reports no condition.
pub fn condition_state(
    requested: &ClusterParameters,
    committed: Option<&ClusterParameters>,
    phase: Option<NervixClusterInitializationPhase>,
    uncommitted_reason: Option<&str>,
) -> Option<ConditionState> {
    let assignments = requested.assignments();
    if assignments.is_empty() {
        return None;
    }
    let state = match phase {
        Some(NervixClusterInitializationPhase::Initializing) => match uncommitted_reason {
            Some(reason) => ConditionState {
                committed: false,
                reason: "Uncommitted",
                message: format!(
                    "{} is not committed yet; the operator retries while the cluster \
                     initializes: {}",
                    listed(&assignments),
                    truncated(reason)
                ),
            },
            None => ConditionState {
                committed: false,
                reason: "Pending",
                message: format!(
                    "the operator commits {} once the bootstrap node accepts the initial \
                     credential",
                    listed(&assignments)
                ),
            },
        },
        Some(
            NervixClusterInitializationPhase::RemovingCredentials
            | NervixClusterInitializationPhase::Initialized,
        ) => {
            if committed == Some(requested) {
                ConditionState {
                    committed: true,
                    reason: "Committed",
                    message: format!(
                        "{} committed while the cluster initialized",
                        listed(&assignments)
                    ),
                }
            } else {
                let committed = match committed {
                    Some(committed) => listed(&committed.assignments()),
                    None => "none".to_string(),
                };
                ConditionState {
                    committed: false,
                    reason: "ChangedAfterInitialization",
                    message: format!(
                        "spec.initialClusterParameters requests {} but the operator committed \
                         {committed} while the cluster initialized. It changes no parameter of an \
                         initialized cluster; change them with ALTER CLUSTER SET",
                        listed(&assignments)
                    ),
                }
            }
        }
        None => ConditionState {
            committed: false,
            reason: "InitializationNotRequested",
            message: format!(
                "spec.initialClusterParameters requests {}, which the operator commits only while \
                 it initializes a new cluster, and that needs \
                 spec.initialDefaultUserPasswordSecretRef. Change a running cluster's parameters \
                 with ALTER CLUSTER SET",
                listed(&assignments)
            ),
        },
    };
    Some(state)
}

/// The conditions a status update reports: `state` as a condition whose transition time moves only
/// when its status changes.
pub fn conditions(
    previous: &[Condition],
    state: Option<ConditionState>,
    generation: Option<i64>,
    now: Time,
) -> Vec<Condition> {
    let Some(state) = state else {
        return Vec::new();
    };
    let status = if state.committed { "True" } else { "False" };
    let mut last_transition_time = now;
    for condition in previous {
        if condition.type_ == CONDITION_TYPE && condition.status == status {
            last_transition_time = condition.last_transition_time.clone();
        }
    }
    vec![Condition {
        type_: CONDITION_TYPE.to_string(),
        status: status.to_string(),
        reason: state.reason.to_string(),
        message: state.message,
        observed_generation: generation,
        last_transition_time,
    }]
}

/// At most `REASON_LIMIT` characters of `reason`.
fn truncated(reason: &str) -> String {
    let mut characters = reason.chars();
    let kept = characters.by_ref().take(REASON_LIMIT).collect::<String>();
    if characters.next().is_some() {
        return format!("{kept}…");
    }
    kept
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::VecDeque};

    use k8s_openapi::jiff::Timestamp;

    use super::*;

    const UNSEEDED_DESCRIPTION: &str = "cluster: nervix-kube\nnode-1 [voter] leader";
    const DEFAULT_DESCRIPTION: &str = "cluster: nervix-kube\n\
        node-unavailability-timeout committed=10s revision=1\n\
        replica-count committed=0 revision=1\n\
        node=node-1 replica-count running=0 revision=1";
    const COMMITTED_DESCRIPTION: &str = "cluster: nervix-kube\n\
        node-unavailability-timeout committed=10s revision=2\n\
        replica-count committed=2 revision=2\n\
        node=node-1 replica-count running=2 revision=2";

    /// A `nervix-cli` that answers each statement it runs with the next scripted answer, and
    /// records every statement.
    struct ScriptedCli {
        answers: RefCell<VecDeque<Result<CliOutput, CliError>>>,
        statements: RefCell<Vec<String>>,
    }

    impl ScriptedCli {
        fn new(answers: Vec<Result<CliOutput, CliError>>) -> Self {
            Self {
                answers: RefCell::new(answers.into()),
                statements: RefCell::new(Vec::new()),
            }
        }

        fn statements(&self) -> Vec<String> {
            self.statements.borrow().clone()
        }
    }

    impl Cli for ScriptedCli {
        async fn run(&self, statement: &str) -> Result<CliOutput, CliError> {
            self.statements.borrow_mut().push(statement.to_string());
            self.answers
                .borrow_mut()
                .pop_front()
                .expect("every statement the test runs has a scripted answer")
        }
    }

    fn printed(stdout: &str) -> Result<CliOutput, CliError> {
        Ok(CliOutput {
            succeeded: true,
            stdout: stdout.to_string(),
            stderr: String::new(),
        })
    }

    fn timed_out() -> Result<CliOutput, CliError> {
        Err(CliError::TimedOut {
            pod: "nervix-0".to_string(),
            timeout: CLI_TIMEOUT,
        })
    }

    fn replica_count(count: i32) -> ClusterParameters {
        ClusterParameters {
            replica_count: Some(count),
        }
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a test runtime starts")
            .block_on(future)
    }

    #[test]
    fn nothing_requested_runs_nothing() {
        let cli = ScriptedCli::new(Vec::new());

        let commitment = block_on(commit(&cli, &ClusterParameters::default().assignments()));

        assert_eq!(commitment, Commitment::Committed);
        assert!(cli.statements().is_empty());
    }

    #[test]
    fn requested_values_are_committed_with_one_statement_and_verified() {
        let cli = ScriptedCli::new(vec![
            printed(DEFAULT_DESCRIPTION),
            printed("cluster parameters changed"),
            printed(COMMITTED_DESCRIPTION),
        ]);

        let commitment = block_on(commit(&cli, &replica_count(2).assignments()));

        assert_eq!(commitment, Commitment::Committed);
        assert_eq!(
            cli.statements(),
            vec![
                "DESCRIBE CLUSTER;".to_string(),
                "ALTER CLUSTER SET REPLICA_COUNT = 2;".to_string(),
                "DESCRIBE CLUSTER;".to_string(),
            ]
        );
    }

    #[test]
    fn a_repeated_attempt_after_the_commit_runs_no_second_statement() {
        let cli = ScriptedCli::new(vec![printed(COMMITTED_DESCRIPTION)]);

        let commitment = block_on(commit(&cli, &replica_count(2).assignments()));

        assert_eq!(commitment, Commitment::Committed);
        assert_eq!(cli.statements(), vec!["DESCRIBE CLUSTER;".to_string()]);
    }

    #[test]
    fn a_statement_whose_outcome_was_lost_counts_once_the_description_reports_it() {
        let cli = ScriptedCli::new(vec![
            printed(DEFAULT_DESCRIPTION),
            timed_out(),
            printed(COMMITTED_DESCRIPTION),
        ]);

        let commitment = block_on(commit(&cli, &replica_count(2).assignments()));

        assert_eq!(commitment, Commitment::Committed);
    }

    #[test]
    fn a_refused_statement_leaves_the_parameters_uncommitted_with_the_refusal() {
        let cli = ScriptedCli::new(vec![
            printed(UNSEEDED_DESCRIPTION),
            printed("error: unexpected token ALTER"),
            printed(UNSEEDED_DESCRIPTION),
        ]);

        let commitment = block_on(commit(&cli, &replica_count(2).assignments()));

        assert_eq!(
            commitment,
            Commitment::Uncommitted {
                reason: "ALTER CLUSTER SET REPLICA_COUNT = 2; was refused: error: unexpected \
                         token ALTER"
                    .to_string()
            }
        );
    }

    #[test]
    fn an_unreadable_description_runs_no_statement() {
        let cli = ScriptedCli::new(vec![Ok(CliOutput {
            succeeded: false,
            stdout: String::new(),
            stderr: "Error: authentication failed".to_string(),
        })]);

        let commitment = block_on(commit(&cli, &replica_count(2).assignments()));

        assert_eq!(
            commitment,
            Commitment::Uncommitted {
                reason: "DESCRIBE CLUSTER; was refused, so the committed parameters are \
                         unknown: Error: authentication failed"
                    .to_string()
            }
        );
        assert_eq!(cli.statements(), vec!["DESCRIBE CLUSTER;".to_string()]);
    }

    #[test]
    fn a_failed_exec_reports_its_error() {
        let cli = ScriptedCli::new(vec![timed_out()]);

        let commitment = block_on(commit(&cli, &replica_count(2).assignments()));

        assert_eq!(
            commitment,
            Commitment::Uncommitted {
                reason: "could not read the committed parameters: nervix-cli in pod nervix-0 \
                         did not finish within 120s"
                    .to_string()
            }
        );
    }

    #[test]
    fn the_description_reports_committed_values_and_ignores_running_ones() {
        let committed = CommittedParameters::parse(COMMITTED_DESCRIPTION);

        assert_eq!(
            committed,
            CommittedParameters(BTreeMap::from([
                ("node-unavailability-timeout".to_string(), "10s".to_string()),
                ("replica-count".to_string(), "2".to_string()),
            ]))
        );
        assert!(
            committed
                .uncommitted(&replica_count(2).assignments())
                .is_empty()
        );
        assert_eq!(
            committed.uncommitted(&replica_count(1).assignments()),
            replica_count(1).assignments()
        );
    }

    #[test]
    fn nothing_requested_reports_no_condition() {
        for phase in [
            None,
            Some(NervixClusterInitializationPhase::Initializing),
            Some(NervixClusterInitializationPhase::Initialized),
        ] {
            assert_eq!(
                condition_state(
                    &ClusterParameters::default(),
                    Some(&replica_count(2)),
                    phase,
                    None
                ),
                None
            );
        }
    }

    #[test]
    fn an_initializing_cluster_reports_its_request_pending_or_uncommitted() {
        let pending = condition_state(
            &replica_count(2),
            None,
            Some(NervixClusterInitializationPhase::Initializing),
            None,
        )
        .expect("a request is reported");
        assert!(!pending.committed);
        assert_eq!(pending.reason, "Pending");

        let uncommitted = condition_state(
            &replica_count(2),
            None,
            Some(NervixClusterInitializationPhase::Initializing),
            Some("ALTER CLUSTER SET REPLICA_COUNT = 2; was refused: error: no leader"),
        )
        .expect("a request is reported");
        assert!(!uncommitted.committed);
        assert_eq!(uncommitted.reason, "Uncommitted");
        assert!(uncommitted.message.contains("error: no leader"));
    }

    #[test]
    fn an_initialized_cluster_reports_the_request_it_committed() {
        for phase in [
            NervixClusterInitializationPhase::RemovingCredentials,
            NervixClusterInitializationPhase::Initialized,
        ] {
            let state = condition_state(
                &replica_count(2),
                Some(&replica_count(2)),
                Some(phase),
                None,
            )
            .expect("a request is reported");

            assert_eq!(
                state,
                ConditionState {
                    committed: true,
                    reason: "Committed",
                    message: "REPLICA_COUNT = 2 committed while the cluster initialized"
                        .to_string(),
                }
            );
        }
    }

    #[test]
    fn a_request_changed_after_initialization_is_refused_with_the_committed_values() {
        let state = condition_state(
            &replica_count(1),
            Some(&replica_count(2)),
            Some(NervixClusterInitializationPhase::Initialized),
            None,
        )
        .expect("a request is reported");

        assert!(!state.committed);
        assert_eq!(state.reason, "ChangedAfterInitialization");
        assert!(state.message.contains("requests REPLICA_COUNT = 1"));
        assert!(state.message.contains("committed REPLICA_COUNT = 2"));
        assert!(state.message.contains("ALTER CLUSTER SET"));

        let added_later = condition_state(
            &replica_count(1),
            None,
            Some(NervixClusterInitializationPhase::Initialized),
            None,
        )
        .expect("a request is reported");
        assert_eq!(added_later.reason, "ChangedAfterInitialization");
        assert!(added_later.message.contains("committed none"));
    }

    #[test]
    fn a_request_without_initialization_is_refused() {
        let state =
            condition_state(&replica_count(2), None, None, None).expect("a request is reported");

        assert!(!state.committed);
        assert_eq!(state.reason, "InitializationNotRequested");
        assert!(
            state
                .message
                .contains("initialDefaultUserPasswordSecretRef")
        );
    }

    #[test]
    fn a_condition_keeps_its_transition_time_until_its_status_changes() {
        let earlier = Time(Timestamp::from_second(1_000).expect("a valid timestamp"));
        let now = Time(Timestamp::from_second(2_000).expect("a valid timestamp"));
        let pending = conditions(
            &[],
            condition_state(
                &replica_count(2),
                None,
                Some(NervixClusterInitializationPhase::Initializing),
                None,
            ),
            Some(1),
            earlier.clone(),
        );
        assert_eq!(pending[0].last_transition_time, earlier);

        let uncommitted = conditions(
            &pending,
            condition_state(
                &replica_count(2),
                None,
                Some(NervixClusterInitializationPhase::Initializing),
                Some("refused"),
            ),
            Some(1),
            now.clone(),
        );
        assert_eq!(uncommitted[0].reason, "Uncommitted");
        assert_eq!(uncommitted[0].last_transition_time, earlier);

        let committed = conditions(
            &uncommitted,
            condition_state(
                &replica_count(2),
                Some(&replica_count(2)),
                Some(NervixClusterInitializationPhase::RemovingCredentials),
                None,
            ),
            Some(1),
            now.clone(),
        );
        assert_eq!(committed[0].status, "True");
        assert_eq!(committed[0].last_transition_time, now);
        assert_eq!(committed[0].observed_generation, Some(1));
    }

    #[test]
    fn a_long_reason_is_truncated() {
        let reason = "x".repeat(REASON_LIMIT + 10);

        let kept = truncated(&reason);

        assert_eq!(kept.chars().count(), REASON_LIMIT + 1);
        assert!(kept.ends_with('…'));
    }
}
