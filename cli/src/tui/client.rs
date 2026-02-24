use std::fmt;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::protocol::{Request, Response};
use crate::status::StatusReport;
use super::app::{Command, Message};

const MAX_RESPONSE_BYTES: u64 = 64 * 1024;

#[derive(Debug)]
pub enum DaemonError {
    Connect(io::Error),
    Write(io::Error),
    Read(io::Error),
    Timeout,
    Parse(String),
    Daemon(String),
}

impl fmt::Display for DaemonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DaemonError::Connect(e) => write!(f, "connect: {e}"),
            DaemonError::Write(e) => write!(f, "write: {e}"),
            DaemonError::Read(e) => write!(f, "read: {e}"),
            DaemonError::Timeout => write!(f, "timeout"),
            DaemonError::Parse(e) => write!(f, "parse: {e}"),
            DaemonError::Daemon(e) => write!(f, "daemon: {e}"),
        }
    }
}

pub struct DaemonClient {
    socket_path: PathBuf,
    timeout: Duration,
}

impl DaemonClient {
    pub fn new(socket_path: PathBuf, timeout: Duration) -> Self {
        Self {
            socket_path,
            timeout,
        }
    }

    pub async fn ping(&self) -> Result<(), DaemonError> {
        let payload = self.send_request(&Request::Ping).await?;
        let _ = payload;
        Ok(())
    }

    pub async fn status(&self) -> Result<StatusReport, DaemonError> {
        let payload = self.send_request(&Request::Status).await?;
        let data = payload
            .data
            .ok_or_else(|| DaemonError::Parse("missing data in status response".to_string()))?;
        serde_json::from_value(data).map_err(|e| DaemonError::Parse(e.to_string()))
    }

    async fn send_request(
        &self,
        request: &Request,
    ) -> Result<crate::protocol::OkPayload, DaemonError> {
        let stream = tokio::time::timeout(
            self.timeout,
            UnixStream::connect(&self.socket_path),
        )
        .await
        .map_err(|_| DaemonError::Timeout)?
        .map_err(DaemonError::Connect)?;

        let (reader, mut writer) = stream.into_split();

        let req = serde_json::to_string(request).unwrap();
        tokio::time::timeout(
            self.timeout,
            writer.write_all(format!("{req}\n").as_bytes()),
        )
        .await
        .map_err(|_| DaemonError::Timeout)?
        .map_err(DaemonError::Write)?;

        let mut buf_reader = BufReader::new(reader.take(MAX_RESPONSE_BYTES));
        let mut line = String::new();
        tokio::time::timeout(self.timeout, buf_reader.read_line(&mut line))
            .await
            .map_err(|_| DaemonError::Timeout)?
            .map_err(DaemonError::Read)?;

        let response: Response = serde_json::from_str(line.trim())
            .map_err(|e| DaemonError::Parse(e.to_string()))?;
        response.into_result().map_err(DaemonError::Daemon)
    }
}

pub async fn daemon_worker(
    client: DaemonClient,
    mut cmd_rx: tokio::sync::mpsc::Receiver<Command>,
    msg_tx: std::sync::mpsc::Sender<Message>,
) {
    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            Command::None => {}
            Command::Ping { request_id } => {
                let result = client.ping().await.map_err(|e| e.to_string());
                if msg_tx
                    .send(Message::PingResult { request_id, result })
                    .is_err()
                {
                    break;
                }
            }
            Command::FetchStatus { request_id } => {
                let result = client.status().await.map_err(|e| e.to_string());
                if msg_tx
                    .send(Message::StatusResult { request_id, result })
                    .is_err()
                {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::os::unix::net::UnixListener;

    fn mock_server(dir: &std::path::Path, response: &str) -> PathBuf {
        let sock = dir.join("daemon.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let resp = response.to_string();
        std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                let mut reader =
                    std::io::BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                std::io::BufRead::read_line(&mut reader, &mut line).unwrap();
                let mut writer = stream;
                writeln!(writer, "{resp}").unwrap();
            }
        });
        sock
    }

    #[tokio::test]
    async fn ping_ok() {
        let dir = tempfile::tempdir().unwrap();
        let sock = mock_server(dir.path(), r#"{"status":"ok"}"#);
        let client = DaemonClient::new(sock, Duration::from_secs(2));
        let result = client.ping().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn ping_daemon_error() {
        let dir = tempfile::tempdir().unwrap();
        let sock = mock_server(dir.path(), r#"{"error":"invalid request"}"#);
        let client = DaemonClient::new(sock, Duration::from_secs(2));
        let result = client.ping().await;
        match result {
            Err(DaemonError::Daemon(msg)) => assert_eq!(msg, "invalid request"),
            other => panic!("expected Daemon error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn connect_failure() {
        let client = DaemonClient::new(
            PathBuf::from("/nonexistent/daemon.sock"),
            Duration::from_secs(2),
        );
        let result = client.ping().await;
        assert!(matches!(result, Err(DaemonError::Connect(_))));
    }

    #[tokio::test]
    async fn parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let sock = mock_server(dir.path(), "not json at all");
        let client = DaemonClient::new(sock, Duration::from_secs(2));
        let result = client.ping().await;
        assert!(matches!(result, Err(DaemonError::Parse(_))));
    }

    #[tokio::test]
    async fn status_ok() {
        let dir = tempfile::tempdir().unwrap();
        let report_json = serde_json::json!({
            "schema_version": 1,
            "mount_point": "/mnt/storage",
            "status_code": "healthy",
            "status": "healthy",
            "total_devices": 2,
            "present_count": 2,
            "missing_count": 0,
            "profile": "RAID1",
            "last_scrub": "never",
            "disks": []
        });
        let resp = serde_json::json!({"status": "ok", "data": report_json});
        let sock = mock_server(dir.path(), &resp.to_string());
        let client = DaemonClient::new(sock, Duration::from_secs(2));
        let result = client.status().await;
        let report = result.unwrap();
        assert_eq!(report.mount_point, "/mnt/storage");
        assert_eq!(report.status_code, crate::status::StatusCode::Healthy);
    }

    #[tokio::test]
    async fn status_daemon_error() {
        let dir = tempfile::tempdir().unwrap();
        let sock = mock_server(dir.path(), r#"{"error":"config: not found"}"#);
        let client = DaemonClient::new(sock, Duration::from_secs(2));
        let result = client.status().await;
        match result {
            Err(DaemonError::Daemon(msg)) => assert!(msg.contains("config")),
            other => panic!("expected Daemon error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn status_missing_data() {
        let dir = tempfile::tempdir().unwrap();
        let sock = mock_server(dir.path(), r#"{"status":"ok"}"#);
        let client = DaemonClient::new(sock, Duration::from_secs(2));
        let result = client.status().await;
        match result {
            Err(DaemonError::Parse(msg)) => assert!(msg.contains("missing data")),
            other => panic!("expected Parse error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn timeout_no_response() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("daemon.sock");
        let _listener = UnixListener::bind(&sock).unwrap();
        // Listener accepts nothing — connection succeeds but no response
        let client = DaemonClient::new(sock, Duration::from_millis(100));
        let result = client.ping().await;
        assert!(matches!(result, Err(DaemonError::Timeout)));
    }
}
