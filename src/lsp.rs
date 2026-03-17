use anyhow::{Result, anyhow};
use lsp_types::{ClientCapabilities, InitializeParams, request::Request};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

pub struct LspClient {
    tx: mpsc::Sender<(Value, Option<oneshot::Sender<Result<Value>>>)>,
}

impl LspClient {
    pub async fn start(
        command: &str,
        args: &[&str],
        notification_tx: mpsc::Sender<Value>,
        root_uri: Option<lsp_types::Uri>,
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

        let (tx, mut rx) = mpsc::channel::<(Value, Option<oneshot::Sender<Result<Value>>>)>(100);
        let (reader_tx, mut reader_rx) = mpsc::channel::<Result<Option<Value>>>(100);

        tokio::spawn(async move {
            loop {
                let result = Self::read_message(&mut stdout_reader).await;
                let is_err_or_none = matches!(result, Err(_) | Ok(None));
                if reader_tx.send(result).await.is_err() {
                    break;
                }
                if is_err_or_none {
                    break;
                }
            }
        });

        tokio::spawn(async move {
            let mut pending_requests = HashMap::new();
            let mut request_id = 0;

            loop {
                tokio::select! {
                    Some((mut request, reply_tx_opt)) = rx.recv() => {
                        if let Some(reply_tx) = reply_tx_opt {
                            request_id += 1;
                            request["id"] = json!(request_id);
                            let id_key = request_id.to_string();
                            pending_requests.insert(id_key, reply_tx);
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
                    Some(line_result) = reader_rx.recv() => {
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

        let params = InitializeParams {
            process_id: Some(std::process::id()),
            root_uri: None,
            root_path: None,
            initialization_options: None,
            capabilities: ClientCapabilities::default(),
            trace: None,
            workspace_folders: root_uri.map(|uri| vec![lsp_types::WorkspaceFolder {
                uri: uri.clone(),
                name: "workspace".to_string(),
            }]),
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
            .send((request, Some(reply_tx)))
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

        self.tx
            .send((request, None))
            .await
            .map_err(|_| anyhow!("LSP channel closed"))?;
        Ok(())
    }
}
