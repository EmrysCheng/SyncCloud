//! Local HTTP fixture. No credentials or live provider endpoints are used.
use tokio::io::{AsyncReadExt, AsyncWriteExt};
pub struct Request {
    pub method: String,
    pub path: String,
    pub body: Vec<u8>,
}
pub struct Response {
    pub code: u16,
    pub body: Vec<u8>,
    pub headers: Vec<(String, String)>,
}
impl Response {
    pub fn new(code: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            code,
            body: body.into(),
            headers: Vec::new(),
        }
    }
    pub fn json(body: serde_json::Value) -> Self {
        Self::new(200, serde_json::to_vec(&body).unwrap())
    }
}
pub struct Server {
    pub url: String,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
pub async fn serve(handler: impl Fn(Request) -> Response + Send + Sync + 'static) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let handler = std::sync::Arc::new(handler);
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let handler = handler.clone();
            tokio::spawn(async move {
                let mut headers = Vec::new();
                let mut byte = [0; 1];
                while !headers.ends_with(b"\r\n\r\n") {
                    if socket.read_exact(&mut byte).await.is_err() {
                        return;
                    }
                    headers.push(byte[0]);
                }
                let text = String::from_utf8_lossy(&headers);
                let mut lines = text.lines();
                let first = lines.next().unwrap().split_whitespace().collect::<Vec<_>>();
                let method = first[0].to_string();
                let path = first[1].to_string();
                let fields = lines
                    .filter_map(|l| l.split_once(':'))
                    .map(|(k, v)| (k.to_lowercase(), v.trim().to_string()))
                    .collect::<std::collections::HashMap<_, _>>();
                if fields
                    .get("expect")
                    .is_some_and(|s| s.eq_ignore_ascii_case("100-continue"))
                {
                    socket
                        .write_all(b"HTTP/1.1 100 Continue\r\n\r\n")
                        .await
                        .unwrap();
                }
                let mut body = Vec::new();
                if fields
                    .get("transfer-encoding")
                    .is_some_and(|s| s.contains("chunked"))
                {
                    loop {
                        let mut line = Vec::new();
                        while !line.ends_with(b"\r\n") {
                            socket.read_exact(&mut byte).await.unwrap();
                            line.push(byte[0]);
                        }
                        let line = String::from_utf8(line).unwrap();
                        let size =
                            usize::from_str_radix(line.trim().split(';').next().unwrap(), 16)
                                .unwrap();
                        if size == 0 {
                            break;
                        }
                        let offset = body.len();
                        body.resize(offset + size, 0);
                        socket.read_exact(&mut body[offset..]).await.unwrap();
                        let mut crlf = [0; 2];
                        socket.read_exact(&mut crlf).await.unwrap();
                    }
                } else if let Some(len) = fields.get("content-length").and_then(|s| s.parse().ok())
                {
                    body.resize(len, 0);
                    socket.read_exact(&mut body).await.unwrap();
                }
                if fields
                    .get("content-encoding")
                    .is_some_and(|s| s.contains("aws-chunked"))
                {
                    let mut decoded = Vec::new();
                    let mut rest = body.as_slice();
                    loop {
                        let end = rest.windows(2).position(|v| v == b"\r\n").unwrap();
                        let line = std::str::from_utf8(&rest[..end]).unwrap();
                        let size =
                            usize::from_str_radix(line.split(';').next().unwrap(), 16).unwrap();
                        if size == 0 {
                            break;
                        }
                        rest = &rest[end + 2..];
                        decoded.extend_from_slice(&rest[..size]);
                        rest = &rest[size + 2..];
                    }
                    body = decoded;
                }
                let response = handler(Request {
                    method: method.clone(),
                    path,
                    body,
                });
                let mut header = format!(
                    "HTTP/1.1 {} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n",
                    response.code,
                    response.body.len()
                );
                for (k, v) in response.headers {
                    header.push_str(&format!("{k}: {v}\r\n"));
                }
                header.push_str("\r\n");
                if socket.write_all(header.as_bytes()).await.is_err() {
                    return;
                }
                if method != "HEAD" {
                    let _ = socket.write_all(&response.body).await;
                }
            });
        }
    });
    Server { url, task }
}
