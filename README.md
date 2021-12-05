# reqwest-middleware-cache

[![Rust](https://github.com/06chaynes/reqwest-middleware-cache/actions/workflows/rust.yml/badge.svg)](https://github.com/06chaynes/reqwest-middleware-cache/actions/workflows/rust.yml) ![crates.io](https://img.shields.io/crates/v/reqwest-middleware-cache.svg)

A caching middleware for [reqwest](https://github.com/seanmonstar/reqwest) that follows HTTP caching rules, thanks to [http-cache-semantics](https://github.com/kornelski/rusty-http-cache-semantics). By default it uses [cacache](https://github.com/zkat/cacache-rs) as the backend cache manager. Uses [reqwest-middleware](https://github.com/TrueLayer/reqwest-middleware) for middleware support.

## Install

Cargo.toml

```toml
[dependencies]
reqwest-middleware-cache = "0.1.0"
```

With [cargo add](https://github.com/killercup/cargo-edit#Installation) installed :

```sh
cargo add reqwest-middleware-cache
```

## Example

```rust
use reqwest::Client;
use reqwest_middleware::{ClientBuilder, Result};
use reqwest_middleware_cache::{managers::CACacheManager, Cache, CacheMode};

#[tokio::main]
async fn main() -> Result<()> {
    let client = ClientBuilder::new(Client::new())
        .with(Cache {
            mode: CacheMode::Default,
            cache_manager: CACacheManager::default(),
        })
        .build();
    client
        .get("https://developer.mozilla.org/en-US/docs/Web/HTTP/Caching")
        .send()
        .await?;
    Ok(())
}
```

## Features

The following features are available. By default `manager-cacache` is enabled.

- `manager-cacache` (default): use [cacache](https://github.com/zkat/cacache-rs), a high-performance disk cache, for the manager backend.

## Documentation

- [API Docs](https://docs.rs/reqwest-middleware-cache)

## License

This project is licensed under [the Apache-2.0 License](LICENSE.md)
