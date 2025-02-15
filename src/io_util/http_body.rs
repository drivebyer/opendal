// Copyright 2022 Datafuse Labs.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::future::Future;
use std::io::Error;
use std::io::Result;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;

use bytes::Bytes;
use futures::channel::mpsc::Sender;
use futures::channel::mpsc::{self};
use futures::ready;
use futures::AsyncWrite;
use futures::Sink;
use futures::StreamExt;
use http::Response;
use hyper::client::ResponseFuture;
use hyper::Body;
use pin_project::pin_project;

use crate::error::other;
use crate::ops::OpWrite;

/// Create a HTTP channel.
///
/// Read [`opendal::services::s3`]'s `write` implementations for more details.
pub(crate) fn new_http_channel() -> (Sender<Bytes>, Body) {
    let (tx, rx) = mpsc::channel(0);

    (tx, Body::wrap_stream(rx.map(Ok::<_, Error>)))
}

#[pin_project]
pub(crate) struct HttpBodyWriter {
    op: OpWrite,
    tx: Sender<Bytes>,
    fut: ResponseFuture,
    handle: fn(&OpWrite, Response<Body>) -> Result<()>,
}

impl HttpBodyWriter {
    /// Create a HTTP body writer.
    ///
    /// # Params
    ///
    /// - op: the OpWrite that input by `write` operation.
    /// - tx: the Sender created by [`new_http_channel`]
    /// - fut: the future created by HTTP client.
    /// - handle: the handle which parse response to result.
    ///
    /// Read [`opendal::services::s3`]'s `write` implementations for more details.
    pub fn new(
        op: &OpWrite,
        tx: Sender<Bytes>,
        fut: ResponseFuture,
        handle: fn(&OpWrite, Response<Body>) -> Result<()>,
    ) -> HttpBodyWriter {
        HttpBodyWriter {
            op: op.clone(),
            tx,
            fut,
            handle,
        }
    }

    fn poll_response(
        self: &mut Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), Error>> {
        match Pin::new(&mut self.fut).poll(cx) {
            Poll::Ready(Ok(resp)) => Poll::Ready((self.handle)(&self.op, resp)),
            // TODO: we need to inject an object error here.
            Poll::Ready(Err(e)) => Poll::Ready(Err(other(e))),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for HttpBodyWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize>> {
        if let Poll::Ready(v) = Pin::new(&mut *self).poll_response(cx) {
            unreachable!("response returned too early: {:?}", v)
        }

        ready!(self.tx.poll_ready(cx).map_err(other))?;

        let size = buf.len();
        self.tx
            .start_send(Bytes::from(buf.to_vec()))
            .map_err(other)?;

        Poll::Ready(Ok(size))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        Pin::new(&mut self.tx).poll_flush(cx).map_err(other)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        if let Err(e) = ready!(Pin::new(&mut self.tx).poll_close(cx)) {
            return Poll::Ready(Err(other(e)));
        }

        self.poll_response(cx)
    }
}

#[cfg(test)]
mod tests {
    use futures::SinkExt;
    use serde::Deserialize;

    use super::*;

    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct HttpBin {
        data: String,
    }

    #[tokio::test]
    async fn test_http_channel() {
        let (mut tx, body) = new_http_channel();

        let fut = tokio::spawn(async {
            let client = hyper::Client::builder().build(hyper_tls::HttpsConnector::new());
            let req = hyper::Request::put("https://httpbin.org/anything")
                .body(body)
                .expect("request must be valid");
            let resp = client.request(req).await.expect("request must succeed");
            let bs = hyper::body::to_bytes(resp.into_body())
                .await
                .expect("read body must succeed");
            serde_json::from_slice::<HttpBin>(&bs).expect("deserialize must succeed")
        });

        tx.feed(Bytes::from("Hello, World!"))
            .await
            .expect("feed must succeed");
        tx.close().await.expect("close must succeed");

        let content = fut.await.expect("future must polled");
        assert_eq!(&content.data, "Hello, World!")
    }
}
