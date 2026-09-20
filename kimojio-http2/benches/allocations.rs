#[path = "support/allocation.rs"]
mod allocation;
#[allow(dead_code, reason = "shared with the runtime and correctness benches")]
mod support;

#[global_allocator]
static ALLOCATOR: allocation::CountingAllocator = allocation::CountingAllocator::new();

#[kimojio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    support::entry(allocation::AllocationMeter(&ALLOCATOR)).await
}
