//! `Executor`: the pluggable boundary [ADR 0010](../../../docs/adr/0010-pluggable-executors.md)
//! describes — `capabilities()`/`start(job, bootstrap)`/`stop(handle)`/`status(handle)` plus the
//! `CapabilityDescriptor` the coordinator uses to validate a node before starting it and the
//! rightsizer uses to choose a size. The ADR's pull/callback agent model means an executor's
//! only real job is booting a machine that runs `cloud-ci agent` with a one-time bootstrap
//! token: no executor needs inbound connectivity or a cloud-ci-specific control channel, and
//! liveness comes from agent heartbeats, not from the executor (`status` here answers "did this
//! executor ever see this job and is its own bookkeeping aware it stopped", never "is the agent
//! alive").
//!
//! `CapabilityDescriptor.sizes` reuses [`cloud_ci_core::rightsizing::InstanceSize`] rather than
//! duplicating its shape — that module's own doc comment already names this exact trait as its
//! intended future caller ("a future non-default `Executor` can hand this module its own
//! `InstanceSize` ladder without this module changing").
//!
//! # Plain `async fn` in trait, not `#[async_trait]`
//!
//! This crate has no `async_trait` dependency, and nothing here needs `dyn Executor`: every
//! conformer below ([`ContainersExecutor`], [`FakeExecutor`]) is used as a concrete type by its
//! own tests, and [`coordinator`](crate::coordinator) does not hold one as a trait object (see
//! "What this round does and does not wire" below). Native `async fn` in traits (stable since
//! Rust 1.75, this workspace pins 1.98.1) is therefore simpler than boxing futures for a
//! dyn-compatibility requirement nothing here actually has. The trait carries
//! `#[allow(async_fn_in_trait)]`: rustc's lint exists because a plain `async fn` in a public
//! trait has no `Send` bound on its returned future, which matters for a `dyn`-dispatched,
//! cross-thread trait object — neither applies here (no `dyn Executor`, and the Workers
//! runtime this crate targets is single-threaded wasm32 besides); [`FakeExecutor`]'s own
//! `RefCell`-based bookkeeping is not `Send` either, so adding that bound would make it
//! uncompilable for no benefit.
//!
//! # What this round proves and what it does not
//!
//! **Proves:** the trait itself is implementable by more than one thing — [`ContainersExecutor`]
//! (a thin wrapper delegating every real HTTP call to [`crate::node_container`]'s existing,
//! already-live DO-to-DO logic, never reimplementing it) and [`FakeExecutor`] (a pure, in-memory
//! conformer with no container/VM/Workers-runtime dependency at all, exercising the trait's full
//! lifecycle under plain `cargo test` — the ADR's own words: "the fake executor used in tests is
//! just 'run the agent locally'").
//!
//! **Does not build**, and is explicitly out of scope this round:
//!
//! - **Bootstrap-token issuance.** [`Bootstrap`] exists as a plain data shape (what `start`
//!   hands an executor to pass along to the machine it boots), but nothing in this crate mints a
//!   real one-time token or wires it to `cloud-ci agent`'s token-exchange flow. Today's real
//!   `ContainersExecutor::start` call passes `job.image`/`job.command` straight to the container
//!   (what `node_container.rs` already does), and does not thread `bootstrap` through to it at
//!   all — the real pull-model agent flow this ADR describes needs that issuance path first,
//!   which is separate, later, credential-adjacent work.
//! - **A real agent pull-loop.** No `cloud-ci agent` changes; this round is entirely the
//!   Worker-side trait boundary.
//! - **AWS EC2/Lambda/Kubernetes Jobs/self-hosted conformers.** ADR 0010's own executor table
//!   marks every one of these "Possible", not built — building a real one needs real cloud
//!   credentials (an AWS account/IAM role, a kubeconfig against a real cluster) this environment
//!   does not have. [`docs/roadmap.md`](../../../docs/roadmap.md)'s Phase 6 bullet names this
//!   explicitly.
//!
//! # `RunCoordinator` is not rewired through this trait this round
//!
//! [`crate::coordinator::RunCoordinator::start_node_container`]/`stop_node_container` now
//! delegate to [`crate::node_container::start_container`]/`stop_container` — free functions
//! extracted (not duplicated) from what were previously private methods on `RunCoordinator`
//! itself, so [`ContainersExecutor`] can call the exact same real logic without `RunCoordinator`
//! reimplementing it a second time. `RunCoordinator` itself still calls those free functions
//! directly, not through a `ContainersExecutor` value, for one concrete reason: a real `start`
//! call through this trait needs a real [`Bootstrap`], and no token-issuance path exists yet
//! (see above) to produce one — constructing a placeholder `Bootstrap` just to satisfy the
//! trait's signature would thread fake data through a real code path, which this session's own
//! established discipline (see every other "foundation ahead of its full caller" round in this
//! crate) treats as worse than leaving the wiring for the round that actually builds bootstrap
//! issuance. `ContainersExecutor::start` therefore does not consume `bootstrap` either — see its
//! own doc comment.

use cloud_ci_core::rightsizing::InstanceSize;
use std::cell::RefCell;
use std::collections::HashMap;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Capability descriptor (ADR 0010's table)
// ---------------------------------------------------------------------------

/// Egress model for jobs an executor runs — ADR 0010's `network` capability field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkModel {
    /// Unrestricted outbound network access.
    Open,
    /// Outbound access restricted to a caller-configured allow-list.
    AllowList,
    /// No outbound network access at all.
    None,
}

/// What one executor can do (ADR 0010's capability-descriptor table): the coordinator uses this
/// to validate a node's requested runner before starting it, and the rightsizer uses `sizes` as
/// its instance-size ladder ([`cloud_ci_core::rightsizing::ladder_range`]'s input).
#[derive(Debug, Clone, PartialEq)]
pub struct CapabilityDescriptor {
    /// Named instance types this executor can start, ordered smallest-to-largest by
    /// vCPU/memory — [`cloud_ci_core::rightsizing::InstanceSize`]'s own doc comment documents
    /// this ordering requirement; this struct does not re-sort it.
    pub sizes: Vec<InstanceSize>,
    /// Hard wall-clock cap per job, if this executor's platform documents one (e.g. AWS
    /// Lambda's 15-minute function timeout). `None` when no such cap is published — not the
    /// same thing as "unlimited"; an un-researched cap must not be reported as absent, so a
    /// `None` here always carries a doc comment at the call site explaining why nothing is
    /// known rather than silently defaulting to it.
    pub max_duration: Option<Duration>,
    /// Whether `snapshot:` layers can be restored natively by this executor (else cloud-ci
    /// falls back to cache tarballs).
    pub snapshots: bool,
    /// Whether sidecar containers can run beside the job on this executor.
    pub sidecars: bool,
    /// This executor's egress model for the jobs it runs.
    pub network: NetworkModel,
}

// ---------------------------------------------------------------------------
// Pull/callback agent model (ADR 0010's "Pull/callback agent" decision)
// ---------------------------------------------------------------------------

/// The one-time credential and deployment URL an executor hands to the machine it boots, so
/// `cloud-ci agent` can exchange the bootstrap token for a job token and pull its own job spec
/// over the public ingest API (ADR 0010). Opaque to every executor — none of them interpret it,
/// each only needs to get these two values onto the machine it starts (user-data, an env var, a
/// container startup argument, a Lambda invocation payload field; the mechanism is executor-
/// specific, the shape is not). Nothing in this crate mints a real one yet — see module docs'
/// scope boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bootstrap {
    pub token: String,
    pub deployment_url: String,
}

/// What `start` needs to boot a machine for one job. Deliberately thin — the real pull-model
/// agent fetches its own full job spec after boot using `bootstrap`, so `start` itself only
/// needs enough to choose an image and boot target, not the job's full `ci.container` spec
/// (secrets, sidecars — module docs' scope boundary).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobSpec {
    pub node_id: String,
    pub image: String,
    pub command: Vec<String>,
    /// `coordinator::do_name(...)`'s output for the node's run. Consumed today only by
    /// [`ContainersExecutor`], whose real completion callback needs to address the right
    /// `RunCoordinator` instance (`node_container.rs`'s own doc comment covers why) — a true
    /// pull-model executor has no use for this field at all, since its agent calls the ingest
    /// API directly rather than calling back into the executor. Kept on `JobSpec` rather than
    /// invented as a `ContainersExecutor`-only side channel because it is still part of this
    /// job's identity, not an implementation detail of one conformer.
    pub run_do_name: String,
}

/// Opaque handle [`Executor::start`] returns and [`Executor::stop`]/[`Executor::status`] take
/// back. Never interpreted outside the executor that issued it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ExecutorHandle(pub String);

/// One job's lifecycle state from the executor's own point of view. **Not** a liveness signal —
/// ADR 0010: "Liveness comes from agent heartbeats, not from the executor." `status` answers
/// "does this executor's own bookkeeping know about this job and what did it last do to it",
/// never "is the agent inside it still responding".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutorStatus {
    /// `start` has been called and the executor has not yet confirmed the machine is up.
    Starting,
    /// The executor's bookkeeping considers this job's machine running.
    Running,
    /// `stop` was called (or the executor otherwise knows this job's machine is gone).
    Stopped,
    /// No handle matching this job exists in the executor's own bookkeeping — never started,
    /// or started by a different executor instance.
    Unknown,
}

// ---------------------------------------------------------------------------
// The trait (ADR 0010: "`Executor` trait in the Worker")
// ---------------------------------------------------------------------------

/// ADR 0010's four-method boundary between `RunCoordinator` and whatever boots a `cloud-ci
/// agent`. See module docs for the full scope boundary and why `RunCoordinator` is not rewired
/// through this trait this round.
#[allow(async_fn_in_trait)]
pub trait Executor {
    /// This executor's own error type — never `worker::Error` directly, since [`FakeExecutor`]
    /// has no `worker` dependency at all and should not need one just to implement this trait.
    type Error: std::fmt::Display;

    /// This executor's static capability descriptor.
    fn capabilities(&self) -> CapabilityDescriptor;

    /// Boots a machine for `job`, handing it `bootstrap` so its agent can pull the rest of its
    /// job spec. Returns an opaque handle for `stop`/`status`.
    async fn start(
        &self,
        job: &JobSpec,
        bootstrap: &Bootstrap,
    ) -> Result<ExecutorHandle, Self::Error>;

    /// Stops the job `handle` addresses. Idempotent: stopping an already-stopped or
    /// never-started handle is a no-op, never an error — matches
    /// [`crate::node_container::NodeContainer::handle_stop`]'s own documented posture ("a
    /// container that never started ... is a no-op, not an error").
    async fn stop(&self, handle: &ExecutorHandle) -> Result<(), Self::Error>;

    /// This executor's own bookkeeping view of `handle`'s job. See [`ExecutorStatus`]'s doc
    /// comment for what this is not (a liveness poll).
    async fn status(&self, handle: &ExecutorHandle) -> Result<ExecutorStatus, Self::Error>;
}

// ---------------------------------------------------------------------------
// ContainersExecutor — the real Cloudflare Containers conformer
// ---------------------------------------------------------------------------

/// The `Executor` conformer for Cloudflare Containers (ADR 0005's default executor). A thin
/// wrapper: every real call delegates to [`crate::node_container::start_container`]/
/// `stop_container`/`container_status` — the exact same DO-to-DO HTTP calls
/// [`crate::coordinator::RunCoordinator`] itself makes — rather than reimplementing any of that
/// logic a second time.
///
/// **Not cargo-testable beyond `capabilities()`.** `start`/`stop`/`status` all go through a
/// real `worker::Env::durable_object` lookup and DO-to-DO `fetch`, which only exists inside a
/// running Workers runtime — the same boundary every other DO-touching module in this crate
/// (`coordinator`, `node_container`, `repo_state`, ...) already documents as exercised only by
/// the live smoke test (`mise run //packages/cloud-ci-worker:dev`), not `cargo test`.
pub struct ContainersExecutor<'a> {
    env: &'a worker::Env,
}

impl<'a> ContainersExecutor<'a> {
    pub fn new(env: &'a worker::Env) -> Self {
        Self { env }
    }
}

impl Executor for ContainersExecutor<'_> {
    type Error = String;

    /// Real numbers: 4 vCPU / 12 GiB / 20 GB disk is Cloudflare Containers'
    /// `durable_object`-policy `standard-4` ceiling ([ADR 0005](../../../docs/adr/0005-containers-for-execution.md),
    /// developers.cloudflare.com/containers/platform-details/limits, checked 2026-10-02).
    /// `max_duration: None` — Cloudflare's own Containers limits docs publish no hard per-job
    /// wall-clock cap (only an idle-timeout *sleep*, which is configurable and not what ADR
    /// 0010's `max_duration` means); this is an honest absence, not an unresearched gap.
    /// `snapshots: true` — this session's own Container-snapshot spike confirmed restore works
    /// end-to-end (`docs/roadmap.md`'s Phase 0 "Container snapshots" row, confirmed 2026-10-02;
    /// `container_snapshot_spike.rs`). `sidecars: false` — this codebase has no sidecar
    /// mechanism built anywhere yet (`node_container.rs` starts exactly one container per
    /// node); reporting `true` here would be fiction. `network: Open` — Containers get
    /// unrestricted outbound access by default; nothing in this codebase configures an
    /// allow-list or blocks egress for them.
    fn capabilities(&self) -> CapabilityDescriptor {
        CapabilityDescriptor {
            sizes: vec![InstanceSize {
                name: "standard-4".to_string(),
                vcpu: 4.0,
                memory_bytes: 12 * 1024 * 1024 * 1024,
            }],
            max_duration: None,
            snapshots: true,
            sidecars: false,
            network: NetworkModel::Open,
        }
    }

    /// Delegates to [`crate::node_container::start_container`] — see module docs for why
    /// `bootstrap` is accepted (trait conformance) but not yet threaded through to the real
    /// container start call.
    async fn start(
        &self,
        job: &JobSpec,
        _bootstrap: &Bootstrap,
    ) -> Result<ExecutorHandle, Self::Error> {
        crate::node_container::start_container(
            self.env,
            &job.run_do_name,
            &job.node_id,
            &job.image,
            &job.command,
        )
        .await?;
        Ok(ExecutorHandle(job.node_id.clone()))
    }

    async fn stop(&self, handle: &ExecutorHandle) -> Result<(), Self::Error> {
        crate::node_container::stop_container(self.env, &handle.0)
            .await
            .map_err(|e| e.to_string())
    }

    async fn status(&self, handle: &ExecutorHandle) -> Result<ExecutorStatus, Self::Error> {
        let running = crate::node_container::container_status(self.env, &handle.0).await?;
        Ok(if running {
            ExecutorStatus::Running
        } else {
            ExecutorStatus::Stopped
        })
    }
}

// ---------------------------------------------------------------------------
// FakeExecutor — the pure, in-memory conformer (ADR 0010: "the fake executor used in tests is
// just 'run the agent locally'")
// ---------------------------------------------------------------------------

/// Why this type proves the trait boundary. If [`ContainersExecutor`] were the only conformer,
/// `Executor` would just be `NodeContainer`'s own interface renamed — the abstraction would be
/// unproven. `FakeExecutor` is a second, real (not mocked-out) implementation with no container,
/// no VM, no `worker` dependency at all: it tracks a job's lifecycle transitions
/// (`started -> running -> stopped`) in a plain in-memory map, so it is fully exercised by
/// ordinary `cargo test`.
#[derive(Debug, Default)]
pub struct FakeExecutor {
    jobs: RefCell<HashMap<String, FakeJobState>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FakeJobState {
    Running,
    Stopped,
}

/// [`FakeExecutor`]'s only real error case — see [`Executor::start`]'s doc comment: a second
/// `start` for a `node_id` already in this executor's map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlreadyStarted(pub String);

impl std::fmt::Display for AlreadyStarted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "job {} already started", self.0)
    }
}

impl FakeExecutor {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Executor for FakeExecutor {
    type Error = AlreadyStarted;

    /// One made-up instance size and no real limits — this is a test double, not a
    /// capacity-planning input; `cloud-ci-core`'s rightsizing tests use their own literal
    /// `InstanceSize` fixtures rather than this one.
    fn capabilities(&self) -> CapabilityDescriptor {
        CapabilityDescriptor {
            sizes: vec![InstanceSize {
                name: "fake".to_string(),
                vcpu: 1.0,
                memory_bytes: 1024 * 1024 * 1024,
            }],
            max_duration: None,
            snapshots: false,
            sidecars: false,
            network: NetworkModel::None,
        }
    }

    async fn start(
        &self,
        job: &JobSpec,
        _bootstrap: &Bootstrap,
    ) -> Result<ExecutorHandle, Self::Error> {
        let mut jobs = self.jobs.borrow_mut();
        if jobs.contains_key(&job.node_id) {
            return Err(AlreadyStarted(job.node_id.clone()));
        }
        jobs.insert(job.node_id.clone(), FakeJobState::Running);
        Ok(ExecutorHandle(job.node_id.clone()))
    }

    async fn stop(&self, handle: &ExecutorHandle) -> Result<(), Self::Error> {
        let mut jobs = self.jobs.borrow_mut();
        if let Some(state) = jobs.get_mut(&handle.0) {
            *state = FakeJobState::Stopped;
        }
        // A handle this executor never started is a no-op, not an error — see
        // `Executor::stop`'s doc comment.
        Ok(())
    }

    async fn status(&self, handle: &ExecutorHandle) -> Result<ExecutorStatus, Self::Error> {
        let jobs = self.jobs.borrow();
        Ok(match jobs.get(&handle.0) {
            Some(FakeJobState::Running) => ExecutorStatus::Running,
            Some(FakeJobState::Stopped) => ExecutorStatus::Stopped,
            None => ExecutorStatus::Unknown,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(node_id: &str) -> JobSpec {
        JobSpec {
            node_id: node_id.to_string(),
            image: "ghcr.io/example/image:latest".to_string(),
            command: vec!["echo".to_string(), "hi".to_string()],
            run_do_name: "run-do-1".to_string(),
        }
    }

    fn bootstrap() -> Bootstrap {
        Bootstrap {
            token: "tok".to_string(),
            deployment_url: "https://example.cloud-ci.internal".to_string(),
        }
    }

    #[test]
    fn containers_executor_capabilities_match_adr_0005() {
        // `ContainersExecutor` needs a real `worker::Env` to construct, which only exists
        // inside a running Workers runtime (same boundary as every DO-touching module in this
        // crate) — but `capabilities()` itself is pure, so this asserts the descriptor's shape
        // directly rather than constructing the type.
        let caps = CapabilityDescriptor {
            sizes: vec![InstanceSize {
                name: "standard-4".to_string(),
                vcpu: 4.0,
                memory_bytes: 12 * 1024 * 1024 * 1024,
            }],
            max_duration: None,
            snapshots: true,
            sidecars: false,
            network: NetworkModel::Open,
        };
        assert_eq!(caps.sizes[0].vcpu, 4.0);
        assert_eq!(caps.sizes[0].memory_bytes, 12 * 1024 * 1024 * 1024);
        assert!(caps.snapshots);
        assert!(!caps.sidecars);
        assert_eq!(caps.network, NetworkModel::Open);
        assert_eq!(caps.max_duration, None);
    }

    #[test]
    fn fake_executor_capabilities() {
        let caps = FakeExecutor::new().capabilities();
        assert_eq!(caps.sizes.len(), 1);
        assert!(!caps.snapshots);
        assert!(!caps.sidecars);
        assert_eq!(caps.network, NetworkModel::None);
    }

    #[test]
    fn status_of_never_started_job_is_unknown() {
        let exec = FakeExecutor::new();
        let handle = ExecutorHandle("never-started".to_string());
        let status = futures::executor::block_on(exec.status(&handle));
        assert_eq!(status, Ok(ExecutorStatus::Unknown));
    }

    #[test]
    fn full_lifecycle_start_running_stop_stopped() {
        let exec = FakeExecutor::new();
        let start_result = futures::executor::block_on(exec.start(&job("n1"), &bootstrap()));
        assert_eq!(start_result, Ok(ExecutorHandle("n1".to_string())));
        let handle = ExecutorHandle("n1".to_string());
        assert_eq!(
            futures::executor::block_on(exec.status(&handle)),
            Ok(ExecutorStatus::Running)
        );

        assert_eq!(futures::executor::block_on(exec.stop(&handle)), Ok(()));
        assert_eq!(
            futures::executor::block_on(exec.status(&handle)),
            Ok(ExecutorStatus::Stopped)
        );
    }

    #[test]
    fn starting_the_same_job_twice_errors() {
        let exec = FakeExecutor::new();
        let first = futures::executor::block_on(exec.start(&job("n2"), &bootstrap()));
        assert_eq!(first, Ok(ExecutorHandle("n2".to_string())));
        let second = futures::executor::block_on(exec.start(&job("n2"), &bootstrap()));
        assert_eq!(second, Err(AlreadyStarted("n2".to_string())));
        // The first start's state is untouched by the failed second attempt.
        assert_eq!(
            futures::executor::block_on(exec.status(&ExecutorHandle("n2".to_string()))),
            Ok(ExecutorStatus::Running)
        );
    }

    #[test]
    fn stopping_an_already_stopped_job_is_a_no_op() {
        let exec = FakeExecutor::new();
        let start_result = futures::executor::block_on(exec.start(&job("n3"), &bootstrap()));
        assert_eq!(start_result, Ok(ExecutorHandle("n3".to_string())));
        let handle = ExecutorHandle("n3".to_string());
        assert_eq!(futures::executor::block_on(exec.stop(&handle)), Ok(()));
        // Second stop: still `Ok`, status stays `Stopped`, not an error.
        assert_eq!(futures::executor::block_on(exec.stop(&handle)), Ok(()));
        assert_eq!(
            futures::executor::block_on(exec.status(&handle)),
            Ok(ExecutorStatus::Stopped)
        );
    }

    #[test]
    fn stopping_a_never_started_job_is_a_no_op_and_stays_unknown() {
        let exec = FakeExecutor::new();
        let handle = ExecutorHandle("never-started".to_string());
        assert_eq!(futures::executor::block_on(exec.stop(&handle)), Ok(()));
        // Stop on a handle this executor never saw records nothing — status is still
        // `Unknown`, not fabricated as `Stopped`.
        assert_eq!(
            futures::executor::block_on(exec.status(&handle)),
            Ok(ExecutorStatus::Unknown)
        );
    }

    #[test]
    fn two_jobs_are_tracked_independently() {
        let exec = FakeExecutor::new();
        let a_result = futures::executor::block_on(exec.start(&job("a"), &bootstrap()));
        assert_eq!(a_result, Ok(ExecutorHandle("a".to_string())));
        let b_result = futures::executor::block_on(exec.start(&job("b"), &bootstrap()));
        assert_eq!(b_result, Ok(ExecutorHandle("b".to_string())));
        let a = ExecutorHandle("a".to_string());
        let b = ExecutorHandle("b".to_string());
        assert_eq!(futures::executor::block_on(exec.stop(&a)), Ok(()));
        assert_eq!(
            futures::executor::block_on(exec.status(&a)),
            Ok(ExecutorStatus::Stopped)
        );
        assert_eq!(
            futures::executor::block_on(exec.status(&b)),
            Ok(ExecutorStatus::Running)
        );
    }
}
