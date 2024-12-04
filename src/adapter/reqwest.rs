use super::BoltLoadAdapter;
use async_trait::async_trait;
use futures::Stream;
use url::Url;

#[derive(Debug, Clone)]
pub struct ReqwestAdapter {
    client: reqwest::Client,
    url: Url,
    head_response: tokio::sync::Mutex<Option<reqwest::Response>>,
    before_request: Option<Box<dyn Fn(reqwest::RequestBuilder) -> reqwest::RequestBuilder>>,
}

impl ReqwestAdapter {
    async fn perform_head(&self) -> Result<reqwest::Response, reqwest::Error> {
        let response = self
            .apply_before_request(self.client.head(self.url.clone()))
            .send()
            .await?;
        response.error_for_status()?;
        Ok(response)
    }
    pub fn before_request(
        &mut self,
        f: impl Fn(reqwest::RequestBuilder) -> reqwest::RequestBuilder,
    ) {
        self.before_request = Some(Box::new(f));
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

#[async_trait]
impl BoltLoadAdapter for ReqwestAdapter {
    async fn get_content_size(&self) -> std::io::Result<u64> {
        let mut response = self.head_response.lock().await;
        if response.is_none() {
            *response = Some(self.perform_head().await?);
        }
        Ok(response.as_ref().unwrap().content_length().unwrap_or(0))
    }

    async fn is_range_stream_available(&self) -> bool {
        let response = self.head_response.lock().await;
        if response.is_none() {
            *response = Some(self.perform_head().await?);
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
            let response = self
                .apply_before_request(
                    self.client
                        .get(self.url.clone())
                        .header("Range", "bytes=0-8"),
                )
                .send()
                .await?;
            is_range_supported = response.error_for_status().is_ok_and(|res| {
                res.headers().get("Content-Range").is_some()
                    && res.content_length().unwrap_or(0) > 1
            });
        }
        is_range_supported
    }

    async fn full_stream(&self) -> std::io::Result<Stream<Item = Result<Bytes, Error>>> {
        let response = self
            .apply_before_request(self.client.get(self.url.clone()))
            .send()
            .await?;
        response.error_for_status()?;
        Ok(response.bytes_stream())
    }

    async fn range_stream(
        &self,
        start: u64,
        end: u64,
    ) -> std::io::Result<Stream<Item = Result<Bytes, Error>>> {
        let response = self
            .apply_before_request(
                self.client
                    .get(self.url.clone())
                    .header("Range", format!("bytes={}-{}", start, end - 1)),
            )
            .send()
            .await?;
        response.error_for_status()?;
        Ok(response.bytes_stream())
    }
}
