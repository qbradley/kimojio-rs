#[allow(
    dead_code,
    reason = "shared with the allocation and correctness benches"
)]
mod support;

#[kimojio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    support::entry(support::NoMeter).await
}
