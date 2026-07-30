use std::future::Future;

const DEFAULT_RUNTIME_PLUGIN_WORKER_THREADS: usize = 4;
const MAX_RUNTIME_PLUGIN_WORKER_THREADS: usize = 16;

#[macro_export]
macro_rules! runtime_main {
    ($future:expr $(,)?) => {
        fn main() {
            $crate::runtime_main::run_runtime_main(
                concat!(env!("CARGO_PKG_NAME"), "-main"),
                $future,
            );
        }
    };
    ($thread_name:expr, $future:expr $(,)?) => {
        fn main() {
            $crate::runtime_main::run_runtime_main($thread_name, $future);
        }
    };
}

pub fn run_runtime_main<F>(thread_name: &'static str, future: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    let stack_size = std::env::var("SKIPPR_MAIN_THREAD_STACK_BYTES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(32 * 1024 * 1024);
    // Cap child plugin Tokio workers so each sink process cannot spawn ~num_cpus
    // threads; 16 concurrent children on a 64-core host previously meant ~1k threads.
    let worker_threads = runtime_plugin_worker_threads(
        std::env::var("SKIPPR_RUNTIME_PLUGIN_WORKER_THREADS")
            .ok()
            .as_deref(),
    );

    let handle = std::thread::Builder::new()
        .name(thread_name.to_string())
        .stack_size(stack_size)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(worker_threads)
                .enable_all()
                .build()
                .expect("failed to build Tokio runtime");
            runtime.block_on(future);
        })
        .expect("failed to spawn runtime plugin main thread");

    if let Err(panic) = handle.join() {
        std::panic::resume_unwind(panic);
    }
}

fn runtime_plugin_worker_threads(value: Option<&str>) -> usize {
    value
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_RUNTIME_PLUGIN_WORKER_THREADS)
        .clamp(1, MAX_RUNTIME_PLUGIN_WORKER_THREADS)
}

#[cfg(test)]
mod tests {
    use super::{
        runtime_plugin_worker_threads, DEFAULT_RUNTIME_PLUGIN_WORKER_THREADS,
        MAX_RUNTIME_PLUGIN_WORKER_THREADS,
    };

    #[test]
    fn runtime_plugin_worker_threads_defaults_to_four() {
        assert_eq!(
            runtime_plugin_worker_threads(None),
            DEFAULT_RUNTIME_PLUGIN_WORKER_THREADS
        );
        assert_eq!(
            runtime_plugin_worker_threads(Some("")),
            DEFAULT_RUNTIME_PLUGIN_WORKER_THREADS
        );
        assert_eq!(
            runtime_plugin_worker_threads(Some("invalid")),
            DEFAULT_RUNTIME_PLUGIN_WORKER_THREADS
        );
        assert_eq!(
            runtime_plugin_worker_threads(Some("0")),
            DEFAULT_RUNTIME_PLUGIN_WORKER_THREADS
        );
    }

    #[test]
    fn runtime_plugin_worker_threads_honors_bounded_override() {
        assert_eq!(runtime_plugin_worker_threads(Some("1")), 1);
        assert_eq!(runtime_plugin_worker_threads(Some("8")), 8);
        assert_eq!(
            runtime_plugin_worker_threads(Some("128")),
            MAX_RUNTIME_PLUGIN_WORKER_THREADS
        );
    }
}
