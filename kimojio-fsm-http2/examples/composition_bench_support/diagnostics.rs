use std::{collections::VecDeque, fmt};

#[derive(Default, serde::Serialize)]
struct Application {
    head_commands: usize,
    end_stream_head_commands: usize,
    send_permits: usize,
    admissions: usize,
    admitted_bytes: usize,
    sent_ok: usize,
    sent_error: usize,
    sent_accepted_bytes: usize,
    received_body_bytes: usize,
    released_body_bytes: usize,
    held_leases: usize,
    held_body_bytes: usize,
    peak_held_leases: usize,
    peak_held_body_bytes: usize,
    complete_receive_ends: usize,
    write_operations: usize,
    data_write_operations: usize,
    first_head_turn: Option<usize>,
    first_permit_turn: Option<usize>,
    first_admission_turn: Option<usize>,
    first_body_turn: Option<usize>,
    first_release_turn: Option<usize>,
    first_data_write_turn: Option<usize>,
}

pub(super) struct Diagnostics {
    pub failed: bool,
    pub turn: usize,
    recent: VecDeque<String>,
    before_failure: Option<VecDeque<String>>,
    important: Vec<String>,
    application: Application,
    milestones: Vec<String>,
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
            application: Application::default(),
            milestones: Vec::with_capacity(64),
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

    fn milestone(&mut self, message: fmt::Arguments<'_>) {
        if self.milestones.len() < 64 {
            self.milestones
                .push(format!("turn={} {message}", self.turn));
        }
    }

    pub fn head_command(&mut self, stream: u32, end: bool, incoming_ended: bool, received: usize) {
        self.application.head_commands += 1;
        self.application.end_stream_head_commands += usize::from(end);
        self.application.first_head_turn.get_or_insert(self.turn);
        self.milestone(format_args!(
            "HEAD_COMMAND accepted stream={stream} end_stream={end} incoming_ended={incoming_ended} received={received}"
        ));
    }

    pub fn permit(&mut self, stream: u32, bytes: usize, retained_capacity: usize) {
        self.application.send_permits += 1;
        self.application.first_permit_turn.get_or_insert(self.turn);
        self.milestone(format_args!(
            "SEND_PERMIT stream={stream} max_bytes={bytes} max_retained_capacity={retained_capacity}"
        ));
    }

    pub fn admission(&mut self, stream: u32, bytes: usize, end: bool) {
        self.application.admissions += 1;
        self.application.admitted_bytes += bytes;
        self.application
            .first_admission_turn
            .get_or_insert(self.turn);
        self.milestone(format_args!(
            "ADMISSION accepted stream={stream} bytes={bytes} end_stream={end}"
        ));
    }

    pub fn sent(&mut self, accepted: usize, success: bool) {
        self.application.sent_ok += usize::from(success);
        self.application.sent_error += usize::from(!success);
        self.application.sent_accepted_bytes += accepted;
    }

    pub fn body_received(&mut self, bytes: usize) {
        self.application.received_body_bytes += bytes;
        self.application.held_leases += 1;
        self.application.held_body_bytes += bytes;
        self.application.peak_held_leases = self
            .application
            .peak_held_leases
            .max(self.application.held_leases);
        self.application.peak_held_body_bytes = self
            .application
            .peak_held_body_bytes
            .max(self.application.held_body_bytes);
        self.application.first_body_turn.get_or_insert(self.turn);
    }

    pub fn body_released(&mut self, bytes: usize) {
        assert!(self.application.held_leases > 0);
        assert!(self.application.held_body_bytes >= bytes);
        self.application.held_leases -= 1;
        self.application.held_body_bytes -= bytes;
        self.application.released_body_bytes += bytes;
        self.application.first_release_turn.get_or_insert(self.turn);
    }

    pub fn receive_end(&mut self, complete: bool) {
        self.application.complete_receive_ends += usize::from(complete);
    }

    pub fn write_issued(&mut self, data: bool) {
        self.application.write_operations += 1;
        self.application.data_write_operations += usize::from(data);
        if data {
            self.application
                .first_data_write_turn
                .get_or_insert(self.turn);
        }
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
            "{endpoint} APPLICATION {}",
            serde_json::to_string(&self.application).unwrap()
        );
        eprintln!("=== {endpoint}: first application milestones ===");
        for line in &self.milestones {
            eprintln!("{line}");
        }
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

    #[test]
    fn permission_and_lease_counters_distinguish_admission_from_wire_progress() {
        let mut diagnostics = Diagnostics::new(true);
        diagnostics.turn = 3;
        diagnostics.head_command(1, false, false, 0);
        diagnostics.turn = 4;
        diagnostics.permit(1, 65536, 65536);
        diagnostics.admission(1, 32768, false);
        diagnostics.body_received(16384);
        diagnostics.body_released(16384);
        diagnostics.write_issued(false);
        diagnostics.sent(0, false);
        let app = &diagnostics.application;
        assert_eq!(app.head_commands, 1);
        assert_eq!(app.end_stream_head_commands, 0);
        assert_eq!(app.send_permits, 1);
        assert_eq!(app.admitted_bytes, 32768);
        assert_eq!(app.received_body_bytes, app.released_body_bytes);
        assert_eq!(app.held_leases, 0);
        assert_eq!(app.held_body_bytes, 0);
        assert_eq!(app.peak_held_leases, 1);
        assert_eq!(app.peak_held_body_bytes, 16384);
        assert_eq!(app.first_head_turn, Some(3));
        assert_eq!(app.first_admission_turn, Some(4));
        assert_eq!(app.data_write_operations, 0);
        assert_eq!(app.first_data_write_turn, None);
        assert_eq!(app.sent_ok, 0);
        assert_eq!(app.sent_error, 1);
    }
}
