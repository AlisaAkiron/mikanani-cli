use crate::config::QbtProfile;
use anyhow::{Context, bail};
use std::time::Duration;

/// Minimal qBittorrent WebUI API v2 client (blocking).
///
/// Deliberately built with `.no_proxy()`: qBt endpoints are local/LAN and
/// must not be routed through the user's system proxy.
pub struct QbtClient {
    endpoint: String,
    http: reqwest::blocking::Client,
    sid: Option<String>,
    version: String,
}

impl std::fmt::Debug for QbtClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QbtClient")
            .field("endpoint", &self.endpoint)
            .field("sid", &self.sid.as_ref().map(|_| "<redacted>"))
            .field("version", &self.version)
            .finish()
    }
}

impl QbtClient {
    /// Logs in when the profile has a username (empty = qBt's localhost
    /// auth bypass), then verifies connectivity via /app/version.
    pub fn connect(profile: &QbtProfile) -> anyhow::Result<QbtClient> {
        let http = reqwest::blocking::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(15))
            .build()
            .context("building the qBittorrent HTTP client")?;
        let mut client = QbtClient {
            endpoint: profile.endpoint.trim_end_matches('/').to_string(),
            http,
            sid: None,
            version: String::new(),
        };
        if !profile.username.is_empty() {
            client.login(&profile.username, &profile.password)?;
        }
        client.version = client.fetch_version()?;
        Ok(client)
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    fn unreachable_hint(&self) -> String {
        format!(
            "could not reach qBittorrent at {} — is the WebUI enabled?",
            self.endpoint
        )
    }

    fn login(&mut self, username: &str, password: &str) -> anyhow::Result<()> {
        let response = self
            .http
            .post(format!("{}/api/v2/auth/login", self.endpoint))
            .form(&[("username", username), ("password", password)])
            .send()
            .with_context(|| self.unreachable_hint())?;
        let status = response.status();
        // Grab the SID cookie before consuming the body.
        let sid = response
            .headers()
            .get_all(reqwest::header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .filter_map(|c| c.split(';').next())
            .map(str::trim)
            // Legacy qBt uses `SID`; 5.1+ suffixes the WebUI port
            // (`QBT_SID_8080`) so multiple instances coexist. Match both.
            .find(|kv| {
                let name = kv.split('=').next().unwrap_or_default();
                name == "SID" || name.starts_with("QBT_SID_")
            })
            .map(str::to_string);
        let body = response.text().unwrap_or_default();
        // Older qBt answers 200 + "Fails.", newer answers 401 — treat both
        // as bad credentials.
        if !status.is_success() || body.trim() == "Fails." {
            bail!("qBittorrent login failed — check username/password");
        }
        // A successful login with no SID cookie means the server isn't issuing
        // a session — it's exempting this client from auth ("bypass
        // authentication for clients on localhost" or a whitelisted subnet),
        // even though we sent credentials. Proceed cookieless, exactly like
        // empty-username bypass mode; the version check verifies connectivity,
        // and any request that genuinely needed auth surfaces its own error.
        self.sid = sid;
        Ok(())
    }

    fn with_cookie(&self, builder: reqwest::blocking::RequestBuilder) -> reqwest::blocking::RequestBuilder {
        match &self.sid {
            Some(sid) => builder.header(reqwest::header::COOKIE, sid.clone()),
            None => builder,
        }
    }

    fn fetch_version(&self) -> anyhow::Result<String> {
        let response = self
            .with_cookie(self.http.get(format!("{}/api/v2/app/version", self.endpoint)))
            .send()
            .with_context(|| self.unreachable_hint())?
            .error_for_status()
            .context("qBittorrent rejected the version request")?;
        Ok(response.text().context("reading the version response")?.trim().to_string())
    }

    /// Creates the category if missing. 409 means it already exists (or
    /// qBt considers the name invalid — which then surfaces at add time).
    pub fn ensure_category(&self, name: &str) -> anyhow::Result<()> {
        let response = self
            .with_cookie(self.http.post(format!("{}/api/v2/torrents/createCategory", self.endpoint)))
            .form(&[("category", name), ("savePath", "")])
            .send()
            .with_context(|| self.unreachable_hint())?;
        let status = response.status();
        if status.is_success() || status == reqwest::StatusCode::CONFLICT {
            Ok(())
        } else {
            bail!("creating qBittorrent category \"{name}\" failed: HTTP {status}");
        }
    }

    /// Uploads one .torrent and starts it, grouped under `category`
    /// (autoTMM=true so qBt manages the save path per category).
    pub fn add_torrent(&self, filename: &str, bytes: Vec<u8>, category: &str) -> anyhow::Result<()> {
        let part = reqwest::blocking::multipart::Part::bytes(bytes)
            .file_name(filename.to_string())
            .mime_str("application/x-bittorrent")
            .context("building the torrent upload")?;
        let form = reqwest::blocking::multipart::Form::new()
            .part("torrents", part)
            .text("category", category.to_string())
            .text("autoTMM", "true");
        let response = self
            .with_cookie(self.http.post(format!("{}/api/v2/torrents/add", self.endpoint)))
            .multipart(form)
            .send()
            .with_context(|| self.unreachable_hint())?;
        let status = response.status();
        let body = response.text().unwrap_or_default();
        if !status.is_success() || body.trim() == "Fails." {
            let detail = if body.trim().is_empty() { format!("HTTP {status}") } else { body.trim().to_string() };
            bail!("qBittorrent rejected the torrent: {detail}");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::QbtProfile;
    use std::io::{Read, Write};
    use std::sync::{Arc, Mutex};

    /// Scripted mock: serves one response per incoming connection, in
    /// order, and records each raw request for assertions.
    struct MockQbt {
        endpoint: String,
        requests: Arc<Mutex<Vec<String>>>,
    }

    fn serve_script(responses: Vec<(&'static str, &'static str, &'static str)>) -> MockQbt {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        std::thread::spawn(move || {
            for (status, extra_headers, body) in responses {
                let Ok((mut sock, _)) = listener.accept() else { return };
                sock.set_read_timeout(Some(std::time::Duration::from_millis(200))).unwrap();
                let mut raw = Vec::new();
                let mut buf = [0u8; 8192];
                loop {
                    match sock.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            raw.extend_from_slice(&buf[..n]);
                            // stop once the whole body announced by
                            // Content-Length has arrived (or none is)
                            let text = String::from_utf8_lossy(&raw);
                            if let Some(headers_end) = text.find("\r\n\r\n") {
                                let content_length = text
                                    .lines()
                                    .find_map(|l| l.to_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)));
                                let body_len = raw.len() - (headers_end + 4);
                                match content_length {
                                    Some(cl) if body_len >= cl => break,
                                    None => break,
                                    _ => {}
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }
                recorded.lock().unwrap().push(String::from_utf8_lossy(&raw).into_owned());
                let _ = write!(
                    sock,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{extra_headers}Connection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        MockQbt { endpoint: format!("http://{addr}"), requests }
    }

    fn profile(endpoint: &str, username: &str, password: &str) -> QbtProfile {
        QbtProfile {
            endpoint: endpoint.to_string(),
            username: username.to_string(),
            password: password.to_string(),
        }
    }

    #[test]
    fn connect_with_auth_captures_sid_and_version() {
        let mock = serve_script(vec![
            ("200 OK", "Set-Cookie: SID=abc123; path=/\r\n", "Ok."),
            ("200 OK", "", "v5.0.3"),
        ]);
        let client = QbtClient::connect(&profile(&mock.endpoint, "admin", "pw")).unwrap();
        assert_eq!(client.version(), "v5.0.3");
        let requests = mock.requests.lock().unwrap();
        assert!(requests[0].contains("POST /api/v2/auth/login"));
        assert!(requests[0].contains("username=admin"));
        assert!(requests[1].contains("GET /api/v2/app/version"));
        assert!(requests[1].contains("SID=abc123"), "version request must carry the SID cookie");
    }

    #[test]
    fn connect_captures_modern_qbt_sid_cookie() {
        // qBittorrent 5.1+ names the session cookie QBT_SID_<port> (suffixed
        // with the WebUI port so multiple instances coexist), not the legacy
        // `SID`. It must still be captured and replayed on the version
        // request, or an authenticated qBt answers 403.
        let mock = serve_script(vec![
            ("204 No Content", "Set-Cookie: QBT_SID_8080=EvMwGlN6; HttpOnly; SameSite=Lax; path=/\r\n", ""),
            ("200 OK", "", "v5.2.3.10"),
        ]);
        let client = QbtClient::connect(&profile(&mock.endpoint, "admin", "pw")).unwrap();
        assert_eq!(client.version(), "v5.2.3.10");
        let requests = mock.requests.lock().unwrap();
        assert!(requests[1].contains("GET /api/v2/app/version"));
        assert!(
            requests[1].contains("QBT_SID_8080=EvMwGlN6"),
            "version request must carry the modern per-port session cookie"
        );
    }

    #[test]
    fn connect_bypass_mode_skips_login() {
        let mock = serve_script(vec![("200 OK", "", "v5.0.3")]);
        let client = QbtClient::connect(&profile(&mock.endpoint, "", "")).unwrap();
        assert_eq!(client.version(), "v5.0.3");
        let requests = mock.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].contains("GET /api/v2/app/version"));
    }

    #[test]
    fn login_fails_body_is_friendly_error() {
        let mock = serve_script(vec![("200 OK", "", "Fails.")]);
        let err = QbtClient::connect(&profile(&mock.endpoint, "admin", "bad")).unwrap_err();
        assert!(format!("{err:#}").contains("login failed"), "was: {err:#}");
    }

    #[test]
    fn login_401_is_friendly_error() {
        let mock = serve_script(vec![("401 Unauthorized", "", "")]);
        let err = QbtClient::connect(&profile(&mock.endpoint, "admin", "bad")).unwrap_err();
        assert!(format!("{err:#}").contains("login failed"), "was: {err:#}");
    }

    #[test]
    fn login_ok_without_sid_cookie_uses_bypass() {
        // qBt returns `200 Ok.` with no Set-Cookie when the client is exempt
        // from auth (localhost/whitelisted-subnet bypass) even though we sent
        // credentials. connect() must proceed cookieless, not bail: the
        // version check below still verifies connectivity via the bypass.
        let mock = serve_script(vec![
            ("200 OK", "", "Ok."),
            ("200 OK", "", "v5.0.3"),
        ]);
        let client = QbtClient::connect(&profile(&mock.endpoint, "admin", "pw")).unwrap();
        assert_eq!(client.version(), "v5.0.3");
        let requests = mock.requests.lock().unwrap();
        assert!(requests[0].contains("POST /api/v2/auth/login"));
        assert!(requests[1].contains("GET /api/v2/app/version"));
        assert!(!requests[1].contains("SID="), "no cookie was issued, so none must be sent");
    }

    #[test]
    fn authed_category_and_add_carry_sid_cookie() {
        let mock = serve_script(vec![
            ("200 OK", "Set-Cookie: SID=xyz789; path=/\r\n", "Ok."),
            ("200 OK", "", "v5.0.3"),
            ("200 OK", "", ""),
            ("200 OK", "", "Ok."),
        ]);
        let client = QbtClient::connect(&profile(&mock.endpoint, "admin", "pw")).unwrap();
        client.ensure_category("mikan").unwrap();
        client.add_torrent("x.torrent", b"d0:e".to_vec(), "mikan").unwrap();
        let requests = mock.requests.lock().unwrap();
        assert!(requests[2].contains("SID=xyz789"), "createCategory must carry the SID cookie");
        assert!(requests[3].contains("SID=xyz789"), "torrents/add must carry the SID cookie");
        // minor gaps from review: assert the autoTMM value and the part's mime type
        assert!(requests[3].contains("application/x-bittorrent"));
        let add_body = &requests[3];
        let autotmm_pos = add_body.find("name=\"autoTMM\"").expect("autoTMM field present");
        assert!(add_body[autotmm_pos..].contains("true"), "autoTMM must be true");
    }

    #[test]
    fn ensure_category_accepts_ok_and_conflict() {
        let mock = serve_script(vec![
            ("200 OK", "", "v5.0.3"),
            ("200 OK", "", ""),
            ("409 Conflict", "", ""),
            ("400 Bad Request", "", ""),
        ]);
        let client = QbtClient::connect(&profile(&mock.endpoint, "", "")).unwrap();
        client.ensure_category("mikan").unwrap();
        client.ensure_category("mikan").unwrap();
        assert!(client.ensure_category("").is_err());
        let requests = mock.requests.lock().unwrap();
        assert!(requests[1].contains("POST /api/v2/torrents/createCategory"));
        assert!(requests[1].contains("category=mikan"));
    }

    #[test]
    fn add_torrent_sends_multipart_and_accepts_ok() {
        let mock = serve_script(vec![
            ("200 OK", "", "v5.0.3"),
            ("200 OK", "", "Ok."),
        ]);
        let client = QbtClient::connect(&profile(&mock.endpoint, "", "")).unwrap();
        client.add_torrent("ep [01].torrent", b"d8:announce0:e".to_vec(), "石纪元").unwrap();
        let requests = mock.requests.lock().unwrap();
        let add = &requests[1];
        assert!(add.contains("POST /api/v2/torrents/add"));
        assert!(add.contains("name=\"torrents\""));
        assert!(add.contains("filename=\"ep [01].torrent\""));
        assert!(add.contains("d8:announce0:e"));
        assert!(add.contains("name=\"category\""));
        assert!(add.contains("石纪元"));
        assert!(add.contains("name=\"autoTMM\""));
    }

    #[test]
    fn add_torrent_fails_body_is_error() {
        let mock = serve_script(vec![
            ("200 OK", "", "v5.0.3"),
            ("200 OK", "", "Fails."),
        ]);
        let client = QbtClient::connect(&profile(&mock.endpoint, "", "")).unwrap();
        assert!(client.add_torrent("x.torrent", vec![1], "c").is_err());
    }
}
