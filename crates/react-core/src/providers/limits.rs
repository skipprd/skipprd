/// Provider-side concurrency defaults and guardrails.
///
/// These are intentionally conservative and centralized so that suites and runtime implementations
/// can remain consistent without duplicating magic numbers across crates.
///
/// Default max in-flight concurrency for warehouse providers.
///
/// Providers may override this (and suites should respect provider-specific `max_concurrency()`).
pub const DEFAULT_WAREHOUSE_MAX_CONCURRENCY: usize = 15;
