use crate::lsp;
use crate::models;
use crate::MyHandler;
use async_trait::async_trait;
use rust_mcp_sdk::auth::AuthInfo;
use rust_mcp_sdk::error::McpSdkError;
use rust_mcp_sdk::mcp_server::ServerHandler;
use rust_mcp_sdk::schema::{
    CallToolRequestParams, ClientMessage, ContentBlock, InitializeRequestParams, InitializeResult,
    MessageFromServer, NotificationFromServer, RequestId, ServerMessage,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::sync::RwLockReadGuard;
use url::Url;

struct LspTestContext {
    #[allow(dead_code)]
    temp_dir: tempfile::TempDir,
    lsp_client: Arc<Mutex<lsp::LspClient>>,
    handler: MyHandler,
    main_rs_path: std::path::PathBuf,
}

impl LspTestContext {
    async fn setup() -> Self {
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
        let main_rs = r#"fn hello() {
    println!("hello");
}
fn main() {
    let x = 1;
    println!("{}", x);
    hello();
}
"#;
        tokio::fs::create_dir(project_path.join("src")).await.unwrap();
        let main_rs_path = project_path.join("src/main.rs");
        tokio::fs::write(&main_rs_path, main_rs).await.unwrap();

        let root_uri: lsp_types::Uri = Url::from_directory_path(project_path)
            .unwrap()
            .to_string()
            .parse()
            .unwrap();

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

        // Send didOpen
        let content = tokio::fs::read_to_string(&main_rs_path).await.unwrap();
        lsp_client
            .send_notification::<lsp_types::notification::DidOpenTextDocument>(
                lsp_types::DidOpenTextDocumentParams {
                    text_document: lsp_types::TextDocumentItem {
                        uri: Url::from_file_path(&main_rs_path)
                            .unwrap()
                            .to_string()
                            .parse()
                            .unwrap(),
                        language_id: "rust".to_string(),
                        version: 0,
                        text: content,
                    },
                },
            )
            .await
            .unwrap();

        let lsp_client = Arc::new(Mutex::new(lsp_client));
        let handler = MyHandler {
            lsp_client: Some(lsp_client.clone()),
            subscribed_to_diagnostics: std::sync::atomic::AtomicBool::new(false),
            mcp_runtime: Arc::new(Mutex::new(None)),
        };

        // Give rust-analyzer some time to initialize and index
        tokio::time::sleep(tokio::time::Duration::from_secs(15)).await;

        Self {
            temp_dir,
            lsp_client,
            handler,
            main_rs_path,
        }
    }

    async fn teardown(self) {
        let mut client = self.lsp_client.lock().await;
        let _ = client.shutdown().await;
    }

    fn main_rs_path_str(&self) -> String {
        self.main_rs_path.to_string_lossy().to_string()
    }
}

async fn run_lsp_test<F, Fut>(test_fn: F)
where
    F: FnOnce(LspTestContext) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let ctx = LspTestContext::setup().await;
    test_fn(ctx).await;
}

#[tokio::test]
async fn test_editor_get_definition() {
    run_lsp_test(|ctx| async move {
        let mut args = serde_json::Map::new();
        args.insert("path".to_string(), serde_json::json!(ctx.main_rs_path_str()));
        args.insert("line".to_string(), serde_json::json!(5)); // println line
        args.insert("character".to_string(), serde_json::json!(20)); // Cursor on 'x'

        let params = CallToolRequestParams {
            name: "editor_get_definition".to_string(),
            arguments: Some(args),
            meta: None,
            task: None,
        };
        let result = ctx
            .handler
            .handle_call_tool_request(params, Arc::new(MockMcpServer::new()))
            .await
            .unwrap();
        if let ContentBlock::TextContent(text) = &result.content[0] {
            let locations: Vec<models::Location> = serde_json::from_str(&text.text).unwrap();
            assert!(!locations.is_empty(), "Definition not found for 'x'");
            assert!(locations[0].path.contains("main.rs"));
        }
        ctx.teardown().await;
    })
    .await;
}

#[tokio::test]
async fn test_code_show_sub_symbol() {
    run_lsp_test(|ctx| async move {
        let mut args = serde_json::Map::new();
        args.insert("symbol".to_string(), serde_json::json!("main.rs"));
        args.insert("symbolPath".to_string(), serde_json::json!(ctx.main_rs_path_str()));
        args.insert("level".to_string(), serde_json::json!(1));

        let params = CallToolRequestParams {
            name: "code_show_sub_symbol".to_string(),
            arguments: Some(args),
            meta: None,
            task: None,
        };
        let result = ctx
            .handler
            .handle_call_tool_request(params, Arc::new(MockMcpServer::new()))
            .await
            .unwrap();
        if let ContentBlock::TextContent(text) = &result.content[0] {
            let members: Vec<models::SymbolMember> = serde_json::from_str(&text.text).unwrap();
            assert!(
                members.iter().any(|m| m.name.contains("main")),
                "Symbol 'main' not found"
            );
            assert!(
                members.iter().any(|m| m.name.contains("hello")),
                "Symbol 'hello' not found"
            );
        }
        ctx.teardown().await;
    })
    .await;
}

#[tokio::test]
async fn test_code_find_symbol() {
    run_lsp_test(|ctx| async move {
        let mut args = serde_json::Map::new();
        args.insert("symbolName".to_string(), serde_json::json!("hello"));
        args.insert("feelingLucky".to_string(), serde_json::json!(true));

        let params = CallToolRequestParams {
            name: "code_find_symbol".to_string(),
            arguments: Some(args),
            meta: None,
            task: None,
        };
        let result = ctx
            .handler
            .handle_call_tool_request(params, Arc::new(MockMcpServer::new()))
            .await
            .unwrap();
        if let ContentBlock::TextContent(text) = &result.content[0] {
            let locations: Vec<models::Location> = serde_json::from_str(&text.text).unwrap();
            assert!(
                !locations.is_empty(),
                "Symbol 'hello' not found via code_find_symbol"
            );
            assert!(locations[0].path.contains("main.rs"));
        }
        ctx.teardown().await;
    })
    .await;
}

#[tokio::test]
async fn test_editor_get_references() {
    run_lsp_test(|ctx| async move {
        let mut args = serde_json::Map::new();
        args.insert("path".to_string(), serde_json::json!(ctx.main_rs_path_str()));
        args.insert("line".to_string(), serde_json::json!(0)); // line 0 is 'fn hello()'
        args.insert("character".to_string(), serde_json::json!(3)); // inside 'hello'

        let params = CallToolRequestParams {
            name: "editor_get_references".to_string(),
            arguments: Some(args),
            meta: None,
            task: None,
        };
        let result = ctx
            .handler
            .handle_call_tool_request(params, Arc::new(MockMcpServer::new()))
            .await
            .unwrap();
        if let ContentBlock::TextContent(text) = &result.content[0] {
            let locations: Vec<models::Location> = serde_json::from_str(&text.text).unwrap();
            assert!(
                locations.len() >= 2,
                "Expected at least 2 references for 'hello', found {}",
                locations.len()
            );
        }
        ctx.teardown().await;
    })
    .await;
}

#[tokio::test]
async fn test_refactor_interactive_rename() {
    run_lsp_test(|ctx| async move {
        let mut symbol_to_find_args = serde_json::Map::new();
        symbol_to_find_args.insert("symbolName".to_string(), serde_json::json!("hello"));
        symbol_to_find_args.insert(
            "locationHint".to_string(),
            serde_json::json!({"line": 0, "character": 3}),
        );

        let mut args = serde_json::Map::new();
        args.insert("path".to_string(), serde_json::json!(ctx.main_rs_path_str()));
        args.insert(
            "symbolToFind".to_string(),
            serde_json::Value::Object(symbol_to_find_args),
        );
        args.insert("newName".to_string(), serde_json::json!("greet"));

        let params = CallToolRequestParams {
            name: "refactor_interactive_rename".to_string(),
            arguments: Some(args),
            meta: None,
            task: None,
        };
        let result = ctx
            .handler
            .handle_call_tool_request(params, Arc::new(MockMcpServer::new()))
            .await
            .unwrap();
        if let ContentBlock::TextContent(text) = &result.content[0] {
            assert!(text.text.contains("Applied"), "Rename failed: {}", text.text);
        }

        // Verify change on disk
        let new_content = tokio::fs::read_to_string(&ctx.main_rs_path).await.unwrap();
        assert!(
            new_content.contains("fn greet()"),
            "Rename didn't update function definition"
        );
        assert!(
            new_content.contains("greet();"),
            "Rename didn't update function call"
        );
        assert!(
            !new_content.contains("fn hello()"),
            "Old name still exists in definition"
        );
        ctx.teardown().await;
    })
    .await;
}

#[tokio::test]
async fn test_code_get_actions_for_diagnostic() {
    run_lsp_test(|ctx| async move {
        let mut diag_obj = serde_json::Map::new();
        diag_obj.insert(
            "path".to_string(),
            serde_json::json!(ctx.main_rs_path_str()),
        );
        diag_obj.insert(
            "diagnostic".to_string(),
            serde_json::json!({
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
                "message": "dummy diagnostic",
                "severity": 1
            }),
        );

        let mut args = serde_json::Map::new();
        args.insert(
            "diagnosticObject".to_string(),
            serde_json::Value::Object(diag_obj),
        );

        let params = CallToolRequestParams {
            name: "code_get_actions_for_diagnostic".to_string(),
            arguments: Some(args),
            meta: None,
            task: None,
        };
        let result = ctx
            .handler
            .handle_call_tool_request(params, Arc::new(MockMcpServer::new()))
            .await
            .unwrap();
        assert!(!result.is_error.unwrap_or(false));
        ctx.teardown().await;
    })
    .await;
}

#[tokio::test]
async fn test_editor_subscribe_to_diagnostics() {
    run_lsp_test(|ctx| async move {
        let params = CallToolRequestParams {
            name: "editor_subscribe_to_diagnostics".to_string(),
            arguments: Some(serde_json::Map::new()),
            meta: None,
            task: None,
        };
        let result = ctx
            .handler
            .handle_call_tool_request(params, Arc::new(MockMcpServer::new()))
            .await
            .unwrap();

        if let ContentBlock::TextContent(text) = &result.content[0] {
            assert!(text.text.contains("Subscribed"));
        }
        assert!(ctx
            .handler
            .subscribed_to_diagnostics
            .load(std::sync::atomic::Ordering::SeqCst));
        ctx.teardown().await;
    })
    .await;
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
    async fn set_client_details(
        &self,
        _client_details: InitializeRequestParams,
    ) -> std::result::Result<(), McpSdkError> {
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
    async fn update_auth_info(&self, _auth_info: Option<AuthInfo>) {}
    async fn wait_for_initialization(&self) {}
    fn task_store(
        &self,
    ) -> Option<
        Arc<
            dyn rust_mcp_sdk::task_store::TaskStore<
                rust_mcp_sdk::schema::ClientJsonrpcRequest,
                rust_mcp_sdk::schema::ResultFromServer,
            >,
        >,
    > {
        None
    }
    fn client_task_store(
        &self,
    ) -> Option<
        Arc<
            dyn rust_mcp_sdk::task_store::TaskStore<
                rust_mcp_sdk::schema::ServerJsonrpcRequest,
                rust_mcp_sdk::schema::ResultFromClient,
            >,
        >,
    > {
        None
    }
    async fn send_notification(
        &self,
        _notification: NotificationFromServer,
    ) -> std::result::Result<(), McpSdkError> {
        Ok(())
    }
    async fn stderr_message(&self, _message: String) -> std::result::Result<(), McpSdkError> {
        Ok(())
    }
    fn session_id(&self) -> Option<String> {
        None
    }
    async fn send(
        &self,
        _message: MessageFromServer,
        _request_id: Option<RequestId>,
        _timeout: Option<Duration>,
    ) -> std::result::Result<Option<ClientMessage>, McpSdkError> {
        Ok(None)
    }
    async fn send_batch(
        &self,
        _messages: Vec<ServerMessage>,
        _timeout: Option<Duration>,
    ) -> std::result::Result<Option<Vec<ClientMessage>>, McpSdkError> {
        Ok(None)
    }
}
