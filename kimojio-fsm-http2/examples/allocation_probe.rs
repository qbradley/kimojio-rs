//! Allocation-only measurement of the strict in-memory HTTP/2 workload.
#[path = "composition_bench_support/allocation_counter.rs"]
mod allocation_counter;
#[allow(dead_code, reason = "the timed entry point is not used by this probe")]
mod composition_bench_support;

use allocation_counter::CountingAllocator;
use composition_bench_support::{
    Case,
    allocation::{Workload, run},
};

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator::new();

fn main() {
    let args: Vec<_> = std::env::args().collect();
    let workload = Workload {
        mode: args.get(1).map(String::as_str).unwrap_or("direct"),
        case: Case::named(args.get(2).map(String::as_str).unwrap_or("empty")),
        concurrency: args.get(3).map(|s| s.parse().unwrap()).unwrap_or(1),
        fragment: args.get(4).map(|s| s.parse().unwrap()).unwrap_or(65536),
        batches: args.get(5).map(|s| s.parse().unwrap()).unwrap_or(1),
        phase: args.get(6).map(String::as_str).unwrap_or("cold"),
    };
    println!("{}", run(&ALLOCATOR, workload));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_routes_and_measurement_boundaries() {
        for mode in ["direct", "selected", "auto"] {
            for phase in ["cold", "steady"] {
                let result = run(
                    &ALLOCATOR,
                    Workload {
                        mode,
                        case: Case::named("duplex"),
                        concurrency: 8,
                        fragment: 1024,
                        batches: 1,
                        phase,
                    },
                );
                assert_eq!(result["exchanges"], 8);
                assert_eq!(result["payload_bytes"], 65536);
                assert_eq!(result["shutdown_settled"], true);
                assert!(result["allocation_counts"]["allocations"].as_u64().unwrap() > 0);
            }
        }
    }
}
