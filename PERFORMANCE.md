# Performance Analysis

Notes from profiling Skippr under load. These are reference observations, not prescriptive targets.

## mpstat

Analysis of CPU efficiency:

- ~64% user time (`%usr`) — program consuming a significant portion of CPU
- ~11-12% system time (`%sys`) — kernel operations (syscalls, memory management)
- ~23-25% idle time (`%idle`) — some CPU cycles still available
- Very low `%iowait` (~0.05%) — not bottlenecked by disk I/O
- Low `%irq` and `%softirq` — not impacted by excessive interrupts

The program is CPU-bound with efficient I/O. High `%sys` may indicate frequent syscalls that could be batched. Profile with:

```bash
strace -c -p <pid>
```

## vmstat 1

Key metrics under heavy load:

- **User (us):** ~60-70% — CPU-bound workload
- **System (sy):** ~10-13% — moderate kernel activity
- **Idle (id):** ~18-29% — some CPU headroom
- **I/O Wait (wa):** 0% — no disk bottleneck
- **Run queue (r):** ~16-37 processes — high CPU demand
- **Blocked (b):** 0 — no I/O blocking
- **Swap (swpd):** 0 — no swap pressure
- **Free memory:** ~52GB available
- **Context switches (cs):** ~1.7M-1.9M — very high, characteristic of the Tokio multi-threaded runtime

Consider CPU affinity (`taskset`) to reduce task switching overhead if context switches become a bottleneck:

```bash
taskset -c 0-7 skippr-el sync --pipeline my_pipeline
```

## strace (PID 12244)

Syscall breakdown:

| Syscall | Time % | Notes |
|---|---|---|
| `clock_nanosleep` | 92.09% | Tokio runtime idle time (sleeping between async polls) |
| `futex` | 2.12% | Thread synchronization — 31 errors indicate contention |
| `restart_syscall` | 2.02% | Signal interruptions restarting syscalls |
| `statx` | — | 1,257 calls — frequent file checks (WAL segment scanning) |
| `unlinkat`, `mkdir`, `openat` | — | WAL segment lifecycle (create, check, delete) |

The high `clock_nanosleep` is expected for an async runtime — Tokio sleeps between work. The `futex` contention and `statx` frequency are areas to watch:

- Reduce lock contention with finer-grained locks or lock-free structures
- Cache file metadata to reduce `statx` calls
- Batch file operations where possible

Profile thread activity:

```bash
perf record -g -p <pid>
perf report
```
