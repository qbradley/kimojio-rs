use std::{cell::Cell, fmt, ops::Deref, rc::Rc};

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
    Trailers(HeaderMap),
}

/// A body polled directly by the connection, without a producer task.
///
/// Each data frame must fit `Config::protocol.max_buffer_bytes`. The source is
/// polled only when the protocol accepts another frame. A trailers frame ends
/// the source; trailers require a streaming (chunked) body.
pub struct OutgoingBody {
    pub(crate) length: BodyLength,
    pub(crate) source: LocalBoxStream<'static, Result<OutgoingFrame, Error>>,
}

impl fmt::Debug for OutgoingBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OutgoingBody")
            .field("length", &self.length)
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
            source: futures::stream::empty().boxed_local(),
        }
    }

    pub fn full(bytes: impl Into<Vec<u8>>) -> Self {
        let bytes = bytes.into();
        if bytes.is_empty() {
            return Self::empty();
        }
        Self {
            length: BodyLength::Known(bytes.len() as u64),
            source: futures::stream::once(async { Ok(OutgoingFrame::Data(bytes)) }).boxed_local(),
        }
    }

    /// `length = None` requests chunked framing; `Some(n)` declares exactly n bytes.
    pub fn from_stream<S>(length: Option<u64>, source: S) -> Self
    where
        S: Stream<Item = Result<OutgoingFrame, Error>> + 'static,
    {
        Self {
            length: length.map_or(BodyLength::Streaming, BodyLength::Known),
            source: source.boxed_local(),
        }
    }
}

/// An exclusive receive-buffer lease. Dropping it releases its entire chunk.
///
/// Holding a chunk stops further body input, but does not stop transport writes.
pub struct BodyChunk {
    pub(crate) op: Option<BodyOp<Vec<u8>>>,
    pub(crate) release: Sender<BodyCompletion<Vec<u8>>>,
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
/// Dropping a client body cancels its unfinished exchange. The server discards
/// further request data that the core permits it to receive.
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
    fn empty_full_and_fallible_sources() {
        futures::executor::block_on(async {
            assert!(OutgoingBody::empty().source.next().await.is_none());
            let mut full = OutgoingBody::full(b"payload");
            assert_eq!(full.length, BodyLength::Known(7));
            let Some(Ok(OutgoingFrame::Data(bytes))) = full.source.next().await else {
                panic!("missing full-body data");
            };
            assert_eq!(bytes, b"payload");
            assert!(full.source.next().await.is_none());
            let mut fallible = OutgoingBody::from_stream(
                None,
                futures::stream::iter([Err(Error::Application("failure".into()))]),
            );
            assert!(matches!(
                fallible.source.next().await,
                Some(Err(Error::Application(_)))
            ));
        });
    }
}
