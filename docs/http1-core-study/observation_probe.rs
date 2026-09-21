#[path = "../../kimojio-fsm-http1/benches/support/mod.rs"]
mod support;

fn main() {
    println!(
        "Client={} Server={}",
        std::mem::size_of::<kimojio_fsm_http1::Client<Vec<u8>, &[u8]>>(),
        std::mem::size_of::<kimojio_fsm_http1::Server<Vec<u8>, &[u8]>>(),
    );
    for scenario in [
        support::Scenario::fixed(128),
        support::Scenario::chunked(1024 * 1024, support::IO_BYTES),
    ] {
        let mut client = support::client::<false, true>(&scenario);
        let mut server = support::server::<false, true>(&scenario);
        for _ in 0..10 {
            std::hint::black_box(client.round_trip());
            std::hint::black_box(server.round_trip());
        }
    }
}
