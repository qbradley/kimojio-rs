#[path = "support/allocation.rs"]
mod allocation;
#[allow(dead_code, reason = "shared with the runtime and correctness benches")]
mod support;

#[global_allocator]
static ALLOCATOR: allocation::CountingAllocator = allocation::CountingAllocator::new();

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if let Ok(path) = std::env::var("KIMOJIO_RETENTION_OUTPUT") {
        ALLOCATOR.retention_enable(&path);
    }
    let control = std::env::var("KIMOJIO_RETENTION_CONTROL").ok();
    let result = kimojio::run(0, async move {
        if let Some(control) = control {
            ALLOCATOR.retention_control(control).await;
            Ok(())
        } else {
            support::entry(allocation::AllocationMeter(&ALLOCATOR)).await
        }
    });
    ALLOCATOR.snapshot("post_runtime_cleanup", 0);
    match result {
        Some(Ok(result)) => result,
        Some(Err(panic)) => std::panic::resume_unwind(panic),
        None => Err("runtime stopped without a main result".into()),
    }
}
