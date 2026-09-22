use std::{
    cell::Cell,
    fmt,
    ops::Deref,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll},
};

use futures::{Stream, StreamExt, stream::LocalBoxStream};
use http::HeaderMap;
use kimojio::{CancellationToken, Receiver, ReceiverOneshot, Sender, SenderOneshot, oneshot};
use kimojio_fsm_http1::{BodyCompletion, BodyLength, BodyOp, ExchangeId};

use crate::Error;

pub(crate) struct BodyDemand {
    pub exchange: ExchangeId,
    pub accepted: SenderOneshot<Result<(), Error>>,
}

/// One frame from a fallible application body source.
#[derive(Debug)]
pub enum OutgoingFrame {
    Data(Vec<u8>),
    /// Transfers a receive lease without copying its payload.
    ///
    /// After admission, the lease returns with the outgoing body receipt, not
    /// when its source ends or cancellation is requested. Its entire receive
    /// allocation must fit the destination's `Config::protocol.max_buffer_bytes`.
    Forward(BodyChunk),
    Trailers(HeaderMap),
}

#[derive(Debug)]
pub(crate) enum OutgoingData {
    Owned(Vec<u8>),
    Shared(Rc<[u8]>),
    Forward(BodyChunk),
}

impl OutgoingData {
    pub(crate) fn retained_capacity(&self) -> usize {
        match self {
            Self::Owned(bytes) => bytes.capacity(),
            Self::Shared(bytes) => bytes.len(),
            Self::Forward(chunk) => chunk.retained_capacity(),
        }
    }
}

impl AsRef<[u8]> for OutgoingData {
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Owned(bytes) => bytes,
            Self::Shared(bytes) => bytes,
            Self::Forward(chunk) => chunk,
        }
    }
}

/// A body polled directly by the connection, without a producer task.
///
/// Each data frame must fit `Config::protocol.max_buffer_bytes`. The source is
/// polled only when the protocol accepts another frame. A trailers frame ends
/// the source; trailers require a streaming (chunked) body.
pub struct OutgoingBody {
    pub(crate) length: BodyLength,
    pub(crate) source: OutgoingSource,
    pub(crate) continue_request: bool,
}

#[derive(Debug)]
pub(crate) enum SourceFrame {
    Data(OutgoingData),
    Trailers(HeaderMap),
}
impl From<OutgoingFrame> for SourceFrame {
    fn from(frame: OutgoingFrame) -> Self {
        match frame {
            OutgoingFrame::Data(bytes) => Self::Data(OutgoingData::Owned(bytes)),
            OutgoingFrame::Forward(chunk) => Self::Data(OutgoingData::Forward(chunk)),
            OutgoingFrame::Trailers(headers) => Self::Trailers(headers),
        }
    }
}

pub(crate) enum OutgoingSource {
    Ready(Option<Vec<u8>>),
    Shared(Option<Rc<[u8]>>),
    Stream(LocalBoxStream<'static, Result<OutgoingFrame, Error>>),
    SharedStream(LocalBoxStream<'static, Result<Rc<[u8]>, Error>>),
}

impl Stream for OutgoingSource {
    type Item = Result<SourceFrame, Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match &mut *self {
            Self::Ready(bytes) => Poll::Ready(
                bytes
                    .take()
                    .map(|bytes| Ok(SourceFrame::Data(OutgoingData::Owned(bytes)))),
            ),
            Self::Shared(bytes) => Poll::Ready(
                bytes
                    .take()
                    .map(|bytes| Ok(SourceFrame::Data(OutgoingData::Shared(bytes)))),
            ),
            Self::Stream(source) => source
                .as_mut()
                .poll_next(cx)
                .map(|frame| frame.map(|frame| frame.map(Into::into))),
            Self::SharedStream(source) => source.as_mut().poll_next(cx).map(|frame| {
                frame.map(|frame| frame.map(|bytes| SourceFrame::Data(OutgoingData::Shared(bytes))))
            }),
        }
    }
}

impl fmt::Debug for OutgoingBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OutgoingBody")
            .field("length", &self.length)
            .field("continue_request", &self.continue_request)
            .finish_non_exhaustive()
    }
}

impl Default for OutgoingBody {
    fn default() -> Self {
        Self::empty()
    }
}

impl OutgoingBody {
    pub fn empty() -> Self {
        Self {
            length: BodyLength::Empty,
            source: OutgoingSource::Ready(None),
            continue_request: false,
        }
    }

    pub fn full(bytes: impl Into<Vec<u8>>) -> Self {
        let bytes = bytes.into();
        if bytes.is_empty() {
            return Self::empty();
        }
        Self {
            length: BodyLength::Known(bytes.len() as u64),
            source: OutgoingSource::Ready(Some(bytes)),
            continue_request: false,
        }
    }

    /// An immutable, shared full body without a payload copy.
    ///
    /// The complete allocation must fit `Config::protocol.max_buffer_bytes`.
    /// Admission/partial writes retain a strong owner until the body receipt,
    /// including cancellation. Cloning the source bytes does not clone payload.
    pub fn shared(bytes: Rc<[u8]>) -> Self {
        if bytes.is_empty() {
            return Self::empty();
        }
        Self {
            length: BodyLength::Known(bytes.len() as u64),
            source: OutgoingSource::Shared(Some(bytes)),
            continue_request: false,
        }
    }

    /// Demand-driven immutable frames. Each complete allocation must fit the
    /// configured buffer limit; a small view cannot hide a larger retained body.
    /// `None` requests chunked framing; `Some(n)` declares exactly n bytes.
    /// Use `from_stream` for sources that emit trailers or forward receive leases.
    pub fn from_shared_stream<S>(length: Option<u64>, source: S) -> Self
    where
        S: Stream<Item = Result<Rc<[u8]>, Error>> + 'static,
    {
        Self {
            length: length.map_or(BodyLength::Streaming, BodyLength::Known),
            source: OutgoingSource::SharedStream(source.boxed_local()),
            continue_request: false,
        }
    }

    /// `length = None` requests chunked framing; `Some(n)` declares exactly n bytes.
    pub fn from_stream<S>(length: Option<u64>, source: S) -> Self
    where
        S: Stream<Item = Result<OutgoingFrame, Error>> + 'static,
    {
        Self {
            length: length.map_or(BodyLength::Streaming, BodyLength::Known),
            source: OutgoingSource::Stream(source.boxed_local()),
            continue_request: false,
        }
    }

    /// Keeps request input active after this server response starts or finishes.
    ///
    /// The application must consume the request or drop its incoming body to
    /// cancel the exchange. Reuse still requires complete input and settled
    /// transport operations. Client request bodies reject this response-only
    /// policy with `Error::InvalidMetadata`.
    pub fn continue_request_body(mut self) -> Self {
        self.continue_request = true;
        self
    }

    /// Forwards data leases and trailers with streaming framing.
    ///
    /// Both connection drivers must remain polled for cross-connection
    /// forwarding. For a same-connection response, call `IncomingBody::accept`
    /// before returning the response. The core still controls early-response
    /// policy and connection reuse.
    pub fn from_incoming(incoming: IncomingBody) -> Self {
        Self::from_stream(
            None,
            futures::stream::try_unfold(incoming, |mut incoming| async move {
                Ok(match incoming.frame().await? {
                    Some(IncomingFrame::Data(chunk)) => {
                        Some((OutgoingFrame::Forward(chunk), incoming))
                    }
                    Some(IncomingFrame::Trailers(headers)) => {
                        Some((OutgoingFrame::Trailers(headers), incoming))
                    }
                    None => None,
                })
            }),
        )
    }
}

/// An exclusive receive-buffer lease. Dropping it releases its entire chunk.
///
/// Holding a chunk stops further body input, but does not stop transport writes.
pub struct BodyChunk {
    pub(crate) op: Option<BodyOp<Vec<u8>>>,
    pub(crate) release: Sender<BodyCompletion<Vec<u8>>>,
    pub(crate) retained_capacity: usize,
}

impl BodyChunk {
    /// Capacity of the complete receive allocation retained by this lease.
    ///
    /// This can exceed the visible payload length. Forwarding checks this
    /// capacity against the destination's buffer limit without copying.
    pub fn retained_capacity(&self) -> usize {
        self.retained_capacity
    }
}

impl Deref for BodyChunk {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.op.as_ref().expect("live body chunk").bytes()
    }
}

impl AsRef<[u8]> for BodyChunk {
    fn as_ref(&self) -> &[u8] {
        self
    }
}

impl fmt::Debug for BodyChunk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BodyChunk")
            .field("len", &self.len())
            .field("retained_capacity", &self.retained_capacity)
            .finish()
    }
}

impl Drop for BodyChunk {
    fn drop(&mut self) {
        if let Some(op) = self.op.take() {
            let len = op.bytes().len();
            // Exactly one receive lease exists. Its return slot cannot be full.
            let _ = self.release.try_send(op.release(len));
        }
    }
}

#[derive(Debug)]
pub enum IncomingFrame {
    Data(BodyChunk),
    Trailers(HeaderMap),
}

/// A bounded response or request body.
///
/// Dropping a client body cancels its unfinished exchange. The server normally
/// discards further permitted request data. With `continue_request_body`, it
/// cancels incomplete input after outstanding leases return and core
/// completion notifications drain.
pub struct IncomingBody {
    pub(crate) data: Receiver<BodyChunk>,
    pub(crate) terminal: Option<ReceiverOneshot<Result<HeaderMap, Error>>>,
    pub(crate) finished: Rc<Cell<bool>>,
    pub(crate) cancel: Rc<CancellationToken>,
    demand: Sender<BodyDemand>,
    exchange: ExchangeId,
    requested: bool,
    ended: bool,
}

impl fmt::Debug for IncomingBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IncomingBody")
            .field("finished", &self.finished.get())
            .finish_non_exhaustive()
    }
}

impl IncomingBody {
    pub(crate) fn new(
        data: Receiver<BodyChunk>,
        terminal: ReceiverOneshot<Result<HeaderMap, Error>>,
        finished: Rc<Cell<bool>>,
        cancel: Rc<CancellationToken>,
        demand: Sender<BodyDemand>,
        exchange: ExchangeId,
    ) -> Self {
        Self {
            data,
            terminal: Some(terminal),
            finished,
            cancel,
            demand,
            exchange,
            requested: false,
            ended: false,
        }
    }

    /// Enables input delivery without waiting for payload.
    ///
    /// This acknowledges delivery credit, not successful body validation. A
    /// server can call it before returning a response that streams request data.
    pub async fn accept(&mut self) -> Result<(), Error> {
        if !self.requested && !self.finished.get() {
            let (accepted, receive) = oneshot();
            self.demand
                .send(BodyDemand {
                    exchange: self.exchange,
                    accepted,
                })
                .await
                .map_err(|_| Error::Closed)?;
            receive.recv().await.map_err(|_| Error::Closed)??;
            self.requested = true;
        }
        Ok(())
    }

    pub async fn frame(&mut self) -> Result<Option<IncomingFrame>, Error> {
        if self.ended {
            return Ok(None);
        }
        if let Err(error) = self.accept().await
            && !self.finished.get()
        {
            return Err(error);
        }
        if let Ok(chunk) = self.data.recv().await {
            return Ok(Some(IncomingFrame::Data(chunk)));
        }
        let result = match self.terminal.as_ref() {
            Some(terminal) => terminal
                .try_recv()
                .ok()
                .flatten()
                .unwrap_or(Err(Error::Closed)),
            None => Ok(HeaderMap::new()),
        };
        self.terminal.take();
        self.ended = true;
        let trailers = result?;
        if trailers.is_empty() {
            Ok(None)
        } else {
            Ok(Some(IncomingFrame::Trailers(trailers)))
        }
    }

    /// Collects up to `limit` bytes. Use `frame` when trailers are needed.
    pub async fn collect(&mut self, limit: usize) -> Result<Vec<u8>, Error> {
        let mut bytes = Vec::new();
        while let Some(frame) = self.frame().await? {
            if let IncomingFrame::Data(chunk) = frame {
                if chunk.len() > limit.saturating_sub(bytes.len()) {
                    self.cancel.cancel();
                    return Err(Error::Limit);
                }
                bytes.extend_from_slice(&chunk);
            }
        }
        Ok(bytes)
    }
}

impl Drop for IncomingBody {
    fn drop(&mut self) {
        if !self.finished.get() {
            self.cancel.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_source_keeps_payload_after_source_drop_and_does_not_inflate_ready_storage() {
        #[allow(dead_code)]
        enum OriginalSource {
            Ready(Option<Vec<u8>>),
            Stream(LocalBoxStream<'static, Result<OutgoingFrame, Error>>),
        }
        assert_eq!(size_of::<OutgoingSource>(), size_of::<OriginalSource>());
        futures::executor::block_on(async {
            let bytes: Rc<[u8]> = Rc::from(b"abc".as_slice());
            let mut body = OutgoingBody::shared(bytes.clone());
            let SourceFrame::Data(data) = body.source.next().await.unwrap().unwrap() else {
                panic!()
            };
            assert_eq!(data.as_ref().as_ptr(), bytes.as_ptr());
            assert_eq!(data.retained_capacity(), 3);
            assert!(body.source.next().await.is_none());
            drop(body);
            assert_eq!(Rc::strong_count(&bytes), 2);
            drop(data);
            assert_eq!(Rc::strong_count(&bytes), 1);
            let calls = Rc::new(Cell::new(0));
            let n = calls.clone();
            let payload = bytes.clone();
            let mut body = OutgoingBody::from_shared_stream(
                None,
                futures::stream::poll_fn(move |_| {
                    n.set(n.get() + 1);
                    Poll::Ready((n.get() == 1).then(|| Ok(payload.clone())))
                }),
            );
            assert_eq!(calls.get(), 0);
            let frame = body.source.next().await.unwrap().unwrap();
            assert_eq!(calls.get(), 1);
            assert!(body.source.next().await.is_none());
            drop(body);
            let SourceFrame::Data(data) = frame else {
                panic!()
            };
            assert_eq!(data.as_ref().as_ptr(), bytes.as_ptr());
            assert_eq!(Rc::strong_count(&bytes), 2);
            drop(data);
            assert_eq!(Rc::strong_count(&bytes), 1);
        });
    }

    #[test]
    fn empty_full_and_fallible_sources() {
        futures::executor::block_on(async {
            assert!(OutgoingBody::empty().source.next().await.is_none());
            let mut full = OutgoingBody::full(b"payload");
            assert!(matches!(full.source, OutgoingSource::Ready(Some(_))));
            assert_eq!(full.length, BodyLength::Known(7));
            let Some(Ok(SourceFrame::Data(OutgoingData::Owned(bytes)))) = full.source.next().await
            else {
                panic!("missing full-body data");
            };
            assert_eq!(bytes, b"payload");
            assert!(full.source.next().await.is_none());
            let mut fallible = OutgoingBody::from_stream(
                None,
                futures::stream::iter([Err(Error::Application("failure".into()))]),
            );
            assert!(matches!(fallible.source, OutgoingSource::Stream(_)));
            assert!(matches!(
                fallible.source.next().await,
                Some(Err(Error::Application(_)))
            ));
        });
    }
}
