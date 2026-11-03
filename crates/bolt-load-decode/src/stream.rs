use std::{
    fmt, io,
    pin::Pin,
    task::{Context, Poll, ready},
};

use bytes::{Buf, Bytes, BytesMut};
use compression_codecs::core::util::{PartialBuffer, WriteBuffer};
use futures::Stream;
use pin_project_lite::pin_project;

use crate::{DecodeError, DecodeOptions, codec::Codec};

pin_project! {
    /// The decoded body, from [`decode`](crate::decode).
    ///
    /// Each item is at most [`DecodeOptions::buffer_size`] bytes. The stream
    /// ends after the first error.
    pub struct Decoded<S> {
        #[pin]
        body: S,
        // The coding applied last comes first.
        layers: Vec<Layer>,
        max_output: Option<u64>,
        emitted: u64,
        ended: bool,
    }
}

impl<S> Decoded<S> {
    pub(crate) fn new(body: S, codecs: Vec<Codec>, options: &DecodeOptions) -> Self {
        let buffer_size = options.buffer_size.max(1);
        Self {
            body,
            layers: codecs
                .into_iter()
                .map(|codec| Layer::new(codec, buffer_size))
                .collect(),
            max_output: options.max_output,
            emitted: 0,
            ended: false,
        }
    }
}

impl<S> fmt::Debug for Decoded<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Decoded")
            .field("layers", &self.layers)
            .field("max_output", &self.max_output)
            .field("emitted", &self.emitted)
            .field("ended", &self.ended)
            .finish_non_exhaustive()
    }
}

impl<S, E> Stream for Decoded<S>
where
    S: Stream<Item = Result<Bytes, E>>,
    E: From<DecodeError>,
{
    type Item = Result<Bytes, E>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();
        if *this.ended {
            return Poll::Ready(None);
        }
        let item = ready!(poll_layers(this.layers, this.body, cx));
        let item = match (item, *this.max_output) {
            (Some(Ok(chunk)), Some(limit)) if *this.emitted + chunk.len() as u64 > limit => {
                Some(Err(DecodeError::TooLarge { limit }.into()))
            }
            (item, _) => item,
        };
        match &item {
            Some(Ok(chunk)) => *this.emitted += chunk.len() as u64,
            Some(Err(_)) | None => *this.ended = true,
        }
        Poll::Ready(item)
    }
}

/// Poll the last layer, which reads from the layers before it, and the first
/// from the body.
fn poll_layers<S, E>(
    layers: &mut [Layer],
    mut body: Pin<&mut S>,
    cx: &mut Context<'_>,
) -> Poll<Option<Result<Bytes, E>>>
where
    S: Stream<Item = Result<Bytes, E>>,
    E: From<DecodeError>,
{
    match layers.split_last_mut() {
        Some((last, rest)) => last.poll_next(cx, |cx| poll_layers(rest, body.as_mut(), cx)),
        None => body.poll_next(cx),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Inside a member.
    Decoding,
    /// The input ended inside a member.
    Finishing,
    /// A member ended; the input may hold another one.
    MemberEnded,
    Done,
}

/// One coding: reads coded chunks and writes decoded ones.
#[derive(Debug)]
struct Layer {
    codec: Codec,
    state: State,
    /// The unread part of the last input chunk.
    input: Bytes,
    /// Whether any input arrived. An empty body is an empty file.
    received: bool,
    /// `filled` decoded bytes, then scratch space up to `buffer_size`.
    output: BytesMut,
    filled: usize,
    buffer_size: usize,
    /// The output filled up, so the codec may hold more without new input.
    flush: bool,
}

impl Layer {
    fn new(codec: Codec, buffer_size: usize) -> Self {
        Self {
            codec,
            state: State::Decoding,
            input: Bytes::new(),
            received: false,
            output: BytesMut::new(),
            filled: 0,
            buffer_size,
            flush: false,
        }
    }

    fn poll_next<E: From<DecodeError>>(
        &mut self,
        cx: &mut Context<'_>,
        mut source: impl FnMut(&mut Context<'_>) -> Poll<Option<Result<Bytes, E>>>,
    ) -> Poll<Option<Result<Bytes, E>>> {
        loop {
            if self.filled == self.buffer_size {
                return Poll::Ready(Some(Ok(self.take())));
            }
            let step = match self.state {
                State::Done => return Poll::Ready((self.filled > 0).then(|| Ok(self.take()))),
                State::Decoding | State::MemberEnded if self.input.is_empty() && !self.flush => {
                    match source(cx) {
                        Poll::Pending if self.filled > 0 => {
                            return Poll::Ready(Some(Ok(self.take())));
                        }
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(Some(Ok(chunk))) => {
                            self.received |= !chunk.is_empty();
                            self.input = chunk;
                        }
                        Poll::Ready(Some(Err(err))) => {
                            self.fail();
                            return Poll::Ready(Some(Err(err)));
                        }
                        Poll::Ready(None) => {
                            self.state = match self.state {
                                State::Decoding if self.received => State::Finishing,
                                _ => State::Done,
                            };
                        }
                    }
                    continue;
                }
                State::Decoding => self.decode(),
                State::Finishing => self.finish(),
                State::MemberEnded => self.next_member(),
            };
            if let Err(err) = step {
                self.fail();
                return Poll::Ready(Some(Err(err.into())));
            }
        }
    }

    fn decode(&mut self) -> Result<(), DecodeError> {
        self.output.resize(self.buffer_size, 0);
        let mut output = WriteBuffer::new_initialized(&mut self.output[self.filled..]);
        let mut input = PartialBuffer::new(&self.input[..]);
        let ended = if self.input.is_empty() {
            self.codec.flush(&mut output)?;
            false
        } else {
            self.codec.decode(&mut input, &mut output)?
        };
        let (read, written) = (input.written_len(), output.written_len());
        if !ended && !self.input.is_empty() && read == 0 && written == 0 {
            return Err(io::Error::other("the decoder made no progress").into());
        }
        self.input.advance(read);
        self.filled += written;
        self.flush = !ended && self.filled == self.buffer_size;
        if ended {
            self.state = State::MemberEnded;
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<(), DecodeError> {
        self.output.resize(self.buffer_size, 0);
        let mut output = WriteBuffer::new_initialized(&mut self.output[self.filled..]);
        // No input is left, so any failure means the body ends inside its
        // coding.
        let ended = self
            .codec
            .finish(&mut output)
            .map_err(|err| match err.kind() {
                io::ErrorKind::UnexpectedEof => err,
                _ => io::Error::new(io::ErrorKind::UnexpectedEof, err),
            })?;
        let written = output.written_len();
        self.filled += written;
        if ended {
            self.state = State::Done;
        } else if written == 0 && self.filled < self.buffer_size {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into());
        }
        Ok(())
    }

    fn next_member(&mut self) -> Result<(), DecodeError> {
        if !self.codec.starts_member(self.input[0]) {
            return Err(DecodeError::TrailingData);
        }
        self.codec.reinit()?;
        self.state = State::Decoding;
        Ok(())
    }

    fn take(&mut self) -> Bytes {
        self.output.truncate(self.filled);
        self.filled = 0;
        // The next `resize` reuses the allocation if the chunk is dropped by
        // then.
        self.output.split().freeze()
    }

    fn fail(&mut self) {
        self.state = State::Done;
        self.input.clear();
        self.filled = 0;
    }
}
