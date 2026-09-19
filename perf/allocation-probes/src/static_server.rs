mod application {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/http1-static/src/main.rs"
    ));

    pub fn invoke_entry() -> impl std::process::Termination {
        main()
    }
}

fn main() -> std::process::ExitCode {
    fsm_allocation_probes::finish(application::invoke_entry())
}
