use std::future::Future;

pub fn run_runtime_main<F>(thread_name: &'static str, future: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    let stack_size = std::env::var("SKIPPR_MAIN_THREAD_STACK_BYTES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(32 * 1024 * 1024);

    let handle = std::thread::Builder::new()
        .name(thread_name.to_string())
        .stack_size(stack_size)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_multi_thread()
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
