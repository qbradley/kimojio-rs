//! Independent-peer client and server fixture for native and generic HTTP/2 wrappers.

mod interop_support;

fn main() {
    if let Err(error) = interop_support::main() {
        eprintln!("wrapper interop: {error}");
        std::process::exit(1);
    }
}
