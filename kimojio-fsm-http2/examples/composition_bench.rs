//! CPU-only HTTP/2 qualification. See docs/http2-performance/README.md.
mod composition_bench_support;

use composition_bench_support::{Case, run};

fn main() {
    let args: Vec<_> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("direct");
    let case = Case::named(args.get(2).map(String::as_str).unwrap_or("empty"));
    let concurrency = args.get(3).map(|s| s.parse().unwrap()).unwrap_or(1);
    let fragment = args.get(4).map(|s| s.parse().unwrap()).unwrap_or(65536);
    let batches = args.get(5).map(|s| s.parse().unwrap()).unwrap_or(1000);
    let phase = args.get(6).map(String::as_str).unwrap_or("steady");
    assert!(concurrency > 0 && concurrency <= 128 && fragment > 0 && batches > 0);
    println!("{}", run(mode, case, concurrency, fragment, batches, phase));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_and_fragmentation() {
        for mode in ["direct", "selected", "auto"] {
            for case in ["empty", "duplex", "32k", "1m", "paused"] {
                for fragment in [17, 1024, 65536] {
                    run(mode, Case::named(case), 1, fragment, 2, "steady");
                }
            }
        }
    }

    #[test]
    fn concurrency_and_construction() {
        for mode in ["direct", "selected", "auto"] {
            for concurrency in [1, 64, 128] {
                run(mode, Case::named("empty"), concurrency, 65536, 2, "steady");
                run(mode, Case::named("duplex"), concurrency, 65536, 2, "steady");
                run(mode, Case::named("paused"), concurrency, 17, 2, "steady");
            }
            run(mode, Case::named("empty"), 1, 17, 2, "construct");
        }
    }

    #[test]
    #[ignore = "frozen 5491242b: fragmented concurrent duplex returns ConnectionFailed; strict repro"]
    fn fragmented_duplex_regression() {
        run("direct", Case::named("duplex"), 8, 1024, 2, "steady");
    }

    #[test]
    #[ignore = "frozen 5491242b: concurrent 1MiB duplex returns ConnectionFailed; strict repro"]
    fn large_duplex_regression() {
        run("direct", Case::named("1m"), 8, 65536, 2, "steady");
    }
}
