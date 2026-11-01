//! Stable operational event vocabulary shared by the Host and RPC boundaries.
//!
//! Codes 1–6 remain owned by [`crate::graft::KernelEventCode`]. Values are an
//! external log contract: add codes, but never renumber or reuse existing ones.
//! The subscriber supplies these literal fields from the code, so guest text
//! cannot replace a category, reason, or human-readable message.

pub const CONTRACT_VERSION: u64 = 1;

#[repr(u16)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventCode {
    HostStarted = 100,
    HostStopped = 101,
    HostFailed = 102,
    InvalidLogFormat = 103,
    HostJoinFailed = 104,
    HostWorkerFailed = 105,
    HostWorkerPanicked = 106,
    CompilerQueuePoisoned = 107,
    CompilerStartFailed = 108,
    CompilerPanicked = 109,
    KernelExited = 110,
    TelemetryDisabled = 111,
    CellExited = 200,
    CellFailed = 201,
    CellCleanupFailed = 202,
    CellTerminationRequested = 203,
    InstantiationTimedOut = 300,
    CacheUnavailable = 301,
    CacheConfigurationInvalid = 302,
    RpcFailed = 400,
    RpcCleanupFailed = 401,
    RpcRejected = 402,
    RpcListenerStopped = 403,
    RpcLoginTimedOut = 404,
    RpcCleanupTimedOut = 405,
    HttpRouteRegistered = 500,
    HttpRouteUnregistered = 501,
    DeploymentEpochAdvanced = 600,
    DeploymentRootActivated = 601,
    DeploymentRetry = 602,
    InvalidDeploymentHead = 603,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Contract {
    pub category: &'static str,
    pub operation: &'static str,
    pub outcome: &'static str,
    pub reason: &'static str,
    pub message: &'static str,
}

/// Return only recognized codes. Unknown values do not authorize free text.
pub fn contract(code: u64) -> Option<Contract> {
    let (category, operation, outcome, reason, message) = match code {
        1 => (
            "kernel",
            "initialize",
            "failed",
            "initial_init_failed",
            "Kernel generation failed to initialize",
        ),
        2 => (
            "kernel",
            "initialize",
            "failed",
            "epoch_init_failed",
            "Kernel generation failed to initialize",
        ),
        3 => (
            "kernel",
            "replace",
            "completed",
            "generation_replaced",
            "Kernel generation replaced",
        ),
        4 => (
            "kernel",
            "graft",
            "superseded",
            "epoch_advanced",
            "Kernel result superseded by newer generation",
        ),
        5 => (
            "kernel",
            "replace",
            "completed",
            "interactive_replaced",
            "Interactive PID0 generation was replaced; restart ww run",
        ),
        6 => (
            "kernel",
            "teardown",
            "timed_out",
            "teardown_deadline",
            "Kernel generation teardown timed out",
        ),
        111 => (
            "host",
            "configure_telemetry",
            "disabled",
            "configuration_or_start_failed",
            "Local trace export disabled: invalid configuration or exporter startup failed",
        ),
        100 => (
            "host",
            "start",
            "completed",
            "started",
            "Host service started",
        ),
        101 => (
            "host",
            "stop",
            "completed",
            "stopped",
            "Host service stopped",
        ),
        102 => (
            "host",
            "run",
            "failed",
            "service_failed",
            "Host service failed",
        ),
        103 => (
            "host",
            "configure",
            "fallback",
            "invalid_format",
            "invalid WW_LOG_FORMAT; expected auto, compact, or json; using auto",
        ),
        104 => (
            "host",
            "join",
            "failed",
            "join_failed",
            "Host task join failed",
        ),
        105 => (
            "host",
            "run",
            "failed",
            "worker_failed",
            "Host worker failed",
        ),
        106 => (
            "host",
            "run",
            "failed",
            "worker_panicked",
            "Host worker panicked",
        ),
        107 => (
            "host",
            "compile",
            "failed",
            "queue_poisoned",
            "Compiler queue lock poisoned",
        ),
        108 => (
            "host",
            "start",
            "failed",
            "worker_start_failed",
            "Compiler worker failed to start",
        ),
        109 => (
            "host",
            "compile",
            "failed",
            "worker_panicked",
            "Compiler worker panicked",
        ),
        110 => (
            "kernel",
            "run",
            "completed",
            "process_exited",
            "Kernel exited",
        ),
        200 => (
            "cell",
            "run",
            "completed",
            "process_exited",
            "Cell process exited",
        ),
        201 => (
            "cell",
            "run",
            "failed",
            "process_failed",
            "Cell process failed",
        ),
        202 => (
            "cell",
            "teardown",
            "failed",
            "cleanup_failed",
            "Cell cleanup failed",
        ),
        203 => (
            "cell",
            "terminate",
            "requested",
            "termination_request",
            "Cell termination requested",
        ),
        300 => (
            "p3",
            "instantiate",
            "timed_out",
            "instantiation_deadline",
            "Guest component instantiation timed out",
        ),
        301 => (
            "p3",
            "compile",
            "fallback",
            "cache_unavailable",
            "Wasmtime persistent cache unavailable; compiling without it",
        ),
        302 => (
            "p3",
            "compile",
            "fallback",
            "cache_configuration_invalid",
            "Wasmtime persistent cache configuration invalid; compiling without it",
        ),
        400 => (
            "rpc",
            "connection",
            "failed",
            "connection_failed",
            "RPC connection failed",
        ),
        401 => (
            "rpc",
            "teardown",
            "failed",
            "cleanup_failed",
            "RPC cleanup failed",
        ),
        402 => (
            "rpc",
            "accept",
            "rejected",
            "connection_budget_exhausted",
            "RPC connection budget exhausted",
        ),
        403 => (
            "rpc",
            "accept",
            "failed",
            "listener_ended",
            "RPC listener ended unexpectedly",
        ),
        404 => (
            "rpc",
            "authenticate",
            "timed_out",
            "login_deadline",
            "RPC login deadline expired",
        ),
        405 => (
            "rpc",
            "teardown",
            "timed_out",
            "cleanup_deadline",
            "RPC cleanup deadline expired",
        ),
        500 => (
            "rpc",
            "register_route",
            "completed",
            "route_registered",
            "registered HTTP route",
        ),
        501 => (
            "rpc",
            "remove_route",
            "completed",
            "route_unregistered",
            "unregistered HTTP route",
        ),
        600 => (
            "host",
            "advance_epoch",
            "completed",
            "authority_revoked",
            "Advancing deployment epoch; authority revoked",
        ),
        601 => (
            "host",
            "activate_deployment",
            "completed",
            "root_activated",
            "Deployment root activated",
        ),
        602 => (
            "host",
            "prepare_deployment",
            "retrying",
            "transient_failure",
            "Transient deployment preparation failure; retry scheduled",
        ),
        603 => (
            "host",
            "prepare_deployment",
            "rejected",
            "invalid_head",
            "Authoritative Stem update selected an invalid deployment head",
        ),
        _ => return None,
    };
    Some(Contract {
        category,
        operation,
        outcome,
        reason,
        message,
    })
}

/// Allocate a process-local connection identity, independent of peer-supplied
/// protocol names or addresses. IDs correlate accept/handler/pump diagnostics;
/// they do not identify a peer and must not become metric labels.
pub(crate) fn next_connection_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_CONNECTION_ID: AtomicU64 = AtomicU64::new(1);
    NEXT_CONNECTION_ID.fetch_add(1, Ordering::Relaxed)
}
