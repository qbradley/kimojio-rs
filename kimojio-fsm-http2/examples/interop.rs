//! Bounded, synchronous, nonblocking socket executor for independent HTTP/2 peers.
//! This fixture is not a production transport or a performance benchmark.

mod interop_support;

fn main() {
    if let Err(error) = interop_support::main() {
        eprintln!("interop: {error}");
        std::process::exit(1);
    }
}
