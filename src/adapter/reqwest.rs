use std::{pin::Pin, sync::Arc};

use super::{AnyStream, BoltLoadAdapter};
use async_trait::async_trait;
use futures::Stream;
use url::Url;

#[derive(Clone)]
pub struct ReqwestAdapter {
    client: reqwest::Client,
    url: Url,
    head_response: Arc<async_lock::Mutex<Option<reqwest::Response>>>,
    before_request:
        Arc<Option<Box<dyn Fn(reqwest::RequestBuilder) -> reqwest::RequestBuilder + Send + Sync>>>,
}

impl ReqwestAdapter {
    async fn perform_head(&self) -> Result<reqwest::Response, std::io::Error> {
        let response = self
            .apply_before_request(self.client.head(self.url.clone()))
            .send()
            .await
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
        response
            .error_for_status()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))
    }

    pub fn before_request(
        &mut self,
        f: impl Fn(reqwest::RequestBuilder) -> reqwest::RequestBuilder + Send + Sync + 'static,
    ) {
        self.before_request = Arc::new(Some(Box::new(f)));
    }

    #[inline]
    fn apply_before_request(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(f) = self.before_request.as_ref() {
            f(builder)
        } else {
            builder
        }
    }
}

struct ReqwestStream(AnyStream<Result<bytes::Bytes, reqwest::Error>>);

impl Stream for ReqwestStream {
    type Item = std::io::Result<bytes::Bytes>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        Pin::new(&mut self.get_mut().0).poll_next(cx).map(|opt| {
            opt.map(|res| {
                res.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))
            })
        })
    }
}

#[async_trait]
impl BoltLoadAdapter for ReqwestAdapter {
    type Item = std::io::Result<bytes::Bytes>;
    type Stream = AnyStream<Self::Item>;

    async fn get_content_size(&self) -> std::io::Result<u64> {
        let mut response = self.head_response.lock().await;
        if response.is_none() {
            *response = Some(self.perform_head().await?);
        }
        Ok(response.as_ref().unwrap().content_length().unwrap_or(0))
    }

    async fn is_range_stream_available(&self) -> bool {
        let mut response = self.head_response.lock().await;
        if response.is_none() {
            match self.perform_head().await {
                Ok(res) => *response = Some(res),
                Err(_) => return false,
            }
        }
        // check Accept-Ranges header
        let mut is_range_supported = response
            .as_ref()
            .unwrap()
            .headers()
            .get("Accept-Ranges")
            .is_some_and(|v| v != "none");
        // try to send a real range request to test
        if !is_range_supported {
            is_range_supported = self
                .apply_before_request(
                    self.client
                        .get(self.url.clone())
                        .header("Range", "bytes=0-8"),
                )
                .send()
                .await
                .and_then(|res| res.error_for_status())
                .and_then(|res| {
                    Ok(res.headers().get("Content-Range").is_some()
                        && res.content_length().unwrap_or(0) > 1)
                })
                .unwrap_or_default();
        }
        is_range_supported
    }

    async fn full_stream(&self) -> std::io::Result<Self::Stream> {
        let response = self
            .apply_before_request(self.client.get(self.url.clone()))
            .send()
            .await
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?
            .error_for_status()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
        let stream = ReqwestStream(Box::new(response.bytes_stream()));
        Ok(Box::new(stream))
    }

    async fn range_stream(&self, start: u64, end: u64) -> std::io::Result<Self::Stream> {
        let response = self
            .apply_before_request(
                self.client
                    .get(self.url.clone())
                    .header("Range", format!("bytes={}-{}", start, end - 1)),
            )
            .send()
            .await
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?
            .error_for_status()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
        let stream = ReqwestStream(Box::new(response.bytes_stream()));
        Ok(Box::new(stream))
    }
}
