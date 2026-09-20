//! Shared allocation-free formatting used by both HTTP/1 and HTTP/2.

pub(crate) fn decimal_bytes(mut value: usize, storage: &mut [u8; 20]) -> &[u8] {
    let mut start = storage.len();
    loop {
        start -= 1;
        storage[start] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            return &storage[start..];
        }
    }
}
