# Skippr

### What is Skippr?

Skippr is a tool for data ingestion and transformation. It is designed to ingest data from a source and transform it into a destination datalake/warehouse.

### Project Structure

- `src/` - Source code for the Skippr CLI and library
- `data/` - Data directory for the Skippr CLI
- `target/` - Build output for the Skippr CLI

### Building the Project

#### Local Development Build
```bash
cargo run sync
```

#### Release Builds

For MacOS:
```bash
SDKROOT=$(xcrun -sdk macosx12.3 --show-sdk-path) \
MACOSX_DEPLOYMENT_TARGET=$(xcrun -sdk macosx12.3 --show-sdk-platform-version) \
cargo build --release --target=x86_64-apple-darwin
```

For Linux:
```bash
cargo build --target x86_64-unknown-linux-gnu --release
```

### Configuration

Skippr is configured through environment variables. Here are the key configuration options:

#### Data Source Configuration
- `DATA_SOURCE_PLUGIN_NAME` - Source plugin to use (e.g. 's3', 's3_inventory', 'stdin')
- `DATA_SOURCE_S3_BUCKET` - Source S3 bucket
- `DATA_SOURCE_S3_PREFIX` - Source S3 prefix
- `DATA_SOURCE_BATCH_SIZE_BYTES` - Batch size in bytes
- `DATA_SOURCE_EVENT_TYPE_FIELDS` - Fields to use for event type

#### Transform Configuration  
- `TRANSFORM_FLATTEN_EVENTS` - Whether to flatten nested JSON events
- `TRANSFORM_NAMESPACE_FIELDS` - Fields to use for namespacing
- `TRANSFORM_BATCH_TIME_FIELDS` - Fields to use for time-based partitioning
- `TRANSFORM_BATCH_TIME_UNIT` - Time unit for partitioning (day/year)

#### Output Configuration
- `DATA_OUTPUT_PLUGIN_NAME` - Output plugin to use (e.g. 'athena')
- `DATA_OUTPUT_S3_BUCKET` - Destination S3 bucket
- `DATA_OUTPUT_S3_PREFIX` - Destination S3 prefix
- `SCHEMA_OUTPUT_PLUGIN_NAME` - Schema output plugin (e.g. 'glue')
- `SCHEMA_OUTPUT_GLUE_DATABASE_NAME` - Glue database name
- `DATA_OUTPUT_ATHENA_WORKGROUP_NAME` - Athena workgroup name

#### JSON Parsing Configuration
- `SKIPPR_ENABLE_SINGLE_QUOTE_PARSING` - Enable parsing of JSON with single quotes (default: false)
- `SKIPPR_ENABLE_UNICODE_PARSING` - Enable parsing of Unicode prefixed strings like u'string' (default: false)

### Data Type Detection

Skippr automatically detects data types from your input data. Here are some key behaviors to be aware of:

#### Timestamp Detection
- Integer values are considered timestamps only if they are 10-11 digits (seconds) or 13 digits (milliseconds)
- For security and data quality reasons, only timestamps after January 1, 2010 are recognized as valid timestamps
- This prevents small integers from being incorrectly identified as timestamps

### Example Usage

Basic S3 to Athena pipeline:
```bash
AWS_PROFILE=skippr-test \
DATA_SOURCE_PLUGIN_NAME=s3 \
DATA_SOURCE_S3_BUCKET=skippr-e2e-sample-data \
DATA_SOURCE_S3_PREFIX=bike-hire \
DATA_SOURCE_BATCH_SIZE_BYTES=10048000 \
DATA_OUTPUT_PLUGIN_NAME=athena \
DATA_OUTPUT_S3_BUCKET=skippr-e2e-sample-data-output \
DATA_OUTPUT_S3_PREFIX=bikehire \
SCHEMA_OUTPUT_PLUGIN_NAME=glue \
SCHEMA_OUTPUT_GLUE_DATABASE_NAME=bikehire \
DATA_OUTPUT_ATHENA_WORKGROUP_NAME=bikehire \
PIPELINE_NAME=bikehire \
SKIPPR_API_TOKEN=your_api_token \
DATA_DIR=./data \
APP_ENV=dev \
cargo run sync
```

### Docker Usage

```bash
docker run --platform=linux/x86_64 \
-e AWS_DEFAULT_REGION=eu-west-1 \
-e AWS_ACCESS_KEY_ID=$AWS_ACCESS_KEY_ID \
-e AWS_SECRET_ACCESS_KEY=$AWS_SECRET_ACCESS_KEY \
-e DATA_SOURCE_PLUGIN_NAME=s3_inventory \
-e DATA_SOURCE_S3_BUCKET=source-bucket \
-e DATA_SOURCE_S3_PREFIX=/example/inventory-dir \
-e DATA_OUTPUT_S3_BUCKET=dest-bucket \
-e DATA_OUTPUT_S3_PREFIX=example \
-e SCHEMA_OUTPUT_GLUE_DATABASE_NAME=test123 \
-e DATA_OUTPUT_ATHENA_WORKGROUP_NAME=test123 \
-e PIPELINE_NAME=test123 \
-e SKIPPR_API_TOKEN=your_api_token \
-e DATA_DIR=./ \
-e APP_ENV=test \
-v `pwd`/data:/data \
skippr/skipprd:v3.1.0
```

# Performance Analysis

### mpstat

Analysis of Your CPU Efficiency (mpstat Output)
Your system's CPU utilization shows:

~64% user time (%usr) → Your program is consuming a significant portion of CPU.
~11-12% system time (%sys) → Kernel operations (syscalls, memory management, etc.).
~23-25% idle time (%idle) → Some CPU cycles are still available.
Very low %iowait (~0.05%) → Not bottlenecked by disk I/O.
Low %irq and %softirq → Not impacted by excessive interrupts.
Is Your Program Efficient?
✅ Yes, it's fairly efficient.
Here's why:

Good CPU utilization (~64%): Your program is making full use of CPU without overloading it.
Low I/O wait (~0.05%): The program is not stuck waiting for disk access, meaning it efficiently processes data in RAM.
Low IRQ & SoftIRQ: No excessive hardware/network interrupts, indicating that performance is CPU-bound rather than being slowed down by other hardware.
Potential Areas for Optimization
High %sys (11-12%)

If the workload is CPU-intensive but involves frequent syscalls (e.g., file operations, networking, memory allocations), consider reducing context switches or optimizing system calls.
Solution: Profile the system calls using:
bash
Copy
Edit
strace -c -p <pid_of_your_program>
If too many syscalls, batch operations together.
Moderate Idle Time (23-25%)

While this is normal, if your program should be fully using all CPU cores, check for possible inefficiencies in threading or parallelization.
Solution: If running multi-threaded, ensure workload is evenly distributed using htop or numactl --hardware.
Check for Load Balancing Across Cores

Your core usage is fairly uniform (good!), but slight variations in %idle indicate some cores might be underutilized.
Solution: Use CPU affinity (taskset) or thread pinning to distribute workload more evenly.
bash
Copy
Edit
taskset -c 0-7 your_program
Final Verdict:
Your program is well-optimized with no major inefficiencies. 🚀

### vmstat 1

Analysis of Your vmstat Output
Your system appears to be running under a heavy load but is handling it relatively well. Let's break down the key metrics.

1️⃣ CPU Usage (us, sy, id, wa)
User (us): ~60-70%
→ Your program is consuming a significant portion of CPU time. This suggests a CPU-bound workload (e.g., computations, processing).
System (sy): ~10-13%
→ Kernel activity is moderate, meaning system calls, memory management, and I/O operations are happening but not excessive.
Idle (id): ~18-29%
→ Some CPU cycles are still free, but the system is fairly busy.
I/O Wait (wa): 0% consistently
→ Excellent! Your CPU is not blocked by slow disk operations. Your program efficiently keeps things in memory.
✅ Conclusion:
Your program is efficiently utilizing CPU without excessive system overhead. If you'd like to increase CPU usage efficiency, consider threading optimization (if applicable).

2️⃣ Load and Process Activity (r, b)
Run queue (r): ~16-37 processes
→ High, meaning many processes are actively waiting for CPU time. If the r value is higher than your CPU core count, then processes are competing for CPU.
Blocked (b): 0
→ Good sign! No processes are stuck waiting for I/O or unresponsive disk operations.
✅ Conclusion:
Your CPU is under high usage but handling it well. If r exceeds core count for long periods, performance tuning may be needed.

3️⃣ Memory Usage (swpd, free, buff, cache)
Swap (swpd): 0
→ No swap usage → Excellent! Your system has enough RAM to handle the workload without swapping to disk.
Free Memory (free): ~52GB
→ Plenty of RAM available, so memory pressure is not an issue.
Buffers (buff) and Cache (cache):
Buffers: ~220MB
Cache: ~2.5GB
→ The system is keeping recently used files in cache, which is normal.
✅ Conclusion:
Your system is not memory-constrained and has no swap usage—this is very good for performance!

4️⃣ Disk and I/O (bi, bo)
Block In (bi): ~0-311 KB/s

Block Out (bo): ~948-21,116 KB/s

→ Low to moderate I/O activity, mostly writes (bo).
→ No backlog (b is 0), so the system isn't stuck waiting for I/O.

✅ Conclusion:
Your system is not I/O-bound, meaning disk performance is not a bottleneck.

5️⃣ System Calls and Context Switches (in, cs)
Interrupts (in): ~80,000 - 107,000

Context switches (cs): ~1.7M - 1.9M

→ Very high context switches, which suggests:

A highly multi-threaded workload.
Frequent CPU task switching (potential inefficiency).
📌 Possible Optimization:

Check if too many small tasks are switching between CPU cores.
Use CPU affinity (taskset) to pin processes to cores and reduce task switching overhead.
Profile thread activity with:
bash
Copy
Edit
perf sched record
perf sched report
Final Verdict:
✅ Your system is handling the load efficiently.
✅ No swap usage, low I/O wait, and plenty of RAM available.
✅ Your workload is heavily CPU-bound but not I/O constrained.
🔸 Context switching is high—consider tuning threading and CPU affinity for optimization.




### sudo strace -c -p 12244


Analysis of Your strace Output
This output shows the syscalls your process (PID 12244) is making, their time consumption, and call frequency.

1️⃣ Key Findings
clock_nanosleep dominates (92.09%)
→ Your program spends most of its time sleeping (likely due to rate-limiting, waiting for an event, or inefficient delays).
futex calls (2.12%) with 31 errors
→ Indicates thread synchronization overhead, possibly due to contention.
restart_syscall (2.02%)
→ Occurs when a syscall is interrupted by a signal and needs to restart (common in multi-threaded apps).
File system activity (statx, unlinkat, openat, getdents64, mkdir)
→ Your program is frequently checking file statuses, reading directories, and creating/deleting files.
2️⃣ Potential Bottlenecks & Optimizations
🔹 clock_nanosleep (92%) → Too Much Sleeping!
Possible causes:

Intentional delay (e.g., polling loops, rate-limiting).
Inefficient waiting strategy (e.g., busy-waiting with sleep).
Fix:
Replace sleep with event-driven waits (e.g., epoll, select for I/O tasks).
Reduce unnecessary sleep calls.
🔹 futex Calls (2.12%) → Thread Contention?
Possible causes:

High thread synchronization overhead (lock contention).
Thread pool inefficiency.
Fix:
Reduce locking (e.g., use lock-free data structures or fine-grained locks).
Check thread pool settings (e.g., oversubscribed threads may cause contention).
Profile with:
bash
Copy
Edit
perf record -g -p 12244
perf report
🔹 restart_syscall (2%) → Signal Interruptions
Possible causes:

Signals (e.g., SIGALRM, SIGHUP) interrupting syscalls.
Fix:
Check which signals are causing interruptions:
bash
Copy
Edit
strace -p 12244 -e signal
Modify signal handling (e.g., SA_RESTART flag).
🔹 File System Activity (Stat, Open, Delete, Create)
High statx calls (1,257 times) → Frequent file checks.
Many unlinkat, mkdir, openat calls → File modifications.
Fix:
Cache file metadata instead of frequent statx calls.
Batch file operations when possible.
Use async I/O (io_uring, aio).
3️⃣ Final Verdict
✅ Your program is functional but could be optimized.
🔸 Major inefficiency: clock_nanosleep (92% sleep time).
🔸 Possible thread contention: futex calls (2%).
🔸 Too many file system lookups.

