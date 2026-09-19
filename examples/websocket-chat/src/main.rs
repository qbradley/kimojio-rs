use std::time::Duration;
use websocket_chat::driver::{self, Config};

fn configuration() -> Result<Option<Config>, String> {
    let mut config = Config::default();
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        if flag == "--help" {
            println!(
                "websocket-chat --bind ADDRESS [--run-for-ms 60000] [--shutdown-grace-ms 1000]\n\
                [--max-clients 64] [--max-message-bytes 1048576] [--max-queued-messages 32]\n\
                [--max-client-bytes 4194304] [--max-total-bytes 33554432] [--frame-bytes 16384]\n\
                [--send-buffer-bytes 65536] [--write-timeout-ms 5000] [--close-timeout-ms 1000]\n\
                [--idle-timeout-ms 60000] [--message-timeout-ms 30000] [--frame-timeout-ms 30000]\n\
                [--handshake-timeout-ms 5000]"
            );
            return Ok(None);
        }
        let value = args
            .next()
            .ok_or_else(|| format!("{flag} requires a value"))?;
        if flag == "--bind" {
            config.bind = value.parse().map_err(|_| "invalid bind address")?;
            continue;
        }
        let number: u64 = value
            .parse()
            .map_err(|_| format!("invalid value for {flag}"))?;
        let size = usize::try_from(number).map_err(|_| "value exceeds addressable storage")?;
        let ns = || {
            number
                .checked_mul(1_000_000)
                .filter(|n| *n > 0)
                .ok_or_else(|| format!("invalid duration for {flag}"))
        };
        match flag.as_str() {
            "--run-for-ms" => config.run_for = Duration::from_millis(number),
            "--shutdown-grace-ms" => config.shutdown_grace = Duration::from_millis(number),
            "--max-clients" | "--max-connections" => config.chat.hub.max_clients = size,
            "--max-message-bytes" => config.chat.hub.max_message_bytes = size,
            "--max-queued-messages" => config.chat.hub.max_messages_per_client = size,
            "--max-client-bytes" => config.chat.hub.max_client_bytes = size,
            "--max-total-bytes" => config.chat.hub.max_total_bytes = size,
            "--frame-bytes" => config.chat.websocket.outgoing_frame_bytes = size,
            "--send-buffer-bytes" => config.send_buffer_bytes = size,
            "--write-timeout-ms" => config.chat.websocket.write_timeout_ns = Some(ns()?),
            "--close-timeout-ms" => config.chat.websocket.close_timeout_ns = Some(ns()?),
            "--idle-timeout-ms" => config.chat.websocket.idle_timeout_ns = Some(ns()?),
            "--message-timeout-ms" => config.chat.websocket.message_timeout_ns = Some(ns()?),
            "--frame-timeout-ms" => config.chat.websocket.frame_timeout_ns = Some(ns()?),
            "--handshake-timeout-ms" => {
                config.chat.http.head_timeout_ns = Some(ns()?);
                config.chat.http.idle_timeout_ns = Some(ns()?);
            }
            _ => return Err(format!("unknown option {flag}")),
        }
    }
    config.chat.websocket.max_message_bytes = config.chat.hub.max_message_bytes as u64;
    config.chat.websocket.max_frame_bytes = config.chat.websocket.max_message_bytes;
    config.chat.websocket.max_buffer_bytes = config
        .chat
        .receive_bytes
        .max(config.chat.hub.max_message_bytes);
    config.chat.websocket.outgoing_frame_bytes = config
        .chat
        .websocket
        .outgoing_frame_bytes
        .min(config.chat.hub.max_message_bytes);
    Ok(Some(config))
}

fn main() {
    let result = match configuration() {
        Ok(Some(config)) => match kimojio::run(0, driver::run(config)) {
            Some(Ok(result)) => result.map(|_| ()),
            Some(Err(_)) => Err("native runtime panicked".into()),
            None => Err("native runtime stopped before resource settlement".into()),
        },
        Ok(None) => Ok(()),
        Err(error) => Err(error),
    };
    if let Err(error) = result {
        eprintln!("websocket-chat: {error}");
        std::process::exit(1);
    }
}
