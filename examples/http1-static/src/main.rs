use std::time::Duration;

use http1_static::driver::{Options, run};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut options = Options {
        bind: "127.0.0.1:8080".parse()?,
        root: ".".into(),
        max_connections: 128,
        timeout: Duration::from_secs(10),
        stop_after: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        if argument == "--help" {
            println!(
                "http1-static [--bind IPv4:PORT] [--root DIR] [--max-connections 1..256] [--timeout-ms MS] [--stop-after EXCHANGES]"
            );
            return Ok(());
        }
        let value = args.next().ok_or("option requires a value")?;
        match argument.as_str() {
            "--bind" => options.bind = value.parse()?,
            "--root" => options.root = value.into(),
            "--max-connections" => options.max_connections = value.parse()?,
            "--timeout-ms" => options.timeout = Duration::from_millis(value.parse()?),
            "--stop-after" => {
                let count = value.parse()?;
                if count == 0 {
                    return Err("--stop-after must be positive".into());
                }
                options.stop_after = Some(count);
            }
            _ => return Err(format!("unknown option: {argument}").into()),
        }
    }
    run(options)?;
    Ok(())
}
