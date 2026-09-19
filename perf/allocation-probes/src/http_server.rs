mod application {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../kimojio-http1/examples/server.rs"
    ));

    pub fn invoke_entry() -> impl std::process::Termination {
        main()
    }
}

fn main() -> std::process::ExitCode {
    fsm_allocation_probes::finish(application::invoke_entry())
}
