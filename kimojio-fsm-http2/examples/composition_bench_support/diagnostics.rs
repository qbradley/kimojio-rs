use std::{collections::VecDeque, fmt};

pub(super) struct Diagnostics {
    pub failed: bool,
    pub turn: usize,
    recent: VecDeque<String>,
    before_failure: Option<VecDeque<String>>,
    important: Vec<String>,
    frame_counts: [usize; 10],
    one_byte_data: usize,
    one_byte_refunds: usize,
    preface: usize,
    header: [u8; 9],
    header_len: usize,
    remaining: usize,
    prefix: [u8; 8],
    prefix_len: usize,
}

impl Diagnostics {
    pub fn from_env(server: bool) -> Option<Box<Self>> {
        std::env::var_os("BENCH_DIAGNOSTICS").map(|_| Box::new(Self::new(server)))
    }

    fn new(server: bool) -> Self {
        Self {
            failed: false,
            turn: 0,
            recent: VecDeque::with_capacity(256),
            before_failure: None,
            important: Vec::with_capacity(32),
            frame_counts: [0; 10],
            one_byte_data: 0,
            one_byte_refunds: 0,
            preface: if server { 0 } else { 24 },
            header: [0; 9],
            header_len: 0,
            remaining: 0,
            prefix: [0; 8],
            prefix_len: 0,
        }
    }

    pub fn record(&mut self, message: fmt::Arguments<'_>) {
        if self.recent.len() == 256 {
            self.recent.pop_front();
        }
        self.recent
            .push_back(format!("turn={} {message}", self.turn));
    }

    pub fn failure(&mut self, message: fmt::Arguments<'_>) {
        if !self.failed {
            self.before_failure = Some(self.recent.clone());
        }
        self.failed = true;
        if self.important.len() < 32 {
            self.important
                .push(format!("turn={} FAILURE {message}", self.turn));
        }
        self.record(message);
    }

    pub fn wire(&mut self, mut bytes: &[u8]) {
        let skip = self.preface.min(bytes.len());
        self.preface -= skip;
        bytes = &bytes[skip..];
        while !bytes.is_empty() {
            if self.header_len < 9 {
                let count = (9 - self.header_len).min(bytes.len());
                self.header[self.header_len..self.header_len + count]
                    .copy_from_slice(&bytes[..count]);
                self.header_len += count;
                bytes = &bytes[count..];
                if self.header_len < 9 {
                    continue;
                }
                self.remaining =
                    u32::from_be_bytes([0, self.header[0], self.header[1], self.header[2]])
                        as usize;
                self.prefix_len = 0;
            }
            let count = self.remaining.min(bytes.len());
            let prefix_count = count.min(8 - self.prefix_len);
            self.prefix[self.prefix_len..self.prefix_len + prefix_count]
                .copy_from_slice(&bytes[..prefix_count]);
            self.prefix_len += prefix_count;
            self.remaining -= count;
            bytes = &bytes[count..];
            if self.remaining == 0 {
                let kind = self.header[3];
                let flags = self.header[4];
                let stream = u32::from_be_bytes(self.header[5..9].try_into().unwrap());
                let length =
                    u32::from_be_bytes([0, self.header[0], self.header[1], self.header[2]]);
                let prefix = self.prefix;
                if let Some(count) = self.frame_counts.get_mut(usize::from(kind)) {
                    *count += 1;
                }
                self.one_byte_data += usize::from(kind == 0 && length == 1);
                self.one_byte_refunds += usize::from(kind == 8 && prefix[..4] == [0, 0, 0, 1]);
                self.record(format_args!(
                    "WIRE kind={kind} flags={flags} stream={stream} length={length} prefix={:02x?}",
                    &prefix[..self.prefix_len]
                ));
                if matches!(kind, 3 | 7) && self.important.len() < 32 {
                    self.important.push(format!(
                        "turn={} WIRE kind={kind} flags={flags} stream={stream} length={length} prefix={:02x?}",
                        self.turn, &prefix[..self.prefix_len]
                    ));
                }
                self.header_len = 0;
            }
        }
    }

    pub fn dump(&self, endpoint: &str) {
        eprintln!(
            "{endpoint} frame_counts={:?} one_byte_data={} one_byte_refunds={}",
            self.frame_counts, self.one_byte_data, self.one_byte_refunds
        );
        if let Some(before_failure) = &self.before_failure {
            eprintln!("=== {endpoint}: before first failure ===");
            for line in before_failure {
                eprintln!("{line}");
            }
        }
        eprintln!("=== {endpoint}: important ===");
        for line in &self.important {
            eprintln!("{line}");
        }
        eprintln!("=== {endpoint}: last {} events ===", self.recent.len());
        for line in &self.recent {
            eprintln!("{line}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fragmented_preface_frames_and_goaway() {
        let mut diagnostics = Diagnostics::new(false);
        for byte in b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n" {
            diagnostics.wire(&[*byte]);
        }
        for byte in [0, 0, 1, 0, 0, 0, 0, 0, 3, 0x5a] {
            diagnostics.wire(&[byte]);
        }
        diagnostics.wire(&[0, 0, 4, 8, 0, 0, 0, 0, 3, 0, 0, 0, 1]);
        diagnostics.wire(&[0, 0, 8, 7, 0, 0, 0, 0, 0, 0, 0, 0, 15, 0, 0, 0, 11]);
        assert_eq!(diagnostics.frame_counts[0], 1);
        assert_eq!(diagnostics.frame_counts[8], 1);
        assert_eq!(diagnostics.frame_counts[7], 1);
        assert_eq!(diagnostics.one_byte_data, 1);
        assert_eq!(diagnostics.one_byte_refunds, 1);
        assert!(diagnostics.important[0].contains("00, 00, 00, 0b"));
        assert_eq!(diagnostics.header_len, 0);
    }

    #[test]
    fn first_failure_snapshot_and_history_are_bounded() {
        let mut diagnostics = Diagnostics::new(true);
        for index in 0..300 {
            diagnostics.record(format_args!("before {index}"));
        }
        diagnostics.failure(format_args!("first"));
        for index in 0..300 {
            diagnostics.failure(format_args!("after {index}"));
        }
        assert!(diagnostics.failed);
        assert_eq!(diagnostics.recent.len(), 256);
        assert_eq!(diagnostics.important.len(), 32);
        let snapshot = diagnostics.before_failure.unwrap();
        assert_eq!(snapshot.len(), 256);
        assert!(snapshot.front().unwrap().ends_with("before 44"));
        assert!(snapshot.back().unwrap().ends_with("before 299"));
    }
}
