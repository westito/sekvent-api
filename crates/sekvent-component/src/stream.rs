//! [`CallStream`]: what a `#[call(stream)]` method answers with and what its
//! handle returns.

use std::fmt;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures::Stream;
use futures::stream::FusedStream;
use tokio::sync::mpsc;

/// The stream a `#[call(stream)]` method answers with, and what its handle
/// returns: items until the producer ends, or until the first error, which
/// ends it. Dropping it ends the call under every binding.
///
/// After it has yielded an `Err` or `None` it yields `None` forever and has
/// already dropped the stream it wrapped, releasing whatever that held.
#[must_use = "a stream does nothing unless polled, and dropping it ends the call"]
pub struct CallStream<T, E> {
    inner: Option<Producer<T, E>>,
}

/// The wrapped stream.
type Producer<T, E> = Pin<Box<dyn Stream<Item = Result<T, E>> + Send>>;

impl<T: Send + 'static, E: Send + 'static> CallStream<T, E> {
    /// Wrap any sendable stream.
    pub fn new<S>(stream: S) -> Self
    where
        S: Stream<Item = Result<T, E>> + Send + 'static,
    {
        Self {
            inner: Some(Box::pin(stream)),
        }
    }

    /// Items sent on `receiver`; the stream ends when every sender is gone.
    pub fn from_receiver(mut receiver: mpsc::Receiver<Result<T, E>>) -> Self {
        Self::new(futures::stream::poll_fn(move |cx| receiver.poll_recv(cx)))
    }

    /// `items`, then the end.
    pub fn iter<I>(items: I) -> Self
    where
        I: IntoIterator<Item = Result<T, E>>,
        I::IntoIter: Send + 'static,
    {
        Self::new(futures::stream::iter(items))
    }

    /// A stream that ends without an item.
    pub fn empty() -> Self {
        Self { inner: None }
    }
}

impl<T, E> CallStream<T, E> {
    /// The next item; `None` once the stream has ended.
    pub async fn next(&mut self) -> Option<Result<T, E>> {
        std::future::poll_fn(|cx| Pin::new(&mut *self).poll_next(cx)).await
    }
}

impl<T, E> Stream for CallStream<T, E> {
    type Item = Result<T, E>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let Some(inner) = this.inner.as_mut() else {
            return Poll::Ready(None);
        };
        match inner.as_mut().poll_next(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Ok(item))) => Poll::Ready(Some(Ok(item))),
            Poll::Ready(end) => {
                // `None` or the error that ends the stream: release the
                // producer now, not when the consumer drops this.
                this.inner = None;
                Poll::Ready(end)
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match &self.inner {
            // An error may end the stream before the producer's lower bound.
            Some(inner) => (0, inner.size_hint().1),
            None => (0, Some(0)),
        }
    }
}

impl<T, E> FusedStream for CallStream<T, E> {
    fn is_terminated(&self) -> bool {
        self.inner.is_none()
    }
}

impl<T, E> fmt::Debug for CallStream<T, E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CallStream")
            .field("terminated", &self.inner.is_none())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use futures::StreamExt as _;

    use super::*;

    /// Sets its flag when dropped.
    struct DropFlag(Arc<AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    /// A producer that yields `items` forever after, holding a drop flag.
    fn producer(
        items: Vec<Result<u32, &'static str>>,
    ) -> (CallStream<u32, &'static str>, Arc<AtomicBool>) {
        let dropped = Arc::new(AtomicBool::new(false));
        let flag = DropFlag(Arc::clone(&dropped));
        let stream = futures::stream::iter(items)
            .chain(futures::stream::repeat(Ok(99)))
            .map(move |item| {
                let _keep = &flag;
                item
            });
        (CallStream::new(stream), dropped)
    }

    #[tokio::test]
    async fn it_ends_at_the_first_error_and_drops_the_producer_then() {
        let (mut stream, dropped) = producer(vec![Ok(1), Err("broken"), Ok(2)]);
        assert!(!stream.is_terminated());
        assert_eq!(stream.size_hint(), (0, None));
        assert_eq!(stream.next().await, Some(Ok(1)));
        assert!(!dropped.load(Ordering::SeqCst));
        assert_eq!(stream.next().await, Some(Err("broken")));
        assert!(dropped.load(Ordering::SeqCst), "dropped at the error");
        assert!(stream.is_terminated());
        assert_eq!(stream.next().await, None);
        assert_eq!(stream.next().await, None);
        assert_eq!(stream.size_hint(), (0, Some(0)));
        assert_eq!(format!("{stream:?}"), "CallStream { terminated: true }");
    }

    #[tokio::test]
    async fn it_ends_at_none_and_drops_the_producer_then() {
        let dropped = Arc::new(AtomicBool::new(false));
        let flag = DropFlag(Arc::clone(&dropped));
        let mut stream =
            CallStream::<u32, ()>::new(futures::stream::iter([Ok(1)]).map(move |item| {
                let _keep = &flag;
                item
            }));
        assert_eq!(format!("{stream:?}"), "CallStream { terminated: false }");
        assert_eq!(stream.size_hint(), (0, Some(1)));
        assert_eq!(stream.next().await, Some(Ok(1)));
        assert!(!dropped.load(Ordering::SeqCst));
        assert_eq!(stream.next().await, None);
        assert!(dropped.load(Ordering::SeqCst), "dropped at the end");
        assert!(stream.is_terminated());
        assert_eq!(stream.next().await, None);
    }

    #[tokio::test]
    async fn constructors() {
        let items: Vec<_> = CallStream::<u32, ()>::iter([Ok(1), Ok(2)]).collect().await;
        assert_eq!(items, [Ok(1), Ok(2)]);

        let mut empty = CallStream::<u32, ()>::empty();
        assert!(empty.is_terminated());
        assert_eq!(empty.next().await, None);

        let (sender, receiver) = mpsc::channel(4);
        let mut fed = CallStream::<u32, ()>::from_receiver(receiver);
        sender.send(Ok(7)).await.unwrap();
        assert_eq!(fed.next().await, Some(Ok(7)));
        let pending = tokio::time::timeout(std::time::Duration::from_millis(1), fed.next());
        drop(sender);
        assert_eq!(pending.await, Ok(None), "every sender gone ends it");
        assert!(fed.is_terminated());

        let (sender, receiver) = mpsc::channel(4);
        let mut failed = CallStream::<u32, &str>::from_receiver(receiver);
        sender.send(Err("stop")).await.unwrap();
        sender.send(Ok(1)).await.unwrap();
        assert_eq!(failed.next().await, Some(Err("stop")));
        assert!(sender.is_closed(), "the receiver is dropped at the error");
        assert_eq!(failed.next().await, None);
    }

    #[test]
    fn it_is_send() {
        fn send<T: Send>(_: &T) {}
        send(&CallStream::<u32, ()>::empty());
    }
}
