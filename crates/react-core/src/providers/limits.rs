/// Provider-side concurrency defaults and guardrails.
///
/// These are intentionally conservative and centralized so that suites and runtime implementations
/// can remain consistent without duplicating magic numbers across crates.
///
/// Notes:
/// - Athena has a soft limit on concurrent queries per account/workgroup (often ~20 by default).
/// - We pick a lower default to avoid thundering herds while still keeping catalog/build workflows fast.
pub const DEFAULT_ATHENA_MAX_CONCURRENCY: usize = 15;

/// Hard safety cap to keep us within typical Athena soft limits.
pub const ATHENA_MAX_CONCURRENCY_CAP: usize = 20;

pub fn clamp_athena_concurrency(n: usize) -> usize {
    n.max(1).min(ATHENA_MAX_CONCURRENCY_CAP)
}

