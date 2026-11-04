//! Bounded HTTP recording downloads; native camera response quirks are regression-tested.
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::{path::Path, time::Duration};
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;
use url::Url;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HttpResponseProfile {
    #[default]
    Standard,
    MilesightCamera,
}

#[derive(Clone, Deserialize)]
pub struct HttpCredentials {
    pub username: String,
    pub password: String,
}
impl std::fmt::Debug for HttpCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HttpCredentials([redacted])")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    #[error("Invalid HTTP recording URL")]
    InvalidUrl,
    #[error("Recording download failed")]
    Network(#[source] reqwest::Error),
    #[error("Recording server returned HTTP {0}")]
    HttpStatus(u16),
    #[error("Recording exceeds import size limit")]
    TooLarge,
    #[error("Recording download cancelled")]
    Cancelled,
    #[error("Recording response contains no footage")]
    NoFootage,
    #[error("Could not write recording download")]
    Io,
}

pub struct HttpImporter {
    standard: reqwest::Client,
    milesight: reqwest::Client,
    max_bytes: u64,
}
impl HttpImporter {
    pub fn new(max_bytes: u64, timeout: Duration) -> anyhow::Result<Self> {
        anyhow::ensure!(
            max_bytes > 0 && !timeout.is_zero(),
            "Import limits must be positive"
        );
        let builder = || {
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .no_proxy()
                .connect_timeout(Duration::from_secs(10).min(timeout))
                .timeout(timeout)
        };
        Ok(Self {
            standard: builder().build()?,
            // Native exports contain a second HTTP status line inside the outer
            // response's headers. Ignore invalid header lines only for this profile.
            milesight: builder()
                .http1_only()
                .http1_ignore_invalid_headers_in_responses(true)
                .build()?,
            max_bytes,
        })
    }

    pub async fn download(
        &self,
        url: Url,
        creds: Option<&HttpCredentials>,
        profile: HttpResponseProfile,
        destination: &Path,
        cancel: &CancellationToken,
        progress: impl Fn(u64, Option<u64>),
    ) -> Result<u64, DownloadError> {
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || (profile == HttpResponseProfile::MilesightCamera && url.path() != "/dataloader.cgi")
        {
            return Err(DownloadError::InvalidUrl);
        }
        let client = match profile {
            HttpResponseProfile::Standard => &self.standard,
            HttpResponseProfile::MilesightCamera => &self.milesight,
        };
        let mut request = client.get(url);
        if let Some(creds) = creds {
            request = request.basic_auth(&creds.username, Some(&creds.password));
        }
        let response = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(DownloadError::Cancelled),
            response = request.send() => response.map_err(|error| DownloadError::Network(error.without_url()))?,
        };
        if response.status().as_u16() != 200 {
            return Err(DownloadError::HttpStatus(response.status().as_u16()));
        }
        let total = response.content_length();
        if total.is_some_and(|total| total > self.max_bytes) {
            return Err(DownloadError::TooLarge);
        }
        // create_new prevents a failed/retried request from damaging an existing import.
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(destination)
            .await
            .map_err(|_| DownloadError::Io)?;
        let outcome = async {
            let mut stream = response.bytes_stream();
            let mut count = 0u64;
            let mut prefix = Vec::new();
            loop {
                let chunk = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(DownloadError::Cancelled),
                    chunk = stream.next() => chunk,
                };
                let Some(chunk) = chunk else { break };
                let chunk = chunk.map_err(|error| DownloadError::Network(error.without_url()))?;
                count = count
                    .checked_add(chunk.len() as u64)
                    .ok_or(DownloadError::TooLarge)?;
                if count > self.max_bytes {
                    return Err(DownloadError::TooLarge);
                }
                prefix.extend_from_slice(&chunk[..chunk.len().min(64 - prefix.len())]);
                file.write_all(&chunk)
                    .await
                    .map_err(|_| DownloadError::Io)?;
                progress(count, total);
            }
            let prefix = String::from_utf8_lossy(&prefix)
                .trim_start()
                .to_ascii_lowercase();
            if count == 0 || prefix.starts_with("<html") || prefix.starts_with("<!doctype html") {
                return Err(DownloadError::NoFootage);
            }
            file.flush().await.map_err(|_| DownloadError::Io)?;
            Ok(count)
        }
        .await;
        drop(file);
        if outcome.is_err() {
            let _ = tokio::fs::remove_file(destination).await;
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    const CAMERA_RESPONSE: &[u8] = b"HTTP/1.0 200 OK\r\nDate: Wed, 04 Nov 2026 09:06:03 GMT\r\nServer: WebServer\r\nConnection: close\r\nHTTP/1.0 200 OK/nServer: lion\nMime-Version:1.0\nDate: Wed Nov 4 07:18:16 2026\nContent-Type: application/octet-stream/nContent-Length: 5\nConnection: close\n\nvideo";

    async fn server(response: &'static [u8]) -> (Url, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "http://{}/dataloader.cgi?filename=test%20clip",
            listener.local_addr().unwrap()
        )
        .parse()
        .unwrap();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let mut buf = [0; 1024];
                let n = socket.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..n]);
            }
            let _ = socket.write_all(response).await;
            String::from_utf8(request).unwrap()
        });
        (url, task)
    }
    fn path() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("ms-download-{}", uuid::Uuid::new_v4()))
    }
    fn importer(max: u64) -> HttpImporter {
        HttpImporter::new(max, Duration::from_secs(2)).unwrap()
    }
    #[tokio::test]
    async fn native_camera_headers_preserve_the_body_without_a_length() {
        let (url, task) = server(CAMERA_RESPONSE).await;
        let path = path();
        let creds = HttpCredentials {
            username: "admin".into(),
            password: "test-secret".into(),
        };
        let progress = std::sync::Mutex::new(Vec::new());
        let count = importer(100)
            .download(
                url,
                Some(&creds),
                HttpResponseProfile::MilesightCamera,
                &path,
                &CancellationToken::new(),
                |count, total| progress.lock().unwrap().push((count, total)),
            )
            .await
            .unwrap();
        assert_eq!(count, 5);
        assert_eq!(tokio::fs::read(&path).await.unwrap(), b"video");
        assert_eq!(*progress.lock().unwrap(), vec![(5, None)]);
        let request = task.await.unwrap().to_ascii_lowercase();
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD
            .encode("admin:test-secret")
            .to_ascii_lowercase();
        assert!(request.contains(&format!("authorization: basic {encoded}")));
        assert!(!format!("{creds:?}").contains("test-secret"));
        tokio::fs::remove_file(path).await.unwrap();
    }
    #[tokio::test]
    async fn standard_download_tracks_length_and_auth_is_optional() {
        let (url, task) =
            server(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nvideo").await;
        let path = path();
        importer(100)
            .download(
                url,
                None,
                HttpResponseProfile::Standard,
                &path,
                &CancellationToken::new(),
                |_, total| assert_eq!(total, Some(5)),
            )
            .await
            .unwrap();
        assert!(
            !task
                .await
                .unwrap()
                .to_lowercase()
                .contains("authorization:")
        );
        tokio::fs::remove_file(path).await.unwrap();
    }
    #[tokio::test]
    async fn error_and_empty_responses_are_distinct_and_partial_files_are_removed() {
        for (response, expected) in [
            (&b"HTTP/1.0 200 OK\r\nContent-Length: 0\r\n\r\n"[..], "no footage"),
            (&b"HTTP/1.0 200 OK\r\nContent-Length: 6\r\n\r\n<html>"[..], "no footage"),
            (&b"HTTP/1.0 401 Unauthorized\r\nContent-Length: 0\r\n\r\n"[..], "HTTP 401"),
            (&b"HTTP/1.0 302 Found\r\nLocation: http://127.0.0.1:1/\r\nContent-Length: 0\r\n\r\n"[..], "HTTP 302"),
        ] {
            let (url, task) = server(response).await;
            let path = path();
            let error = importer(100).download(url,None, HttpResponseProfile::Standard,
&path,&CancellationToken::new(),|_,_|{}).await.unwrap_err();
            assert!(error.to_string().contains(expected), "{error}");
            assert!(!path.exists());
            task.await.unwrap();
        }
    }
    #[tokio::test]
    async fn size_limits_apply_with_and_without_content_length() {
        for response in [
            &b"HTTP/1.0 200 OK\r\nContent-Length: 5\r\n\r\nvideo"[..],
            &b"HTTP/1.0 200 OK\r\n\r\nvideo"[..],
        ] {
            let (url, task) = server(response).await;
            let path = path();
            assert!(matches!(
                importer(4)
                    .download(
                        url,
                        None,
                        HttpResponseProfile::Standard,
                        &path,
                        &CancellationToken::new(),
                        |_, _| {}
                    )
                    .await,
                Err(DownloadError::TooLarge)
            ));
            assert!(!path.exists());
            task.await.unwrap();
        }
    }
    #[tokio::test]
    async fn cancelled_download_does_not_connect_or_create_a_file() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let path = path();
        assert!(matches!(
            importer(100)
                .download(
                    "http://127.0.0.1:1/".parse().unwrap(),
                    None,
                    HttpResponseProfile::Standard,
                    &path,
                    &cancel,
                    |_, _| {}
                )
                .await,
            Err(DownloadError::Cancelled)
        ));
        assert!(!path.exists());
    }
    #[tokio::test]
    async fn credentials_in_urls_and_non_http_schemes_are_rejected() {
        for url in [
            "http://user:secret@127.0.0.1/dataloader.cgi",
            "ftp://127.0.0.1/dataloader.cgi",
        ] {
            assert!(matches!(
                importer(100)
                    .download(
                        url.parse().unwrap(),
                        None,
                        HttpResponseProfile::Standard,
                        &path(),
                        &CancellationToken::new(),
                        |_, _| {}
                    )
                    .await,
                Err(DownloadError::InvalidUrl)
            ));
        }
    }
    #[tokio::test]
    async fn invalid_nested_status_headers_are_not_ignored_for_standard_imports() {
        let (url, task) = server(CAMERA_RESPONSE).await;
        let path = path();
        assert!(matches!(
            importer(100)
                .download(
                    url,
                    None,
                    HttpResponseProfile::Standard,
                    &path,
                    &CancellationToken::new(),
                    |_, _| {}
                )
                .await,
            Err(DownloadError::Network(_))
        ));
        assert!(!path.exists());
        task.await.unwrap();
    }
}
