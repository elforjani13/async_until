#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(feature = "std", feature(maybe_uninit_slice))]

extern crate alloc;

use alloc::{sync::Arc, task::Wake};
use core::{
    future::Future,
    pin::Pin,
    task::{Context, Poll, Waker},
};
use pin_project_lite::pin_project;

pub trait Park {
    fn park_thread();
}

#[cfg(feature = "std")]
mod thread_ext {
    use std::thread::Thread;

    use super::*;

    pub struct TWaker(pub Thread);

    impl TWaker {
        pub fn current() -> Self {
            TWaker(std::thread::current())
        }
    }

    impl Wake for TWaker {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }

    pub struct TParker;

    impl Park for TParker {
        #[inline]
        fn park_thread() {
            std::thread::park();
        }
    }
}

pin_project! {
    #[derive(Debug, Clone)]
    pub struct PinnedFuture<F> {
        #[pin]
        inner: F,
    }

}

impl<F> PinnedFuture<F>
where
    F: Future,
{
    pub fn new(inner: F) -> Self {
        Self { inner }
    }
}

impl<F> Future for PinnedFuture<F>
where
    F: Future,
{
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Self::Output> {
        self.project().inner.poll(ctx)
    }
}

#[cfg(feature = "std")]
#[cfg_attr(docsrs, doc(cfg(feature = "std")))]
impl<F> PinnedFuture<F>
where
    F: Future,
{
    pub fn wait_for_result(self) -> F::Output {
        let thread_waker = Arc::new(thread_ext::TWaker::current());

        self.run_with::<thread_ext::TParker>(thread_waker.into())
    }
}

impl<F> PinnedFuture<F>
where
    F: Future,
{
    pub fn run_with<P: Park>(self, waker: Waker) -> F::Output {
        let mut future = std::pin::pin!(self.inner);
        let mut context = Context::from_waker(&waker);

        loop {
            match future.as_mut().poll(&mut context) {
                Poll::Ready(result) => return result,
                Poll::Pending => P::park_thread(),
            }
        }
    }
}

pub trait PinnedFutureExt: Future + Sized {
    fn to_pinned_future(self) -> PinnedFuture<Self> {
        PinnedFuture::new(self)
    }

    #[cfg(feature = "std")]
    #[cfg_attr(docsrs, doc(cfg(feature = "std")))]
    fn await_for_completion(self) -> Self::Output {
        self.to_pinned_future().wait_for_result()
    }

    fn await_with<P: Park>(self, waker: Waker) -> Self::Output {
        self.to_pinned_future().run_with::<P>(waker)
    }

    #[cfg(feature = "std")]
    #[cfg_attr(docsrs, doc(cfg(feature = "std")))]
    fn spawn_await(self) -> std::thread::JoinHandle<Self::Output>
    where
        Self: Send + 'static,
        Self::Output: Send + 'static,
    {
        std::thread::spawn(|| self.to_pinned_future().await_for_completion())
    }
}

impl<T> PinnedFutureExt for T where T: Future {}

#[cfg(test)]
mod tests {

    use std::{
        mem::MaybeUninit,
        pin::Pin,
        task::{Context, Poll},
    };

    use async_std::{
        io::{self, WriteExt},
        net::TcpStream,
    };
    use bytes::Bytes;
    use http_body_util::{BodyExt, Empty};
    use hyper::Request;
    use pin_project_lite::pin_project;

    use super::*;

    pin_project! {
        #[derive(Debug)]
        pub struct AsyncStdIo<T> {
            #[pin]
            inner: T,
        }
    }

    impl<T> AsyncStdIo<T> {
        pub fn new(inner: T) -> Self {
            Self { inner }
        }
    }

    impl<T> hyper::rt::Read for AsyncStdIo<T>
    where
        T: async_std::io::Read,
    {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            mut buf: hyper::rt::ReadBufCursor<'_>,
        ) -> Poll<Result<(), std::io::Error>> {
            let n = unsafe {
                match async_std::io::Read::poll_read(
                    self.project().inner,
                    cx,
                    MaybeUninit::slice_assume_init_mut(buf.as_mut()),
                ) {
                    Poll::Ready(Ok(amount)) => amount,
                    Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
                    Poll::Pending => return Poll::Pending,
                }
            };
            unsafe {
                buf.advance(n);
            }
            Poll::Ready(Ok(()))
        }
    }
    impl<T> hyper::rt::Write for AsyncStdIo<T>
    where
        T: async_std::io::Write,
    {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<Result<usize, std::io::Error>> {
            async_std::io::Write::poll_write(self.project().inner, cx, buf)
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Result<(), std::io::Error>> {
            async_std::io::Write::poll_flush(self.project().inner, cx)
        }

        fn poll_shutdown(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Result<(), std::io::Error>> {
            async_std::io::Write::poll_close(self.project().inner, cx)
        }

        fn is_write_vectored(&self) -> bool {
            false
        }

        fn poll_write_vectored(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            bufs: &[std::io::IoSlice<'_>],
        ) -> Poll<Result<usize, std::io::Error>> {
            async_std::io::Write::poll_write_vectored(self.project().inner, cx, bufs)
        }
    }

    struct HttpClient;

    impl HttpClient {
        async fn fetch(&self, url: hyper::Uri) -> Result<(), Box<dyn std::error::Error>> {
            let hostname = url.host().expect("No host in url");
            let port_number = url.port_u16().unwrap_or(80);
            let address = format!("{}:{}", hostname, port_number);

            let tcp_stream = TcpStream::connect(address).await?;
            let async_io = AsyncStdIo::new(tcp_stream);

            let (mut sender_req, connection) =
                hyper::client::conn::http1::handshake(async_io).await?;
            let connection = connection.spawn_await();
            let url_authority = url.authority().unwrap();

            let req = Request::builder()
                .method("POST")
                .header(hyper::header::HOST, url_authority.as_str())
                .header(hyper::header::CONTENT_TYPE, "*/*")
                .uri(url)
                .body(Empty::<Bytes>::new())?;

            let mut res = sender_req.send_request(req).await?;

            println!("Status Code: {}", res.status());
            println!("Header: {:#?}\n", res.headers());

            while let Some(body_chunk) = res.frame().await {
                let frame = body_chunk?;
                if let Some(chunk) = frame.data_ref() {
                    io::stdout().write_all(chunk).await?;
                }
            }

            println!("\n\nGET request completed");

            drop(sender_req);

            let _ = connection.join();

            Ok(())
        }

        fn get(
            &self,
            url: hyper::Uri,
        ) -> PinnedFuture<impl Future<Output = Result<(), Box<dyn std::error::Error>>> + '_>
        {
            self.fetch(url).to_pinned_future()
        }
    }
    #[test]
    // #[tokio::test]
    fn test() -> Result<(), Box<dyn std::error::Error>> {
        HttpClient
            .get("https://httpbin.org/get".parse()?)
            .await_for_completion()?;
        Ok(())
    }
}
