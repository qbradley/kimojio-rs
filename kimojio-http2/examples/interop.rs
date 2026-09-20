//! Client-only independent-peer fixture for the native async HTTP/2 wrapper.

mod interop_support;

fn main() {
    if let Err(error) = interop_support::main() {
        eprintln!("wrapper interop: {error}");
        std::process::exit(1);
    }
}
