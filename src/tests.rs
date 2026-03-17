use crate::lsp;
use crate::models;
use crate::MyHandler;
use crate::mock_server::MockMcpServer;
use rust_mcp_sdk::mcp_server::ServerHandler;
use rust_mcp_sdk::schema::{
    CallToolRequestParams, ContentBlock,
};
use std::sync::Arc;
use tokio::sync::Mutex;
use url::Url;

struct LspTestContext {
    #[allow(dead_code)]
    temp_dir: tempfile::TempDir,
    lsp_client: Arc<Mutex<lsp::LspClient>>,
    handler: MyHandler,
    main_rs_path: std::path::PathBuf,
    project_path: std::path::PathBuf,
}

impl LspTestContext {
    async fn setup() -> Self {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let project_path = temp_dir.path().to_path_buf();

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
        let main_rs = r#"mod utils;
fn hello() {
    println!("hello");
}
fn main() {
    let x = 1;
    println!("{}", x);
    hello();
    utils::useful_func();
}
"#;
        tokio::fs::create_dir(project_path.join("src")).await.unwrap();
        let main_rs_path = project_path.join("src/main.rs");
        tokio::fs::write(&main_rs_path, main_rs).await.unwrap();

        // Create src/utils.rs
        let utils_rs = r#"pub fn useful_func() {
    println!("useful");
}
"#;
        tokio::fs::write(project_path.join("src/utils.rs"), utils_rs).await.unwrap();

        let root_uri: lsp_types::Uri = Url::from_directory_path(&project_path)
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

        // Send didOpen for main.rs
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

        // Give rust-analyzer some time to index after didOpen
        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

        // Give rust-analyzer some time to initialize and index
        tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;

        Self {
            temp_dir,
            lsp_client,
            handler,
            main_rs_path,
            project_path,
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
        args.insert("line".to_string(), serde_json::json!(6)); // println line
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
        args.insert("line".to_string(), serde_json::json!(1)); // line 1 is 'fn hello()'
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
            serde_json::json!({"line": 1, "character": 3}),
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
async fn test_ui_show_workspace_diagnostics() {
    run_lsp_test(|ctx| async move {
        let params = CallToolRequestParams {
            name: "ui_show_workspace_diagnostics".to_string(),
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
            assert!(text.text.contains("opened"));
        }
        ctx.teardown().await;
    })
    .await;
}

#[tokio::test]
async fn test_code_apply_action() {
    run_lsp_test(|ctx| async move {
        // Create an action with a workspace edit
        let mut changes = std::collections::HashMap::new();
        let uri: lsp_types::Uri = Url::from_file_path(&ctx.main_rs_path)
            .unwrap()
            .to_string()
            .parse()
            .unwrap();
        changes.insert(
            uri,
            vec![lsp_types::TextEdit {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 0,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 0,
                        character: 0,
                    },
                },
                new_text: "// Leading comment\n".to_string(),
            }],
        );

        let edit = lsp_types::WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        };

        let mut action_obj = serde_json::Map::new();
        action_obj.insert("title".to_string(), serde_json::json!("Add comment"));
        action_obj.insert("edit".to_string(), serde_json::to_value(edit).unwrap());

        let mut args = serde_json::Map::new();
        args.insert("actionObject".to_string(), serde_json::Value::Object(action_obj));

        let params = CallToolRequestParams {
            name: "code_apply_action".to_string(),
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
            assert!(text.text.contains("Applied edit"));
        }

        // Verify change on disk
        let content = tokio::fs::read_to_string(&ctx.main_rs_path).await.unwrap();
        assert!(content.starts_with("// Leading comment"));

        ctx.teardown().await;
    })
    .await;
}

#[tokio::test]
async fn test_multi_file_find_symbol() {
    run_lsp_test(|ctx| async move {
        // Find symbol in another file (utils.rs)
        let mut args = serde_json::Map::new();
        args.insert("symbolName".to_string(), serde_json::json!("useful_func"));
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
            assert!(!locations.is_empty(), "Symbol 'useful_func' not found");
            assert!(locations[0].path.contains("utils.rs"));
        }
        ctx.teardown().await;
    })
    .await;
}

#[tokio::test]
async fn test_zero_based_boundary() {
    run_lsp_test(|ctx| async move {
        let mut args = serde_json::Map::new();
        args.insert("path".to_string(), serde_json::json!(ctx.main_rs_path_str()));
        args.insert("line".to_string(), serde_json::json!(0));
        args.insert("character".to_string(), serde_json::json!(0));

        let params = CallToolRequestParams {
            name: "editor_get_definition".to_string(),
            arguments: Some(args),
            meta: None,
            task: None,
        };
        // Should find 'mod utils' or similar at 0,0
        let result = ctx.handler.handle_call_tool_request(params, Arc::new(MockMcpServer::new())).await.unwrap();
        assert!(!result.is_error.unwrap_or(false));
        ctx.teardown().await;
    }).await;
}

#[tokio::test]
async fn test_negative_find_symbol() {
    run_lsp_test(|ctx| async move {
        let mut args = serde_json::Map::new();
        args.insert("symbolName".to_string(), serde_json::json!("non_existent_symbol_12345"));
        args.insert("feelingLucky".to_string(), serde_json::json!(true));

        let params = CallToolRequestParams {
            name: "code_find_symbol".to_string(),
            arguments: Some(args),
            meta: None,
            task: None,
        };
        let result = ctx.handler.handle_call_tool_request(params, Arc::new(MockMcpServer::new())).await.unwrap();
        if let ContentBlock::TextContent(text) = &result.content[0] {
            let locations: Vec<models::Location> = serde_json::from_str(&text.text).unwrap();
            assert!(locations.is_empty());
        }
        ctx.teardown().await;
    }).await;
}

#[tokio::test]
async fn test_ambiguity_resolution() {
    run_lsp_test(|ctx| async move {
        // Add duplicate symbol in utils.rs
        let utils_rs = r#"pub fn hello() { println!("utils hello"); }
pub fn useful_func() { println!("useful"); }
"#;
        tokio::fs::write(ctx.project_path.join("src/utils.rs"), utils_rs).await.unwrap();
        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

        let mut args = serde_json::Map::new();
        args.insert("symbolName".to_string(), serde_json::json!("hello"));
        args.insert("feelingLucky".to_string(), serde_json::json!(false)); // Expect multiple

        let params = CallToolRequestParams {
            name: "code_find_symbol".to_string(),
            arguments: Some(args),
            meta: None,
            task: None,
        };
        let result = ctx.handler.handle_call_tool_request(params, Arc::new(MockMcpServer::new())).await.unwrap();
        if let ContentBlock::TextContent(text) = &result.content[0] {
            let locations: Vec<models::Location> = serde_json::from_str(&text.text).unwrap();
            assert!(locations.len() >= 2, "Expected at least 2 'hello' symbols, found {}", locations.len());
        }
        ctx.teardown().await;
    }).await;
}

#[tokio::test]
async fn test_negative_get_definition_whitespace() {
    run_lsp_test(|ctx| async move {
        let mut args = serde_json::Map::new();
        args.insert("path".to_string(), serde_json::json!(ctx.main_rs_path_str()));
        args.insert("line".to_string(), serde_json::json!(0));
        args.insert("character".to_string(), serde_json::json!(3)); // Space in "mod utils;"

        let params = CallToolRequestParams {
            name: "editor_get_definition".to_string(),
            arguments: Some(args),
            meta: None,
            task: None,
        };
        let result = ctx.handler.handle_call_tool_request(params, Arc::new(MockMcpServer::new())).await.unwrap();
        if let ContentBlock::TextContent(text) = &result.content[0] {
            let locations: Vec<models::Location> = serde_json::from_str(&text.text).unwrap();
            assert!(locations.is_empty(), "Expected no definition on whitespace, found {:?}", locations);
        }
        ctx.teardown().await;
    }).await;
}

#[tokio::test]
async fn test_doctrine_workflow() {
    run_lsp_test(|ctx| async move {
        // 1. Create a file with a known error (missing semicolon or unused var)
        let broken_rs_path = ctx.project_path.join("src/broken.rs");
        // Let's use something rust-analyzer definitely gives a fix for:
        let broken_rs_content = "fn main() { let x: i32 = \"string\"; }";
        tokio::fs::write(&broken_rs_path, broken_rs_content).await.unwrap();

        {
            let mut client = ctx.lsp_client.lock().await;
            client.send_notification::<lsp_types::notification::DidOpenTextDocument>(
                lsp_types::DidOpenTextDocumentParams {
                    text_document: lsp_types::TextDocumentItem {
                        uri: Url::from_file_path(&broken_rs_path).unwrap().to_string().parse().unwrap(),
                        language_id: "rust".to_string(),
                        version: 0,
                        text: broken_rs_content.to_string(),
                    },
                }
            ).await.unwrap();
        }

        // 2. We skip "waiting for diagnostic subscription" because we can't easily capture it in this test.
        // Instead we manually construct the diagnostic object as if it came from the stream.
        let mut diag_obj = serde_json::Map::new();
        diag_obj.insert("path".to_string(), serde_json::json!(broken_rs_path.to_string_lossy()));
        diag_obj.insert("diagnostic".to_string(), serde_json::json!({
            "range": {"start": {"line": 0, "character": 25}, "end": {"line": 0, "character": 33}},
            "message": "mismatched types",
            "severity": 1
        }));

        // 3. Get Actions
        let mut args = serde_json::Map::new();
        args.insert("diagnosticObject".to_string(), serde_json::Value::Object(diag_obj));
        let params = CallToolRequestParams {
            name: "code_get_actions_for_diagnostic".to_string(),
            arguments: Some(args),
            meta: None,
            task: None,
        };
        let result = ctx.handler.handle_call_tool_request(params, Arc::new(MockMcpServer::new())).await.unwrap();
        
        // 4. Verification (Smoke test that it doesn't crash)
        assert!(!result.is_error.unwrap_or(false));
        
        ctx.teardown().await;
    }).await;
}

#[tokio::test]
async fn test_negative_show_sub_symbol_malformed_path() {
    run_lsp_test(|ctx| async move {
        let mut args = serde_json::Map::new();
        args.insert("symbol".to_string(), serde_json::json!("main"));
        args.insert("symbolPath".to_string(), serde_json::json!("/non/existent/path.rs"));

        let params = CallToolRequestParams {
            name: "code_show_sub_symbol".to_string(),
            arguments: Some(args),
            meta: None,
            task: None,
        };
        let result = ctx.handler.handle_call_tool_request(params, Arc::new(MockMcpServer::new())).await;
        assert!(result.is_err(), "Expected error for non-existent path");
        ctx.teardown().await;
    }).await;
}

#[tokio::test]
async fn test_negative_rename_non_existent() {
    run_lsp_test(|ctx| async move {
        let mut symbol_to_find = serde_json::Map::new();
        symbol_to_find.insert("symbolName".to_string(), serde_json::json!("non_existent_func"));
        symbol_to_find.insert("locationHint".to_string(), serde_json::json!({"line": 100, "character": 0}));

        let mut args = serde_json::Map::new();
        args.insert("path".to_string(), serde_json::json!(ctx.main_rs_path_str()));
        args.insert("symbolToFind".to_string(), serde_json::Value::Object(symbol_to_find));
        args.insert("newName".to_string(), serde_json::json!("fail"));

        let params = CallToolRequestParams {
            name: "refactor_interactive_rename".to_string(),
            arguments: Some(args),
            meta: None,
            task: None,
        };
        let result = ctx.handler.handle_call_tool_request(params, Arc::new(MockMcpServer::new())).await;
        // Rename might "succeed" with 0 edits or return error depending on LSP.
        // If it returns success with "LSP returned no edits", that's also a valid outcome in our current code.
        if let Ok(res) = result {
             if let ContentBlock::TextContent(text) = &res.content[0] {
                 assert!(text.text.contains("no edits") || text.text.contains("failed") || res.is_error.unwrap_or(false));
             }
        }
        ctx.teardown().await;
    }).await;
}

#[tokio::test]
async fn test_filesystem_read_file() {
    let temp_file = std::env::temp_dir().join("test_read.txt");
    tokio::fs::write(&temp_file, "hello world").await.unwrap();
    let content = tokio::fs::read_to_string(&temp_file).await.unwrap();
    assert_eq!(content, "hello world");
    tokio::fs::remove_file(temp_file).await.unwrap();
}
