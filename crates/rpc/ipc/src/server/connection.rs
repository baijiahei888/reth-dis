//! A IPC connection.

use crate::stream_codec::StreamCodec;
use futures::{ready, stream::FuturesUnordered, Sink, Stream, StreamExt};
use std::{
    collections::VecDeque,
    future::Future,
    io,
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio_util::codec::Framed;
use tower::Service;

pub(crate) type JsonRpcStream<T> = Framed<T, StreamCodec>;

/// Wraps a stream of incoming connections.
#[pin_project::pin_project]
pub(crate) struct Incoming<T, Item> {
    #[pin]
    inner: T,
    _marker: PhantomData<Item>,
}
impl<T, Item> Incoming<T, Item>
where
    T: Stream<Item = io::Result<Item>> + Unpin + 'static,
    Item: AsyncRead + AsyncWrite,
{
    /// Create a new instance.
    pub(crate) fn new(inner: T) -> Self {
        Self { inner, _marker: Default::default() }
    }

    /// Polls to accept a new incoming connection to the endpoint.
    pub(crate) fn poll_accept(&mut self, cx: &mut Context<'_>) -> Poll<<Self as Stream>::Item> {
        let res = match ready!(self.poll_next_unpin(cx)) {
            None => Err(io::Error::new(io::ErrorKind::ConnectionAborted, "ipc connection closed")),
            Some(conn) => conn,
        };
        Poll::Ready(res)
    }
}

impl<T, Item> Stream for Incoming<T, Item>
where
    T: Stream<Item = io::Result<Item>> + 'static,
    Item: AsyncRead + AsyncWrite,
{
    type Item = io::Result<IpcConn<JsonRpcStream<Item>>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();
        let res = match ready!(this.inner.poll_next(cx)) {
            Some(Ok(item)) => {
                let framed = IpcConn(tokio_util::codec::Decoder::framed(
                    StreamCodec::stream_incoming(),
                    item,
                ));
                Ok(framed)
            }
            Some(Err(err)) => Err(err),
            None => return Poll::Ready(None),
        };
        Poll::Ready(Some(res))
    }
}

#[pin_project::pin_project]
pub(crate) struct IpcConn<T>(#[pin] T);

impl<T> IpcConn<JsonRpcStream<T>>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    /// Create a response for when the server is busy and can't accept more requests.
    pub(crate) async fn reject_connection(self) {
        let mut parts = self.0.into_parts();
        let _ = parts.io.write_all(b"Too many connections. Please try again later.").await;
    }
}

impl<T> Stream for IpcConn<JsonRpcStream<T>>
where
    T: AsyncRead + AsyncWrite,
{
    type Item = io::Result<String>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.project().0.poll_next(cx)
    }
}

impl<T> Sink<String> for IpcConn<JsonRpcStream<T>>
where
    T: AsyncRead + AsyncWrite,
{
    type Error = io::Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // NOTE: we always flush here this prevents buffering in the underlying
        // `Framed` impl that would cause stalled requests
        self.project().0.poll_flush(cx)
    }

    fn start_send(self: Pin<&mut Self>, item: String) -> Result<(), Self::Error> {
        self.project().0.start_send(item)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.project().0.poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.project().0.poll_close(cx)
    }
}

/// Drives an [IpcConn] forward.
///
/// This forwards received requests from the connection to the service and sends responses to the
/// connection.
///
/// This future terminates when the connection is closed.
#[pin_project::pin_project]
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub(crate) struct IpcConnDriver<T, S, Fut> {
    #[pin]
    pub(crate) conn: IpcConn<JsonRpcStream<T>>,
    pub(crate) service: S,
    #[pin]
    pub(crate) pending_calls: FuturesUnordered<Fut>,
    pub(crate) items: VecDeque<String>,
}

impl<T, S, Fut> IpcConnDriver<T, S, Fut> {
    /// Add a new item to the send queue.
    pub(crate) fn push_back(&mut self, item: String) {
        self.items.push_back(item);
    }
}

impl<T, S> Future for IpcConnDriver<T, S, S::Future>
where
    S: Service<String, Response = Option<String>> + Send + 'static,
    S::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    S::Future: Send,
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();

        loop {
            // process calls
            if !this.pending_calls.is_empty() {
                while let Poll::Ready(Some(res)) = this.pending_calls.as_mut().poll_next(cx) {
                    let item = match res {
                        Ok(Some(resp)) => resp,
                        Ok(None) => continue,
                        Err(err) => err.into().to_string(),
                    };
                    this.items.push_back(item);
                }
            }

            // write to the sink
            while this.conn.as_mut().poll_ready(cx).is_ready() {
                if let Some(item) = this.items.pop_front() {
                    if let Err(err) = this.conn.as_mut().start_send(item) {
                        tracing::warn!("IPC response failed: {:?}", err);
                        return Poll::Ready(())
                    }
                } else {
                    break
                }
            }

            // read from the stream
            match ready!(this.conn.as_mut().poll_next(cx)) {
                Some(Ok(item)) => {
                    let call = this.service.call(item);
                    this.pending_calls.push(call);
                }
                Some(Err(err)) => {
                    tracing::warn!("IPC request failed: {:?}", err);
                    return Poll::Ready(())
                }
                None => return Poll::Ready(()),
            }
        }
    }
}
