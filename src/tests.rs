use crate::lsp;
use crate::models;
use crate::MyHandler;
use rust_mcp_sdk::mcp_server::ServerHandler;
use rust_mcp_sdk::schema::{CallToolRequestParams, ContentBlock, NotificationFromServer};
use std::sync::Arc;
use tokio::sync::Mutex;
use url::Url;
use async_trait::async_trait;
use rust_mcp_sdk::error::McpSdkError;
use rust_mcp_sdk::auth::AuthInfo;
use rust_mcp_sdk::schema::{InitializeRequestParams, InitializeResult, MessageFromServer, RequestId, ClientMessage, ServerMessage};
use std::time::Duration;
use tokio::sync::RwLockReadGuard;

async fn setup_test_project() -> (tempfile::TempDir, lsp_types::Uri) {
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let project_path = temp_dir.path();

    // Create Cargo.toml
    let cargo_toml = r#"[package]
name = "test-project"
version = "0.1.0"
edition = "2021"
"#;
    tokio::fs::write(project_path.join("Cargo.toml"), cargo_toml)
        .await
        .unwrap();

    // Create src/main.rs
    let main_rs = r#"fn main() {
    let x = 1;
    println!("{}", x);
}

fn hello() {
    println!("hello");
}
"#;
    tokio::fs::create_dir(project_path.join("src")).await.unwrap();
    tokio::fs::write(project_path.join("src/main.rs"), main_rs)
        .await
        .unwrap();

    let root_uri: lsp_types::Uri = Url::from_directory_path(project_path)
        .unwrap()
        .to_string()
        .parse()
        .unwrap();
    (temp_dir, root_uri)
}

#[tokio::test]
async fn test_lsp_tools_integration() {
    let (_temp_dir, root_uri) = setup_test_project().await;
    let (notification_tx, mut notification_rx) = tokio::sync::mpsc::channel(100);

    // Drain notifications to prevent blocking
    tokio::spawn(async move {
        while let Some(_notif) = notification_rx.recv().await {}
    });

    let mut lsp_client = lsp::LspClient::start(
        "rust-analyzer",
        &[],
        notification_tx,
        Some(root_uri.clone()),
    )
    .await
    .expect("Failed to start rust-analyzer");

    let main_rs_path = _temp_dir.path().join("src/main.rs");
    let main_rs_path_str = main_rs_path.to_string_lossy().to_string();

    // Send didOpen to make rust-analyzer aware of the file
    let content = tokio::fs::read_to_string(&main_rs_path).await.unwrap();
    lsp_client
        .send_notification::<lsp_types::notification::DidOpenTextDocument>(
            lsp_types::DidOpenTextDocumentParams {
                text_document: lsp_types::TextDocumentItem {
                    uri: Url::from_file_path(&main_rs_path).unwrap().to_string().parse().unwrap(),
                    language_id: "rust".to_string(),
                    version: 0,
                    text: content,
                },
            },
        )
        .await
        .unwrap();

    let handler = MyHandler {
        lsp_client: Some(Arc::new(Mutex::new(lsp_client))),
        subscribed_to_diagnostics: std::sync::atomic::AtomicBool::new(false),
        mcp_runtime: Arc::new(Mutex::new(None)),
    };

    // Give rust-analyzer some time to initialize and index
    tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;

    // Test editor_get_definition
    let mut args = serde_json::Map::new();
    args.insert("path".to_string(), serde_json::json!(main_rs_path_str));
    args.insert("line".to_string(), serde_json::json!(2));
    args.insert("character".to_string(), serde_json::json!(20)); // Cursor on 'x' in println

    let params = CallToolRequestParams {
        name: "editor_get_definition".to_string(),
        arguments: Some(args),
        meta: None,
        task: None,
    };
    let result = handler
        .handle_call_tool_request(params, Arc::new(MockMcpServer::new()))
        .await
        .unwrap();
    if let ContentBlock::TextContent(text) = &result.content[0] {
        let locations: Vec<models::Location> = serde_json::from_str(&text.text).unwrap();
        assert!(!locations.is_empty());
        assert!(locations[0].path.contains("main.rs"));
    }

    // Test code_show_sub_symbol
    let mut args = serde_json::Map::new();
    args.insert("symbol".to_string(), serde_json::json!("main"));
    args.insert("symbolPath".to_string(), serde_json::json!(main_rs_path_str));
    args.insert("level".to_string(), serde_json::json!(1));

    let params = CallToolRequestParams {
        name: "code_show_sub_symbol".to_string(),
        arguments: Some(args),
        meta: None,
        task: None,
    };
    let result = handler
        .handle_call_tool_request(params, Arc::new(MockMcpServer::new()))
        .await
        .unwrap();
    if let ContentBlock::TextContent(text) = &result.content[0] {
        let members: Vec<models::SymbolMember> = serde_json::from_str(&text.text).unwrap();
        assert!(!members.is_empty());
        assert!(members.iter().any(|m| m.name.contains("main")));
        assert!(members.iter().any(|m| m.name.contains("hello")));
    }

    // Test code_find_symbol
    let mut args = serde_json::Map::new();
    args.insert("symbolName".to_string(), serde_json::json!("hello"));
    args.insert("feelingLucky".to_string(), serde_json::json!(true));

    let params = CallToolRequestParams {
        name: "code_find_symbol".to_string(),
        arguments: Some(args),
        meta: None,
        task: None,
    };
    let result = handler
        .handle_call_tool_request(params, Arc::new(MockMcpServer::new()))
        .await
        .unwrap();
    if let ContentBlock::TextContent(text) = &result.content[0] {
        let locations: Vec<models::Location> = serde_json::from_str(&text.text).unwrap();
        assert!(!locations.is_empty());
        assert!(locations[0].path.contains("main.rs"));
    }
}

#[tokio::test]
async fn test_filesystem_read_file() {
    let temp_file = std::env::temp_dir().join("test_read.txt");
    tokio::fs::write(&temp_file, "hello world").await.unwrap();
    let content = tokio::fs::read_to_string(&temp_file).await.unwrap();
    assert_eq!(content, "hello world");
    tokio::fs::remove_file(temp_file).await.unwrap();
}

struct MockMcpServer {
    info: InitializeResult,
}
impl MockMcpServer {
    fn new() -> Self {
        Self {
            info: InitializeResult {
                protocol_version: "2024-11-05".to_string(),
                capabilities: Default::default(),
                server_info: rust_mcp_sdk::schema::Implementation {
                    name: "mock".into(),
                    version: "0.1.0".into(),
                    description: None,
                    icons: vec![],
                    title: None,
                    website_url: None,
                },
                instructions: None,
                meta: None,
            },
        }
    }
}

#[async_trait]
impl rust_mcp_sdk::McpServer for MockMcpServer {
    async fn start(self: Arc<Self>) -> std::result::Result<(), McpSdkError> {
        Ok(())
    }
    async fn set_client_details(&self, _client_details: InitializeRequestParams) -> std::result::Result<(), McpSdkError> {
        Ok(())
    }
    fn server_info(&self) -> &InitializeResult {
        &self.info
    }
    fn client_info(&self) -> Option<InitializeRequestParams> {
        None
    }
    async fn auth_info(&self) -> RwLockReadGuard<'_, Option<AuthInfo>> {
        todo!()
    }
    async fn auth_info_cloned(&self) -> Option<AuthInfo> {
        None
    }
    async fn update_auth_info(&self, _auth_info: Option<AuthInfo>) {
    }
    async fn wait_for_initialization(&self) {
    }
    fn task_store(&self) -> Option<Arc<dyn rust_mcp_sdk::task_store::TaskStore<rust_mcp_sdk::schema::ClientJsonrpcRequest, rust_mcp_sdk::schema::ResultFromServer>>> {
        None
    }
    fn client_task_store(&self) -> Option<Arc<dyn rust_mcp_sdk::task_store::TaskStore<rust_mcp_sdk::schema::ServerJsonrpcRequest, rust_mcp_sdk::schema::ResultFromClient>>> {
        None
    }
    async fn send_notification(&self, _notification: NotificationFromServer) -> std::result::Result<(), McpSdkError> {
        Ok(())
    }
    async fn stderr_message(&self, _message: String) -> std::result::Result<(), McpSdkError> {
        Ok(())
    }
    fn session_id(&self) -> Option<String> {
        None
    }
    async fn send(&self, _message: MessageFromServer, _request_id: Option<RequestId>, _timeout: Option<Duration>) -> std::result::Result<Option<ClientMessage>, McpSdkError> {
        Ok(None)
    }
    async fn send_batch(&self, _messages: Vec<ServerMessage>, _timeout: Option<Duration>) -> std::result::Result<Option<Vec<ClientMessage>>, McpSdkError> {
        Ok(None)
    }
}
