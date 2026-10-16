//! HTTP client construction and network error hints.

use anyhow::{Context, Result};
use std::time::Duration;

use crate::mikan::feed;
use crate::ui::Spinner;

pub fn build_client(proxy: Option<&str>) -> Result<reqwest::blocking::Client> {
    let mut builder = reqwest::blocking::Client::builder().timeout(Duration::from_secs(30));
    if let Some(proxy) = proxy {
        builder = builder.proxy(reqwest::Proxy::all(proxy).context("无效的 --proxy 地址")?);
    }
    builder.build().context("创建 HTTP 客户端")
}

pub fn add_proxy_hint(e: anyhow::Error) -> anyhow::Error {
    let text = format!("{e:#}").to_lowercase();
    let looks_blocked = [
        "certificate", "tls", "ssl", "connect", "timed out", "dns",
        "bad gateway", "gateway timeout", "proxy",
    ]
    .iter()
    .any(|needle| text.contains(needle));
    if looks_blocked {
        e.context(
            "无法访问订阅 —— mikanani.me 常被 DNS 污染；请检查代理\
             （--proxy http://host:port、HTTPS_PROXY 或系统代理）",
        )
    } else {
        e
    }
}

/// Fetch a feed behind a spinner, adding the proxy hint on network failures.
pub fn fetch_feed(client: &reqwest::blocking::Client, url: &str) -> Result<feed::Feed> {
    let _spinner = Spinner::start("正在获取订阅…");
    feed::fetch_feed(client, url).map_err(add_proxy_hint)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_client_rejects_bad_proxy() {
        assert!(build_client(Some("not a url")).is_err());
    }

    #[test]
    fn build_client_accepts_http_proxy() {
        assert!(build_client(Some("http://127.0.0.1:7890")).is_ok());
    }

    #[test]
    fn proxy_hint_added_for_gateway_errors() {
        let err = anyhow::anyhow!("HTTP status server error (502 Bad Gateway) for url (http://x/)");
        let hinted = format!("{:#}", add_proxy_hint(err));
        assert!(hinted.contains("检查代理"), "was: {hinted}");
    }

    #[test]
    fn proxy_hint_added_for_connect_errors() {
        let err = anyhow::anyhow!("error sending request: connection refused");
        let hinted = format!("{:#}", add_proxy_hint(err));
        assert!(hinted.contains("检查代理"), "was: {hinted}");
    }

    #[test]
    fn no_proxy_hint_for_plain_http_404() {
        let err = anyhow::anyhow!("HTTP status client error (404 Not Found) for url (https://mikanani.me/RSS/x)");
        let hinted = format!("{:#}", add_proxy_hint(err));
        assert!(!hinted.contains("检查代理"), "was: {hinted}");
    }
}
