use httparse::Header;

/// The largest number of header fields [`Http1HeaderScratch`] can lend at once.
///
/// The storage is a call-scoped array rather than a retained allocation, so
/// this bound is what keeps it off the heap and keeps the stack frame fixed:
/// 128 fields is 4 KiB on a 64-bit target. It sits above the 100-field default
/// header limit so an ordinary configuration is unaffected.
pub const MAX_SCRATCH_HEADERS: usize = 128;

/// Reusable parser storage for borrowed HTTP/1 header fields.
///
/// [`Self::with_input`] lends the storage and input with one call-scoped
/// lifetime. The callback's result cannot borrow either one, so callers must
/// consume or copy borrowed heads, trailers, and bodies before the callback
/// returns. Storage does not outlive the callback, making it safe to compact or
/// refill the input immediately afterward.
#[derive(Debug)]
pub struct Http1HeaderScratch {
    capacity: usize,
}

impl Http1HeaderScratch {
    /// Prepares storage for at most `capacity` parsed fields.
    ///
    /// `capacity` is clamped to [`MAX_SCRATCH_HEADERS`]. A request carrying
    /// more fields than the clamped capacity is refused as
    /// `TooManyHeaders`, and the reported limit is the clamped value, so the
    /// error still describes the limit actually applied.
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.min(MAX_SCRATCH_HEADERS),
        }
    }

    /// Returns the maximum number of fields this storage can parse.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Lends `input` and header storage for one parser operation.
    ///
    /// The higher-ranked callback lifetime prevents its result from retaining
    /// references to either argument. This makes the end of the callback the
    /// explicit boundary after which an adapter may compact or refill `input`.
    ///
    /// The receiver is `&mut self` even though nothing is mutated, so that the
    /// borrow checker still enforces one parse at a time per scratch.
    ///
    /// ```compile_fail
    /// use kimojio_fsm_http::Http1HeaderScratch;
    ///
    /// let mut scratch = Http1HeaderScratch::new(8);
    /// let escaped = scratch.with_input(b"input", |input, _headers| input);
    /// # let _ = escaped;
    /// ```
    pub fn with_input<R>(
        &mut self,
        input: &[u8],
        parse: impl for<'parse> FnOnce(&'parse [u8], &'parse mut [Header<'parse>]) -> R,
    ) -> R {
        // The array is created per call, so its element lifetime is simply
        // inferred at the call site. Retaining one across calls would instead
        // force the storage to be typed `Header<'static>` and cast down to the
        // caller's lifetime, which is not expressible safely: `Vec` narrows by
        // covariance but cannot be widened back to `'static` to be reused, and
        // through `&mut` even the narrowing is invariant.
        let mut storage = [httparse::EMPTY_HEADER; MAX_SCRATCH_HEADERS];
        parse(input, &mut storage[..self.capacity])
    }
}

#[cfg(test)]
mod tests {
    use super::Http1HeaderScratch;
    use crate::{Http1ConnectionDecoder, Http1ConnectionEvent, Http1MessageHead};

    #[test]
    fn scratch_scopes_borrows_across_repeated_parse_and_refill_cycles() {
        let mut decoder = Http1ConnectionDecoder::response(
            "GET",
            crate::HttpLimits::new()
                .set_max_header_bytes(1024)
                .set_max_body_bytes(1024),
        );
        let mut scratch = Http1HeaderScratch::new(4);
        let mut input = b"HTTP/1.1 100 Continue\r\nx-first: one\r\n\r\n".to_vec();

        let informational_consumed = scratch.with_input(&input, |input, headers| {
            let Http1ConnectionEvent::Head {
                head: Http1MessageHead::Response(head),
                consumed,
                informational: true,
                ..
            } = decoder.next_event(input, headers).unwrap()
            else {
                panic!("expected informational response head");
            };
            assert_eq!(head.headers[0].value, b"one");
            consumed
        });
        input.drain(..informational_consumed);

        input.extend_from_slice(b"HTTP/1.1 200 OK\r\ncontent-len");
        scratch.with_input(&input, |input, headers| {
            assert_eq!(
                decoder.next_event(input, headers),
                Ok(Http1ConnectionEvent::NeedInput)
            );
        });
        input.extend_from_slice(b"gth: 5\r\nx-final: two\r\n\r\nhe");

        let head_consumed = scratch.with_input(&input, |input, headers| {
            let Http1ConnectionEvent::Head {
                head: Http1MessageHead::Response(head),
                consumed,
                informational: false,
                ..
            } = decoder.next_event(input, headers).unwrap()
            else {
                panic!("expected final response head");
            };
            assert_eq!(head.headers[1].value, b"two");
            consumed
        });
        input.drain(..head_consumed);

        let body_consumed = scratch.with_input(&input, |input, headers| {
            let Http1ConnectionEvent::Body { chunk, consumed } =
                decoder.next_event(input, headers).unwrap()
            else {
                panic!("expected first body chunk");
            };
            assert_eq!(chunk, b"he");
            consumed
        });
        input.drain(..body_consumed);
        input.extend_from_slice(b"llo");

        let body_consumed = scratch.with_input(&input, |input, headers| {
            let Http1ConnectionEvent::Body { chunk, consumed } =
                decoder.next_event(input, headers).unwrap()
            else {
                panic!("expected refilled body chunk");
            };
            assert_eq!(chunk, b"llo");
            consumed
        });
        input.drain(..body_consumed);
        assert!(input.is_empty());
        scratch.with_input(&input, |input, headers| {
            assert_eq!(
                decoder.next_event(input, headers),
                Ok(Http1ConnectionEvent::Complete)
            );
        });
    }

    #[test]
    fn scratch_storage_does_not_survive_a_parser_panic() {
        let mut scratch = Http1HeaderScratch::new(1);
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            scratch.with_input(b"name: value\r\n", |input, headers| {
                headers[0] = httparse::Header {
                    name: "name",
                    value: input,
                };
                panic!("parser panic");
            });
        }));
        assert!(panic.is_err());

        // Storage is per call, so an unwound call cannot leave a reference to
        // the previous input visible to the next one.
        scratch.with_input(b"", |_, headers| {
            assert_eq!(headers, [httparse::EMPTY_HEADER]);
        });
    }

    #[test]
    fn capacity_is_clamped_to_the_supported_maximum() {
        assert_eq!(Http1HeaderScratch::new(4).capacity(), 4);
        assert_eq!(
            Http1HeaderScratch::new(usize::MAX).capacity(),
            super::MAX_SCRATCH_HEADERS
        );
        // The lent slice must match the reported capacity, so a caller that
        // reports `TooManyHeaders` names the limit actually applied.
        let mut scratch = Http1HeaderScratch::new(usize::MAX);
        let lent = scratch.with_input(b"", |_, headers| headers.len());
        assert_eq!(lent, super::MAX_SCRATCH_HEADERS);
    }
}
