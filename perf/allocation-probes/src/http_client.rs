mod application {
    pub fn invoke_entry() -> impl std::process::Termination {
        main()
    }

    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../kimojio-http1/examples/bench_client.rs"
    ));
}

fn main() -> std::process::ExitCode {
    fsm_allocation_probes::finish(application::invoke_entry())
}
