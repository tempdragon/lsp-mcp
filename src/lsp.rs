use anyhow::{Result, anyhow};
use lsp_types::{ClientCapabilities, InitializeParams, request::Request};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

pub struct LspClient {
    tx: mpsc::Sender<(Value, oneshot::Sender<Result<Value>>)>,
}

impl LspClient {
    pub async fn start(
        command: &str,
        args: &[&str],
        notification_tx: mpsc::Sender<Value>,
    ) -> Result<Self> {
        let mut child = Command::new(command)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;

        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("Stdin not available"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("Stdout not available"))?;
        let mut stdout_reader = BufReader::new(stdout);

        let (tx, mut rx) = mpsc::channel::<(Value, oneshot::Sender<Result<Value>>)>(100);

        tokio::spawn(async move {
            let mut pending_requests = HashMap::new();
            let mut request_id = 0;

            loop {
                tokio::select! {
                    Some((mut request, reply_tx)) = rx.recv() => {
                        if request.get("id").is_none() && request.get("method").is_some() {
                             // It's a request (or notification from client), assign ID if not notification
                             if request.get("id").is_none() && !Self::is_notification(&request) {
                                request_id += 1;
                                request["id"] = json!(request_id);
                                let id_key = request_id.to_string();
                                pending_requests.insert(id_key, reply_tx);
                             }
                        }

                        let body = serde_json::to_string(&request).unwrap();
                        let content = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
                        if let Err(e) = stdin.write_all(content.as_bytes()).await {
                            eprintln!("LSP stdin write error: {}", e);
                            break;
                        }
                        if let Err(e) = stdin.flush().await {
                            eprintln!("LSP stdin flush error: {}", e);
                            break;
                        }
                    }
                    line_result = Self::read_message(&mut stdout_reader) => {
                        match line_result {
                            Ok(Some(response)) => {
                                if let Some(id) = response.get("id") {
                                    let id_key = id.to_string();
                                    if let Some(reply_tx) = pending_requests.remove(&id_key) {
                                        let result = if let Some(error) = response.get("error") {
                                            Err(anyhow!("LSP error: {}", error))
                                        } else {
                                            Ok(response.get("result").cloned().unwrap_or(Value::Null))
                                        };
                                        let _ = reply_tx.send(result);
                                    }
                                } else {
                                    // It's a notification from the server
                                    let _ = notification_tx.send(response).await;
                                }
                            }
                            Ok(None) => break,
                            Err(e) => {
                                eprintln!("LSP stdout read error: {}", e);
                                break;
                            }
                        }
                    }
                }
            }
        });

        let mut client = Self { tx };

        // Initialize
        #[allow(deprecated)]
        let params = InitializeParams {
            process_id: Some(std::process::id()),
            root_uri: None,
            root_path: None,
            initialization_options: None,
            capabilities: ClientCapabilities::default(),
            trace: None,
            workspace_folders: None,
            client_info: None,
            locale: None,
            work_done_progress_params: Default::default(),
        };

        client
            .send_request::<lsp_types::request::Initialize>(params)
            .await?;
        // Notify initialized
        client
            .send_notification::<lsp_types::notification::Initialized>(
                lsp_types::InitializedParams {},
            )
            .await?;

        Ok(client)
    }

    fn is_notification(value: &Value) -> bool {
        value.get("id").is_none() && value.get("method").is_some()
    }

    async fn read_message<R: AsyncBufReadExt + Unpin>(reader: &mut R) -> Result<Option<Value>> {
        let mut line = String::new();
        let mut content_length = 0;

        loop {
            line.clear();
            if reader.read_line(&mut line).await? == 0 {
                return Ok(None);
            }
            if line == "\r\n" {
                break;
            }
            if let Some(stripped) = line.strip_prefix("Content-Length: ") {
                content_length = stripped.trim().parse()?;
            }
        }

        if content_length == 0 {
            return Err(anyhow!("Invalid Content-Length"));
        }

        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body).await?;
        let json: Value = serde_json::from_slice(&body)?;
        Ok(Some(json))
    }

    pub async fn send_request<R: Request>(&mut self, params: R::Params) -> Result<R::Result> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let request = json!({
            "jsonrpc": "2.0",
            "method": R::METHOD,
            "params": params,
        });

        self.tx
            .send((request, reply_tx))
            .await
            .map_err(|_| anyhow!("LSP channel closed"))?;
        let result = reply_rx.await??;
        Ok(serde_json::from_value(result)?)
    }

    pub async fn send_notification<N: lsp_types::notification::Notification>(
        &mut self,
        params: N::Params,
    ) -> Result<()> {
        let request = json!({
            "jsonrpc": "2.0",
            "method": N::METHOD,
            "params": params,
        });

        // Use a dummy channel since notifications don't have replies
        let (reply_tx, _) = oneshot::channel();
        self.tx
            .send((request, reply_tx))
            .await
            .map_err(|_| anyhow!("LSP channel closed"))?;
        Ok(())
    }
}
