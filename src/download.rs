use crate::feed::Episode;
use crate::sanitize::torrent_filename;
use anyhow::Context;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum Outcome {
    Downloaded(PathBuf),
    Skipped(PathBuf),
    Failed(String),
}

pub fn download(client: &reqwest::blocking::Client, ep: &Episode, dir: &Path) -> Outcome {
    let filename = torrent_filename(&ep.title, &ep.torrent_url);
    let target = dir.join(&filename);
    if target.exists() {
        return Outcome::Skipped(target);
    }

    let bytes = match fetch_bytes(client, &ep.torrent_url) {
        Ok(bytes) => bytes,
        Err(e) => return Outcome::Failed(format!("{filename}: {e:#}")),
    };

    let part = dir.join(format!("{filename}.part"));
    let write_result = std::fs::write(&part, &bytes)
        .and_then(|()| std::fs::rename(&part, &target));
    match write_result {
        Ok(()) => Outcome::Downloaded(target),
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            Outcome::Failed(format!("{filename}: {e}"))
        }
    }
}

pub(crate) fn fetch_bytes(client: &reqwest::blocking::Client, url: &str) -> anyhow::Result<Vec<u8>> {
    let response = client
        .get(url)
        .send()
        .context("request failed")?
        .error_for_status()?;
    Ok(response.bytes().context("reading torrent body")?.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feed::Episode;
    use std::io::{Read, Write};

    /// One-shot HTTP server on an ephemeral port; returns the URL to hit.
    fn serve_once(status_line: &'static str, body: &'static [u8]) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf);
                let _ = write!(
                    sock,
                    "HTTP/1.1 {status_line}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(body);
            }
        });
        format!("http://{addr}/Download/20260628/abcdef123456.torrent")
    }

    fn test_client() -> reqwest::blocking::Client {
        reqwest::blocking::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap()
    }

    fn episode(title: &str, url: String) -> Episode {
        Episode { title: title.to_string(), torrent_url: url, size: None, pub_date: None }
    }

    #[test]
    fn downloads_to_sanitized_filename() {
        let dir = tempfile::tempdir().unwrap();
        let ep = episode("Test / Episode [01]", serve_once("200 OK", b"d8:announce0:e"));

        let outcome = download(&test_client(), &ep, dir.path());

        let expected = dir.path().join("Test ⁄ Episode [01].torrent");
        match outcome {
            Outcome::Downloaded(p) => assert_eq!(p, expected),
            other => panic!("expected Downloaded, got {other:?}"),
        }
        assert_eq!(std::fs::read(&expected).unwrap(), b"d8:announce0:e");
        assert!(!dir.path().join("Test ⁄ Episode [01].torrent.part").exists());
    }

    #[test]
    fn skips_existing_file_without_network() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("Already Here.torrent");
        std::fs::write(&target, b"original").unwrap();
        // Port 9 (discard) — connecting would fail; Skipped must happen first.
        let ep = episode("Already Here", "http://127.0.0.1:9/x.torrent".to_string());

        match download(&test_client(), &ep, dir.path()) {
            Outcome::Skipped(p) => assert_eq!(p, target),
            other => panic!("expected Skipped, got {other:?}"),
        }
        assert_eq!(std::fs::read(&target).unwrap(), b"original");
    }

    #[test]
    fn http_error_reports_failed_and_leaves_no_files() {
        let dir = tempfile::tempdir().unwrap();
        let ep = episode("Missing", serve_once("404 Not Found", b""));

        match download(&test_client(), &ep, dir.path()) {
            Outcome::Failed(msg) => assert!(msg.contains("404"), "message was: {msg}"),
            other => panic!("expected Failed, got {other:?}"),
        }
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn connection_error_reports_failed() {
        let dir = tempfile::tempdir().unwrap();
        let ep = episode("Unreachable", "http://127.0.0.1:9/x.torrent".to_string());

        assert!(matches!(download(&test_client(), &ep, dir.path()), Outcome::Failed(_)));
    }

    /// Hits the real mikanani.me feed. Run explicitly with:
    ///   cargo test -- --ignored
    /// Requires network access via env-var or macOS system proxy.
    #[test]
    #[ignore = "network: fetches the real mikanani.me feed"]
    fn live_fetch_and_download_newest_episode() {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap();
        let feed = crate::feed::fetch_feed(
            &client,
            "https://mikanani.me/RSS/Bangumi?bangumiId=3950&subgroupid=597",
        )
        .expect("fetching live feed (is the proxy up?)");
        assert!(!feed.episodes.is_empty());

        let dir = tempfile::tempdir().unwrap();
        match download(&client, &feed.episodes[0], dir.path()) {
            Outcome::Downloaded(path) => {
                let bytes = std::fs::read(path).unwrap();
                // torrent files are bencoded dicts: they start with 'd'
                assert_eq!(bytes.first(), Some(&b'd'));
            }
            other => panic!("expected Downloaded, got {other:?}"),
        }
    }
}
