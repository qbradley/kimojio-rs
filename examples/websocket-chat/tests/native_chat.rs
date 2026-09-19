#[test]
fn actual_native_chat_wire_and_resource_lifecycle() {
    let result = std::process::Command::new("python3")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/native_chat.py"))
        .arg(env!("CARGO_BIN_EXE_websocket-chat"))
        .output()
        .expect("python3 is required for the native wire test");
    assert!(
        result.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr),
    );
}
