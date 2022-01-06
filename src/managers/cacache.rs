use std::collections::HashMap;
use std::convert::{TryFrom, TryInto};

use crate::CacheManager;

use anyhow::{anyhow, Result};
use http::version::Version;
use http_cache_semantics::CachePolicy;
use reqwest::{
    header::{HeaderName, HeaderValue},
    Request, Response, ResponseBuilderExt,
};
use serde::{Deserialize, Serialize};
use url::Url;

/// Implements [`CacheManager`] with [`cacache`](https://github.com/zkat/cacache-rs) as the backend.
#[derive(Debug, Clone)]
pub struct CACacheManager {
    /// Directory where the cache will be stored.
    pub path: String,
}

impl Default for CACacheManager {
    fn default() -> Self {
        CACacheManager {
            path: "./reqwest-cacache".into(),
        }
    }
}

// HTTP version enum in the http crate does not support serde, hence the modified copy.
#[derive(Debug, Copy, Clone, Deserialize, Serialize)]
enum HttpVersion {
    #[serde(rename = "HTTP/0.9")]
    Http09,
    #[serde(rename = "HTTP/1.0")]
    Http10,
    #[serde(rename = "HTTP/1.1")]
    Http11,
    #[serde(rename = "HTTP/2.0")]
    H2,
    #[serde(rename = "HTTP/3.0")]
    H3,
}

impl TryFrom<Version> for HttpVersion {
    type Error = anyhow::Error;

    fn try_from(value: Version) -> Result<Self> {
        Ok(match value {
            Version::HTTP_09 => HttpVersion::Http09,
            Version::HTTP_10 => HttpVersion::Http10,
            Version::HTTP_11 => HttpVersion::Http11,
            Version::HTTP_2 => HttpVersion::H2,
            Version::HTTP_3 => HttpVersion::H3,
            _ => return Err(anyhow!("Unknown HTTP version")),
        })
    }
}

impl From<HttpVersion> for Version {
    fn from(value: HttpVersion) -> Self {
        match value {
            HttpVersion::Http09 => Version::HTTP_09,
            HttpVersion::Http10 => Version::HTTP_10,
            HttpVersion::Http11 => Version::HTTP_11,
            HttpVersion::H2 => Version::HTTP_2,
            HttpVersion::H3 => Version::HTTP_3,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct Store {
    response: StoredResponse,
    policy: CachePolicy,
}

#[derive(Debug, Deserialize, Serialize)]
struct StoredResponse {
    body: Vec<u8>,
    headers: HashMap<String, String>,
    status: u16,
    url: Url,
    version: HttpVersion,
}

async fn to_store(res: Response, policy: CachePolicy) -> Result<Store> {
    let mut headers = HashMap::new();
    for header in res.headers() {
        headers.insert(header.0.as_str().to_owned(), header.1.to_str()?.to_owned());
    }
    let status = res.status().as_u16();
    let url = res.url().clone();
    let version = res.version().try_into()?;
    let body: Vec<u8> = res.bytes().await?.to_vec();
    Ok(Store {
        response: StoredResponse {
            body,
            headers,
            status,
            url,
            version,
        },
        policy,
    })
}

fn from_store(store: &Store) -> Result<Response> {
    let mut res = http::Response::builder()
        .status(store.response.status)
        .url(store.response.url.clone())
        .version(store.response.version.into())
        .body(store.response.body.clone())?;
    for header in &store.response.headers {
        res.headers_mut().insert(
            HeaderName::from_lowercase(header.0.clone().as_str().to_lowercase().as_bytes())?,
            HeaderValue::from_str(header.1.clone().as_str())?,
        );
    }
    Ok(Response::from(res))
}

fn req_key(req: &Request) -> String {
    format!("{}:{}", req.method(), req.url())
}

#[allow(dead_code)]
impl CACacheManager {
    /// Clears out the entire cache.
    pub async fn clear(&self) -> Result<()> {
        cacache::clear(&self.path).await?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl CacheManager for CACacheManager {
    async fn get(&self, req: &Request) -> Result<Option<(Response, CachePolicy)>> {
        let store: Store = match cacache::read(&self.path, &req_key(req)).await {
            Ok(d) => bincode::deserialize(&d)?,
            Err(_e) => {
                return Ok(None);
            }
        };
        Ok(Some((from_store(&store)?, store.policy)))
    }

    // TODO - This needs some reviewing.
    async fn put(&self, req: &Request, res: Response, policy: CachePolicy) -> Result<Response> {
        let status = res.status();
        let url = res.url().clone();
        let version = res.version();
        let headers = res.headers().clone();
        let data = to_store(res, policy).await?;
        let bytes = bincode::serialize(&data)?;
        cacache::write(&self.path, &req_key(req), bytes).await?;
        let mut ret_res = http::Response::builder()
            .status(status)
            .url(url)
            .version(version)
            .body(data.response.body)?;
        for header in headers {
            ret_res
                .headers_mut()
                .insert(header.0.unwrap(), header.1.clone());
        }
        *ret_res.version_mut() = version;
        Ok(Response::from(ret_res))
    }

    async fn delete(&self, req: &Request) -> Result<()> {
        cacache::remove(&self.path, &req_key(req)).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use http::{Method, Response};
    use reqwest::Request;
    use std::str::FromStr;

    #[tokio::test]
    async fn can_cache_response() -> Result<()> {
        let url = reqwest::Url::from_str("https://example.com")?;
        let res = Response::new("test");
        let res = reqwest::Response::from(res);
        let req = Request::new(Method::GET, url);
        let policy = CachePolicy::new(&req, &res);
        let manager = CACacheManager::default();
        manager.put(&req, res, policy).await?;
        let data = manager.get(&req).await?;
        let body = match data {
            Some(d) => d.0.text().await?,
            None => String::new(),
        };
        assert_eq!(&body, "test");
        manager.delete(&req).await?;
        let data = manager.get(&req).await?;
        assert!(data.is_none());
        manager.clear().await?;
        Ok(())
    }
}
