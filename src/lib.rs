#![forbid(unsafe_code, future_incompatible)]
#![deny(
    missing_docs,
    missing_debug_implementations,
    missing_copy_implementations,
    nonstandard_style,
    unused_qualifications,
    rustdoc::missing_doc_code_examples
)]
//! A caching middleware for Reqwest that follows HTTP caching rules.
//! By default it uses [`cacache`](https://github.com/zkat/cacache-rs) as the backend cache manager.
//!
//! ## Example
//!
//! ```no_run
//! use reqwest::Client;
//! use reqwest_middleware::{ClientBuilder, Result};
//! use reqwest_middleware_cache::{managers::CACacheManager, Cache, CacheMode};
//!
//! #[tokio::main]
//! async fn main() -> Result<()> {
//!     let client = ClientBuilder::new(Client::new())
//!         .with(Cache {
//!             mode: CacheMode::Default,
//!             cache_manager: CACacheManager::default(),
//!         })
//!         .build();
//!     client
//!         .get("https://developer.mozilla.org/en-US/docs/Web/HTTP/Caching")
//!         .send()
//!         .await?;
//!     Ok(())
//! }
//! ```

use std::time::SystemTime;

use anyhow::anyhow;
use http::{
    header::{HeaderName, CACHE_CONTROL},
    HeaderValue, Method,
};
use http_cache_semantics::{AfterResponse, BeforeRequest, CachePolicy};
use reqwest::{Request, Response};
use reqwest_middleware::{Error, Middleware, Next, Result};
use task_local_extensions::Extensions;

/// Backend cache managers, cacache is the default.
pub mod managers;

/// A trait providing methods for storing, reading, and removing cache records.
#[async_trait::async_trait]
pub trait CacheManager {
    /// Attempts to pull a cached reponse and related policy from cache.
    async fn get(&self, req: &Request) -> Result<Option<(Response, CachePolicy)>>;
    /// Attempts to cache a response and related policy.
    async fn put(&self, req: &Request, res: Response, policy: CachePolicy) -> Result<Response>;
    /// Attempts to remove a record from cache.
    async fn delete(&self, req: &Request) -> Result<()>;
}

/// Similar to [make-fetch-happen cache options](https://github.com/npm/make-fetch-happen#--optscache).
/// Passed in when the [`Cache`] struct is being built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheMode {
    /// Will inspect the HTTP cache on the way to the network.
    /// If there is a fresh response it will be used.
    /// If there is a stale response a conditional request will be created,
    /// and a normal request otherwise.
    /// It then updates the HTTP cache with the response.
    /// If the revalidation request fails (for example, on a 500 or if you're offline),
    /// the stale response will be returned.
    Default,
    /// Behaves as if there is no HTTP cache at all.
    NoStore,
    /// Behaves as if there is no HTTP cache on the way to the network.
    /// Ergo, it creates a normal request and updates the HTTP cache with the response.
    Reload,
    /// Creates a conditional request if there is a response in the HTTP cache
    /// and a normal request otherwise. It then updates the HTTP cache with the response.
    NoCache,
    /// Uses any response in the HTTP cache matching the request,
    /// not paying attention to staleness. If there was no response,
    /// it creates a normal request and updates the HTTP cache with the response.
    ForceCache,
    /// Uses any response in the HTTP cache matching the request,
    /// not paying attention to staleness. If there was no response,
    /// it returns a network error. (Can only be used when request’s mode is "same-origin".
    /// Any cached redirects will be followed assuming request’s redirect mode is "follow"
    /// and the redirects do not violate request’s mode.)
    OnlyIfCached,
}

/// Caches requests according to http spec
#[derive(Debug, Clone)]
pub struct Cache<T: CacheManager + Send + Sync + 'static> {
    /// Determines the manager behavior
    pub mode: CacheMode,
    /// Manager instance that implements the CacheManager trait
    pub cache_manager: T,
}

impl<T: CacheManager + Send + Sync + 'static> Cache<T> {
    /// Called by the Reqwest middleware handle method when a request is made.
    pub async fn run<'a>(
        &'a self,
        mut req: Request,
        next: Next<'a>,
        extensions: &mut Extensions,
    ) -> Result<Response> {
        let is_cacheable = (req.method() == Method::GET || req.method() == Method::HEAD)
            && self.mode != CacheMode::NoStore
            && self.mode != CacheMode::Reload;

        if !is_cacheable {
            return self.remote_fetch(req, next, extensions).await;
        }

        if let Some(store) = self.cache_manager.get(&req).await? {
            let (mut res, policy) = store;
            if let Some(warning_code) = get_warning_code(&res) {
                // https://tools.ietf.org/html/rfc7234#section-4.3.4
                //
                // If a stored response is selected for update, the cache MUST:
                //
                // * delete any Warning header fields in the stored response with
                //   warn-code 1xx (see Section 5.5);
                //
                // * retain any Warning header fields in the stored response with
                //   warn-code 2xx;
                //
                #[allow(clippy::manual_range_contains)]
                if warning_code >= 100 && warning_code < 200 {
                    res.headers_mut().remove(reqwest::header::WARNING);
                }
            }

            if self.mode == CacheMode::Default {
                Ok(self
                    .conditional_fetch(req, res, policy, next, extensions)
                    .await?)
            } else if self.mode == CacheMode::NoCache {
                req.headers_mut().insert(
                    CACHE_CONTROL,
                    HeaderValue::from_str("no-cache")
                        .expect("Unable to insert cache-control header"),
                );
                Ok(self
                    .conditional_fetch(req, res, policy, next, extensions)
                    .await?)
            } else if self.mode == CacheMode::ForceCache || self.mode == CacheMode::OnlyIfCached {
                //   112 Disconnected operation
                // SHOULD be included if the cache is intentionally disconnected from
                // the rest of the network for a period of time.
                // (https://tools.ietf.org/html/rfc2616#section-14.46)
                add_warning(&mut res, req.url(), 112, "Disconnected operation");
                Ok(res)
            } else {
                Ok(self.remote_fetch(req, next, extensions).await?)
            }
        } else if self.mode == CacheMode::OnlyIfCached {
            // ENOTCACHED
            let err_res = http::Response::builder()
                .status(http::StatusCode::GATEWAY_TIMEOUT)
                .body("")
                .expect("Unable to build ENOTCACHED response");
            Ok(err_res.into())
        } else {
            Ok(self.remote_fetch(req, next, extensions).await?)
        }
    }

    async fn conditional_fetch<'a>(
        &self,
        mut req: Request,
        mut cached_res: Response,
        mut policy: CachePolicy,
        next: Next<'_>,
        extensions: &mut Extensions,
    ) -> Result<Response> {
        let before_req = policy.before_request(&req, SystemTime::now());
        match before_req {
            BeforeRequest::Fresh(parts) => {
                update_response_headers(parts, &mut cached_res)?;
                return Ok(cached_res);
            }
            BeforeRequest::Stale {
                request: parts,
                matches,
            } => {
                if matches {
                    update_request_headers(parts, &mut req)?;
                }
            }
        }
        let copied_req = req.try_clone().ok_or_else(|| {
            Error::Middleware(anyhow!(
                "Request object is not clonable. Are you passing a streaming body?".to_string()
            ))
        })?;
        match self.remote_fetch(req, next, extensions).await {
            Ok(cond_res) => {
                if cond_res.status().is_server_error() && must_revalidate(&cached_res) {
                    //   111 Revalidation failed
                    //   MUST be included if a cache returns a stale response
                    //   because an attempt to revalidate the response failed,
                    //   due to an inability to reach the server.
                    // (https://tools.ietf.org/html/rfc2616#section-14.46)
                    add_warning(
                        &mut cached_res,
                        copied_req.url(),
                        111,
                        "Revalidation failed",
                    );
                    Ok(cached_res)
                } else if cond_res.status() == http::StatusCode::NOT_MODIFIED {
                    let mut res = http::Response::builder()
                        .status(cond_res.status())
                        .body(
                            cached_res
                                .text()
                                .await
                                .expect("Unable to get cached response body"),
                        )
                        .expect("Unable to build ENOTCACHED response");
                    for (key, value) in cond_res.headers() {
                        res.headers_mut().append(key, value.clone());
                    }
                    let mut converted = Response::from(res);
                    let after_res =
                        policy.after_response(&copied_req, &cond_res, SystemTime::now());
                    match after_res {
                        AfterResponse::Modified(new_policy, parts) => {
                            policy = new_policy;
                            update_response_headers(parts, &mut converted)?;
                        }
                        AfterResponse::NotModified(new_policy, parts) => {
                            policy = new_policy;
                            update_response_headers(parts, &mut converted)?;
                        }
                    }
                    let res = self
                        .cache_manager
                        .put(&copied_req, converted, policy)
                        .await?;
                    Ok(res)
                } else {
                    Ok(cached_res)
                }
            }
            Err(e) => {
                if must_revalidate(&cached_res) {
                    Err(e)
                } else {
                    //   111 Revalidation failed
                    //   MUST be included if a cache returns a stale response
                    //   because an attempt to revalidate the response failed,
                    //   due to an inability to reach the server.
                    // (https://tools.ietf.org/html/rfc2616#section-14.46)
                    add_warning(
                        &mut cached_res,
                        copied_req.url(),
                        111,
                        "Revalidation failed",
                    );
                    //   199 Miscellaneous warning
                    //   The warning text MAY include arbitrary information to
                    //   be presented to a human user, or logged. A system
                    //   receiving this warning MUST NOT take any automated
                    //   action, besides presenting the warning to the user.
                    // (https://tools.ietf.org/html/rfc2616#section-14.46)
                    add_warning(
                        &mut cached_res,
                        copied_req.url(),
                        199,
                        format!("Miscellaneous Warning {}", e).as_str(),
                    );
                    Ok(cached_res)
                }
            }
        }
    }

    async fn remote_fetch<'a>(
        &'a self,
        req: Request,
        next: Next<'a>,
        mut ext: &'a mut Extensions,
    ) -> Result<Response> {
        let copied_req = req.try_clone().ok_or_else(|| {
            Error::Middleware(anyhow!(
                "Request object is not clonable. Are you passing a streaming body?".to_string()
            ))
        })?;
        let res = next.run(req, &mut ext).await?;
        let is_method_get_head =
            copied_req.method() == Method::GET || copied_req.method() == Method::HEAD;
        let policy = CachePolicy::new(&copied_req, &res);
        let is_cacheable = self.mode != CacheMode::NoStore
            && is_method_get_head
            && res.status() == http::StatusCode::OK
            && policy.is_storable();
        if is_cacheable {
            Ok(self.cache_manager.put(&copied_req, res, policy).await?)
        } else if !is_method_get_head {
            self.cache_manager.delete(&copied_req).await?;
            Ok(res)
        } else {
            Ok(res)
        }
    }
}

fn must_revalidate(res: &Response) -> bool {
    if let Some(val) = res.headers().get(CACHE_CONTROL.as_str()) {
        val.to_str()
            .expect("Unable to convert header value to string")
            .to_lowercase()
            .contains("must-revalidate")
    } else {
        false
    }
}

fn get_warning_code(res: &Response) -> Option<usize> {
    res.headers().get(reqwest::header::WARNING).and_then(|hdr| {
        hdr.to_str()
            .expect("Unable to convert warning to string")
            .chars()
            .take(3)
            .collect::<String>()
            .parse()
            .ok()
    })
}

fn update_request_headers(parts: http::request::Parts, req: &mut Request) -> Result<()> {
    let headers = parts.headers;
    for header in headers.iter() {
        req.headers_mut().insert(
            HeaderName::from_lowercase(header.0.clone().as_str().to_lowercase().as_bytes())
                .expect("Unable to set header from part"),
            header.1.clone(),
        );
    }
    Ok(())
}

fn update_response_headers(parts: http::response::Parts, res: &mut Response) -> Result<()> {
    for header in parts.headers.iter() {
        res.headers_mut().insert(header.0.clone(), header.1.clone());
    }
    Ok(())
}

fn add_warning(res: &mut Response, uri: &reqwest::Url, code: usize, message: &str) {
    //   Warning    = "Warning" ":" 1#warning-value
    // warning-value = warn-code SP warn-agent SP warn-text [SP warn-date]
    // warn-code  = 3DIGIT
    // warn-agent = ( host [ ":" port ] ) | pseudonym
    //                 ; the name or pseudonym of the server adding
    //                 ; the Warning header, for use in debugging
    // warn-text  = quoted-string
    // warn-date  = <"> HTTP-date <">
    // (https://tools.ietf.org/html/rfc2616#section-14.46)
    //
    let val = HeaderValue::from_str(
        format!(
            "{} {} {:?} \"{}\"",
            code,
            uri.host().expect("Invalid URL"),
            message,
            httpdate::fmt_http_date(SystemTime::now())
        )
        .as_str(),
    )
    .expect("Failed to generate warning string");
    res.headers_mut().append(reqwest::header::WARNING, val);
}

#[async_trait::async_trait]
impl<T: CacheManager + 'static + Send + Sync> Middleware for Cache<T> {
    async fn handle(
        &self,
        req: Request,
        extensions: &mut Extensions,
        next: Next<'_>,
    ) -> Result<Response> {
        let res = self.run(req, next, extensions).await?;
        Ok(res)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{HeaderValue, Response};
    use reqwest::Result;
    use std::str::FromStr;

    #[tokio::test]
    async fn can_get_warning_code() -> Result<()> {
        let url = reqwest::Url::from_str("https://example.com").unwrap();
        let mut res = reqwest::Response::from(Response::new(""));
        add_warning(&mut res, &url, 111, "Revalidation failed");
        let code = get_warning_code(&res).unwrap();
        assert_eq!(code, 111);
        Ok(())
    }

    #[tokio::test]
    async fn can_check_revalidate() {
        let mut res = Response::new("");
        res.headers_mut().append(
            "Cache-Control",
            HeaderValue::from_str("max-age=1733992, must-revalidate").unwrap(),
        );
        let check = must_revalidate(&res.into());
        assert!(check, "{}", true)
    }
}
