const PREFACE: &[u8; 24] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    Http1,
    Http2,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ProbeError {
    AlreadySelected,
    InvalidCount,
}

pub(super) struct Probe {
    bytes: [u8; PREFACE.len()],
    len: usize,
}

impl Probe {
    pub(super) fn new() -> Self {
        Self {
            bytes: [0; PREFACE.len()],
            len: 0,
        }
    }

    pub(super) fn remaining(&self) -> usize {
        self.bytes.len() - self.len
    }

    pub(super) fn bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }

    pub(super) fn protocol(&self) -> Option<Protocol> {
        if self.bytes() != &PREFACE[..self.len] {
            Some(Protocol::Http1)
        } else if self.len == PREFACE.len() {
            Some(Protocol::Http2)
        } else {
            None
        }
    }

    pub(super) fn accept(&mut self, bytes: &[u8]) -> Result<Option<Protocol>, ProbeError> {
        if self.protocol().is_some() {
            return Err(ProbeError::AlreadySelected);
        }
        if bytes.len() > self.remaining() {
            return Err(ProbeError::InvalidCount);
        }
        let end = self.len + bytes.len();
        self.bytes[self.len..end].copy_from_slice(bytes);
        self.len = end;
        Ok(self.protocol())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_three_part_preface_preserves_every_byte() {
        for first in 0..=PREFACE.len() {
            for second in first..=PREFACE.len() {
                let mut probe = Probe::new();
                for range in [0..first, first..second, second..PREFACE.len()] {
                    if range.is_empty() {
                        continue;
                    }
                    let end = range.end;
                    let selected = probe.accept(&PREFACE[range]).unwrap();
                    assert_eq!(probe.bytes(), &PREFACE[..end]);
                    assert_eq!(selected, (end == PREFACE.len()).then_some(Protocol::Http2));
                }
                assert_eq!(probe.remaining(), 0);
                assert_eq!(probe.protocol(), Some(Protocol::Http2));
            }
        }
    }

    #[test]
    fn bytewise_preface_waits_for_the_complete_signature() {
        let mut probe = Probe::new();
        for (index, byte) in PREFACE.iter().enumerate() {
            assert_eq!(
                probe.accept(std::slice::from_ref(byte)).unwrap(),
                (index + 1 == PREFACE.len()).then_some(Protocol::Http2)
            );
        }
        assert_eq!(probe.bytes(), PREFACE);
    }

    #[test]
    fn every_mismatch_selects_http1_without_losing_the_read_tail() {
        for index in 0..PREFACE.len() {
            for value in 0..=u8::MAX {
                if value == PREFACE[index] {
                    continue;
                }
                let mut received = *PREFACE;
                received[index] = value;
                let mut probe = Probe::new();
                assert_eq!(probe.accept(&received[..index]).unwrap(), None);
                assert_eq!(
                    probe.accept(&received[index..]).unwrap(),
                    Some(Protocol::Http1)
                );
                assert_eq!(probe.bytes(), received);
            }
        }
    }

    #[test]
    fn rejected_input_leaves_the_prefix_unchanged() {
        let mut probe = Probe::new();
        probe.accept(&PREFACE[..9]).unwrap();
        assert_eq!(probe.accept(PREFACE), Err(ProbeError::InvalidCount));
        assert_eq!(probe.bytes(), &PREFACE[..9]);
        assert_eq!(probe.protocol(), None);
        probe.accept(&PREFACE[9..]).unwrap();
        assert_eq!(probe.accept(b"x"), Err(ProbeError::AlreadySelected));
        assert_eq!(probe.bytes(), PREFACE);
    }
}
