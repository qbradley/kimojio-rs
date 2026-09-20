use std::{cell::Cell, fmt, ops::Deref, rc::Rc};

use futures::{Stream, StreamExt, stream::LocalBoxStream};
use http::HeaderMap;
use kimojio::{ReceiverOneshot, ReceiverUnbounded, SenderUnbounded};
use kimojio_fsm_http2::{self as core, SendBuffer};

use crate::{
    Error, InformationalSender, StreamReport,
    driver::{Event, RequestControl},
};

/// A producer frame. A trailer section ends the upload.
#[derive(Debug)]
pub enum OutgoingFrame {
    Data(Vec<u8>),
    Static(&'static [u8]),
    Forward(BodyChunk),
    Trailers(HeaderMap),
}

#[derive(Debug)]
pub(crate) enum Data {
    Owned(Vec<u8>),
    Static(&'static [u8]),
    Forward(BodyChunk),
}

impl AsRef<[u8]> for Data {
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Owned(bytes) => bytes,
            Self::Static(bytes) => bytes,
            Self::Forward(chunk) => chunk,
        }
    }
}

impl SendBuffer for Data {
    fn retained_capacity(&self) -> usize {
        match self {
            Self::Owned(bytes) => bytes.capacity(),
            Self::Static(_) => 0,
            Self::Forward(chunk) => chunk.retained_capacity(),
        }
    }
}

impl Data {
    pub(crate) fn check(&self, bytes: usize, capacity: usize) -> Result<(), Error> {
        if self.as_ref().len() > bytes || self.retained_capacity() > capacity {
            Err(Error::BufferTooLarge {
                bytes: self.as_ref().len(),
                capacity: self.retained_capacity(),
                max_bytes: bytes,
                max_capacity: capacity,
            })
        } else {
            Ok(())
        }
    }
}

/// Ready bodies allocate no producer task. Stream bodies use one scoped task.
pub struct OutgoingBody {
    pub(crate) source: Source,
}

pub(crate) enum Source {
    Empty,
    Full(Data),
    Stream(LocalBoxStream<'static, Result<OutgoingFrame, Error>>),
}

impl Default for OutgoingBody {
    fn default() -> Self {
        Self::empty()
    }
}

impl OutgoingBody {
    pub fn empty() -> Self {
        Self {
            source: Source::Empty,
        }
    }

    /// Retains the original allocation. It must fit one configured send buffer.
    pub fn full(bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            source: Source::Full(Data::Owned(bytes.into())),
        }
    }

    pub fn from_static(bytes: &'static [u8]) -> Self {
        Self {
            source: Source::Full(Data::Static(bytes)),
        }
    }

    /// Each data frame must fit both limits of its core send permit.
    ///
    /// Empty nonterminal frames are skipped, not interpreted as producer EOF.
    /// Content length, if desired, belongs in the request's headers.
    pub fn from_stream<S>(source: S) -> Self
    where
        S: Stream<Item = Result<OutgoingFrame, Error>> + 'static,
    {
        Self {
            source: Source::Stream(source.boxed_local()),
        }
    }

    /// Forwards read-only leases without copying their payload.
    pub fn from_incoming(body: IncomingBody) -> Self {
        Self::from_stream(futures::stream::try_unfold(body, |mut body| async move {
            let Some(frame) = body.frame().await? else {
                return Ok(None);
            };
            let frame = match frame {
                IncomingFrame::Data(chunk) => OutgoingFrame::Forward(chunk),
                IncomingFrame::Trailers(headers) => {
                    // Trailer delivery can precede the authoritative receive outcome.
                    if body.frame().await?.is_some() {
                        return Err(Error::InvalidMetadata);
                    }
                    OutgoingFrame::Trailers(headers)
                }
            };
            Ok(Some((frame, body)))
        }))
    }

    pub(crate) fn is_empty(&self) -> bool {
        matches!(&self.source, Source::Empty)
            || matches!(&self.source, Source::Full(data) if data.as_ref().is_empty())
    }

    pub(crate) fn retained_capacity(&self) -> usize {
        match &self.source {
            Source::Full(data) => data.retained_capacity(),
            _ => 0,
        }
    }
}

/// A read-only core page lease. It can outlive transport closure.
pub struct BodyChunk {
    pub(crate) op: Option<core::BodyOp>,
    pub(crate) events: SenderUnbounded<Event>,
}

impl BodyChunk {
    pub fn retained_capacity(&self) -> usize {
        self.op.as_ref().expect("live lease").retained_capacity()
    }
}

impl Deref for BodyChunk {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.op.as_ref().expect("live lease").bytes()
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
            .field("retained_capacity", &self.retained_capacity())
            .finish()
    }
}

impl Drop for BodyChunk {
    fn drop(&mut self) {
        if let Some(op) = self.op.take() {
            let _ = self.events.send(Event::Release(op.release()));
        }
    }
}

#[derive(Debug)]
pub enum IncomingFrame {
    Data(BodyChunk),
    Trailers(HeaderMap),
}

pub(crate) enum Delivery {
    Frame(IncomingFrame),
    End(core::StreamOutcome),
}

/// Receive EOF and full stream retirement are separate observations.
pub struct IncomingBody {
    pub(crate) stream: core::StreamId,
    pub(crate) frames: Rc<ReceiverUnbounded<Delivery>>,
    pub(crate) close: SenderUnbounded<Delivery>,
    pub(crate) events: SenderUnbounded<Event>,
    pub(crate) received: Rc<Cell<Option<core::StreamOutcome>>>,
    pub(crate) completion: Option<ReceiverOneshot<Result<StreamReport, Error>>>,
    pub(crate) completion_wait:
        Option<futures::future::LocalBoxFuture<'static, Result<StreamReport, Error>>>,
    pub(crate) completed: Option<Result<StreamReport, Error>>,
    pub(crate) eof: bool,
    pub(crate) control: Rc<RequestControl>,
    pub(crate) abandon_on_drop: bool,
}

impl fmt::Debug for IncomingBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IncomingBody")
            .field("stream", &self.stream)
            .field("receive_end", &self.received.get())
            .finish_non_exhaustive()
    }
}

impl IncomingBody {
    /// Creates optional informational-response control for a server request.
    ///
    /// Client response bodies return `None`. Ordinary requests do not allocate
    /// this control unless the handler asks for it.
    pub fn informational_sender(&self) -> Option<InformationalSender> {
        if self.abandon_on_drop {
            None
        } else {
            Some(InformationalSender::new(
                self.stream,
                self.control.clone(),
                self.events.clone(),
            ))
        }
    }

    pub fn stream_id(&self) -> core::StreamId {
        self.stream
    }

    /// The core's receive-half outcome, independent of upload completion.
    pub fn receive_outcome(&self) -> Option<core::StreamOutcome> {
        self.received.get()
    }

    /// Explicitly cancels this stream, including an upload after receive EOF.
    pub fn cancel(&self) {
        if !self.control.retired.get() && !self.control.cancelled.replace(true) {
            let _ = self.events.send(Event::Cancel(self.control.clone()));
        }
    }

    pub async fn frame(&mut self) -> Result<Option<IncomingFrame>, Error> {
        if self.eof {
            return match self.received.get() {
                Some(core::StreamOutcome::Complete) => Ok(None),
                Some(outcome) => Err(Error::Stream(outcome)),
                None => Err(Error::Closed),
            };
        }
        match self.frames.recv().await.map_err(|_| Error::Closed)? {
            Delivery::Frame(frame) => Ok(Some(frame)),
            Delivery::End(outcome) => {
                self.eof = true;
                match outcome {
                    core::StreamOutcome::Complete => Ok(None),
                    other => Err(Error::Stream(other)),
                }
            }
        }
    }

    /// Waits for both stream halves and all body leases to settle.
    ///
    /// Consume the receive body and drop its chunks before awaiting this.
    /// Contextual failures take precedence here. Use `retirement` to inspect the
    /// actual core outcome separately from a failed DATA-buffer receipt.
    pub async fn completion(&mut self) -> Result<core::StreamOutcome, Error> {
        self.wait_retirement().await?.completion_result()
    }

    /// Returns the actual core retirement independently of contextual errors.
    ///
    /// A reset is an `Ok(StreamReport)` with a reset outcome, not a send receipt
    /// inferred as retirement. `Error::Closed` means no retirement was delivered.
    /// Consume the body and release its chunks before awaiting this method.
    pub async fn retirement(&mut self) -> Result<StreamReport, Error> {
        self.wait_retirement().await.cloned()
    }

    async fn wait_retirement(&mut self) -> Result<&StreamReport, Error> {
        if self.completed.is_none() {
            if self.completion_wait.is_none() {
                let receive = self.completion.take().ok_or(Error::Closed)?;
                self.completion_wait = Some(Box::pin(async move {
                    receive.recv().await.map_err(|_| Error::Closed)?
                }));
            }
            let result = self
                .completion_wait
                .as_mut()
                .expect("completion waiter")
                .await;
            self.completion_wait.take();
            self.completed = Some(result);
        }
        self.completed
            .as_ref()
            .expect("retirement result")
            .as_ref()
            .map_err(Clone::clone)
    }

    pub async fn collect(&mut self, limit: usize) -> Result<Vec<u8>, Error> {
        let mut bytes = Vec::new();
        while let Some(frame) = self.frame().await? {
            if let IncomingFrame::Data(chunk) = frame {
                if chunk.len() > limit.saturating_sub(bytes.len()) {
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
        // Native channel close does not drain its retained messages.
        self.close.close();
        while let Ok(Some(delivery)) = self.frames.try_recv() {
            drop(delivery);
        }
        if self.abandon_on_drop
            && self.received.get().is_none()
            && !self.control.cancelled.get()
            && !self.control.retired.get()
        {
            let _ = self.events.send(Event::Abandon(self.stream));
        }
    }
}
