mod lsp;
mod models;
mod sessions;
mod terminals;

#[cfg(test)]
mod tests;

use anyhow::Result;
use async_trait::async_trait;
use rust_mcp_sdk::McpServer;
use rust_mcp_sdk::StdioTransport;
use rust_mcp_sdk::TransportOptions;
use rust_mcp_sdk::mcp_server::server_runtime::create_server;
use rust_mcp_sdk::mcp_server::{McpServerOptions, ServerHandler};
use rust_mcp_sdk::schema::{
    CallToolError, CallToolRequestParams, CallToolResult, ContentBlock, GetPromptRequestParams,
    GetPromptResult, Implementation, InitializeResult, ListPromptsResult, ListToolsResult,
    PaginatedRequestParams, Prompt, PromptMessage, RpcError, TextContent, Tool,
};
use schemars::JsonSchema;
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct FindSymbolArgs {
    symbol_name: String,
    file: Option<String>,
    location_hint: Option<models::Position>,
    context_hint: Option<String>,
    #[serde(default = "default_hover_detail")]
    hover_detail: String,
    #[serde(default = "default_true")]
    feeling_lucky: bool,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct PathLineCharArgs {
    path: String,
    line: u32,
    character: u32,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct ShowSubSymbolArgs {
    symbol: String,
    symbol_path: String,
    #[serde(default = "default_one")]
    level: u32,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct StartSessionArgs {
    description: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct ProposeChangeArgs {
    session_id: String,
    change_object: serde_json::Value,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct SessionIdArgs {
    session_id: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct CreateTerminalArgs {
    name: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct RunCommandArgs {
    terminal_id: String,
    command: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct TerminalIdArgs {
    terminal_id: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct GetActionsArgs {
    diagnostic_object: models::EnrichedDiagnostic,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct ApplyActionArgs {
    action_object: models::CodeAction,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct InteractiveRenameArgs {
    path: String,
    symbol_to_find: FindSymbolArgs,
    new_name: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct ApproveChangeArgs {
    session_id: String,
    proposal_id: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct ReadFileArgs {
    path: String,
}

fn default_hover_detail() -> String {
    "signature".to_string()
}
fn default_true() -> bool {
    true
}
fn default_one() -> u32 {
    1
}

use std::sync::atomic::{AtomicBool, Ordering};

struct MyHandler {
    lsp_client: Option<Arc<Mutex<lsp::LspClient>>>,
    session_manager: Arc<sessions::SessionManager>,
    terminal_manager: Arc<terminals::TerminalManager>,
    subscribed_to_diagnostics: AtomicBool,
    mcp_runtime: Arc<Mutex<Option<Arc<dyn McpServer>>>>,
}

fn create_tool(name: &str, description: &str, schema: serde_json::Value) -> Tool {
    let input_schema: rust_mcp_sdk::schema::ToolInputSchema =
        serde_json::from_value(schema).unwrap();
    Tool {
        name: name.to_string(),
        description: Some(description.to_string()),
        input_schema,
        annotations: None,
        execution: None,
        icons: vec![],
        meta: None,
        output_schema: None,
        title: None,
    }
}

impl MyHandler {
    fn clean_signature(&self, line: &str) -> String {
        line.trim()
            .trim_end_matches('{')
            .trim_end_matches(';')
            .trim()
            .to_string()
    }

    fn collect_nested_matching_symbols(
        &self,
        symbols: &[lsp_types::DocumentSymbol],
        name_filter: &str,
        uri: &lsp_types::Uri,
        results: &mut Vec<(String, lsp_types::SymbolKind, lsp_types::OneOf<lsp_types::Location, lsp_types::Uri>)>,
    ) {
        for s in symbols {
            if s.name.contains(name_filter) {
                results.push((
                    s.name.clone(),
                    s.kind,
                    lsp_types::OneOf::Left(lsp_types::Location {
                        uri: Url::parse(&uri.to_string()).unwrap().to_string().parse().unwrap(),
                        range: s.range,
                    }),
                ));
            }
            if let Some(children) = &s.children {
                self.collect_nested_matching_symbols(children, name_filter, uri, results);
            }
        }
    }

    fn collect_nested_symbol_members(
        &self,
        symbols: &[lsp_types::DocumentSymbol],
        current_level: u32,
        max_level: u32,
        results: &mut Vec<models::SymbolMember>,
        lines: &[&str],
    ) {
        for s in symbols {
            let signature = lines.get(s.selection_range.start.line as usize).cloned().map(|l| self.clean_signature(l)).unwrap_or_default();
            results.push(models::SymbolMember {
                name: s.name.clone(),
                signature,
                line: s.selection_range.start.line,
                character: s.selection_range.start.character,
                kind: format!("{:?}", s.kind),
            });
            if current_level + 1 < max_level {
                if let Some(children) = &s.children {
                    self.collect_nested_symbol_members(children, current_level + 1, max_level, results, lines);
                }
            }
        }
    }
}

#[async_trait]
impl ServerHandler for MyHandler {
    async fn handle_list_prompts_request(
        &self,
        _params: Option<PaginatedRequestParams>,
        _runtime: Arc<dyn McpServer>,
    ) -> std::result::Result<ListPromptsResult, RpcError> {
        Ok(ListPromptsResult {
            prompts: vec![Prompt {
                name: "dynamic_guidance".to_string(),
                description: Some("Provides just-in-time guidance based on context.".to_string()),
                arguments: vec![rust_mcp_sdk::schema::PromptArgument {
                    name: "query".to_string(),
                    description: Some("The user's query to analyze.".to_string()),
                    required: Some(true),
                    title: None,
                }],
                icons: vec![],
                meta: None,
                title: None,
            }],
            next_cursor: None,
            meta: None,
        })
    }

    async fn handle_get_prompt_request(
        &self,
        params: GetPromptRequestParams,
        _runtime: Arc<dyn McpServer>,
    ) -> std::result::Result<GetPromptResult, RpcError> {
        if params.name == "dynamic_guidance" {
            let query = params
                .arguments
                .and_then(|a| a.get("query").cloned())
                .unwrap_or_default();
            let mut hints = Vec::new();
            if query.contains("test") || query.contains("failing") {
                hints.push("[System Note: To debug a failing test, your first step should be to create a terminal and run the test command to capture its output. Do not guess the cause of the failure.]");
            }
            if query.contains("not found") || query.contains("undefined") {
                hints.push("[System Note: Upon receiving a 'symbol not found' diagnostic, use code_find_symbol to check for misspellings or related symbols in other files.]");
            }

            Ok(GetPromptResult {
                description: Some("Dynamic hints based on query.".to_string()),
                messages: vec![PromptMessage {
                    role: rust_mcp_sdk::schema::Role::User,
                    content: ContentBlock::TextContent(TextContent::new(
                        hints.join("\n"),
                        None,
                        None,
                    )),
                }],
                meta: None,
            })
        } else {
            Err(RpcError::method_not_found())
        }
    }

    async fn handle_list_tools_request(
        &self,
        _params: Option<PaginatedRequestParams>,
        _runtime: Arc<dyn McpServer>,
    ) -> std::result::Result<ListToolsResult, RpcError> {
        let tools = vec![
            create_tool(
                "editor_subscribe_to_diagnostics",
                "Subscribes to proactive diagnostic notifications.",
                serde_json::json!({ "type": "object", "properties": {} }),
            ),
            create_tool(
                "code_get_actions_for_diagnostic",
                "Fetches potential Code Actions for a diagnostic.",
                serde_json::to_value(schemars::schema_for!(GetActionsArgs)).unwrap(),
            ),
            create_tool(
                "code_apply_action",
                "Applies a specific Code Action.",
                serde_json::to_value(schemars::schema_for!(ApplyActionArgs)).unwrap(),
            ),
            create_tool(
                "editor_get_definition",
                "Finds the definition of a symbol.",
                serde_json::to_value(schemars::schema_for!(PathLineCharArgs)).unwrap(),
            ),
            create_tool(
                "editor_get_references",
                "Finds all references to a symbol.",
                serde_json::to_value(schemars::schema_for!(PathLineCharArgs)).unwrap(),
            ),
            create_tool(
                "code_find_symbol",
                "Locates symbols and retrieves information.",
                serde_json::to_value(schemars::schema_for!(FindSymbolArgs)).unwrap(),
            ),
            create_tool(
                "code_show_sub_symbol",
                "Shows members of a symbol.",
                serde_json::to_value(schemars::schema_for!(ShowSubSymbolArgs)).unwrap(),
            ),
            create_tool(
                "refactor_start_session",
                "Initiates a refactoring session.",
                serde_json::to_value(schemars::schema_for!(StartSessionArgs)).unwrap(),
            ),
            create_tool(
                "refactor_propose_change",
                "Proposes a code change in a session.",
                serde_json::to_value(schemars::schema_for!(ProposeChangeArgs)).unwrap(),
            ),
            create_tool(
                "refactor_apply",
                "Applies approved changes in a session.",
                serde_json::to_value(schemars::schema_for!(SessionIdArgs)).unwrap(),
            ),
            create_tool(
                "refactor_approve_change",
                "Approves a proposed code change.",
                serde_json::to_value(schemars::schema_for!(ApproveChangeArgs)).unwrap(),
            ),
            create_tool(
                "refactor_interactive_rename",
                "Initiates a workspace-wide rename.",
                serde_json::to_value(schemars::schema_for!(InteractiveRenameArgs)).unwrap(),
            ),
            create_tool(
                "shell_create_terminal",
                "Creates a stateful terminal session.",
                serde_json::to_value(schemars::schema_for!(CreateTerminalArgs)).unwrap(),
            ),
            create_tool(
                "terminal_run_command",
                "Executes a command in a terminal.",
                serde_json::to_value(schemars::schema_for!(RunCommandArgs)).unwrap(),
            ),
            create_tool(
                "terminal_close",
                "Closes a terminal session.",
                serde_json::to_value(schemars::schema_for!(TerminalIdArgs)).unwrap(),
            ),
            create_tool(
                "ui_show_workspace_diagnostics",
                "Opens the workspace diagnostics UI.",
                serde_json::json!({ "type": "object", "properties": {} }),
            ),
            create_tool(
                "filesystem_read_file",
                "Reads the content of a file from the filesystem.",
                serde_json::to_value(schemars::schema_for!(ReadFileArgs)).unwrap(),
            ),
        ];
        Ok(ListToolsResult {
            tools,
            next_cursor: None,
            meta: None,
        })
    }

    async fn handle_call_tool_request(
        &self,
        params: CallToolRequestParams,
        _runtime: Arc<dyn McpServer>,
    ) -> std::result::Result<CallToolResult, CallToolError> {
        match params.name.as_str() {
            "editor_subscribe_to_diagnostics" => {
                self.subscribed_to_diagnostics.store(true, Ordering::SeqCst);
                let mut runtime_lock = self.mcp_runtime.lock().await;
                *runtime_lock = Some(_runtime);
                Ok(CallToolResult {
                    content: vec![ContentBlock::TextContent(TextContent::new(
                        serde_json::to_string(&serde_json::json!({
                            "status": "Subscribed",
                            "range": {
                                "start": { "line": 0, "character": 0 },
                                "end": { "line": 0, "character": 0 }
                            }
                        }))
                        .unwrap(),
                        None,
                        None,
                    ))],
                    is_error: Some(false),
                    meta: None,
                    structured_content: None,
                })
            }
            "code_get_actions_for_diagnostic" => {
                let args: GetActionsArgs = serde_json::from_value(serde_json::Value::Object(
                    params.arguments.unwrap_or_default(),
                ))
                .map_err(|e| CallToolError(Box::new(e)))?;

                if let Some(lsp) = &self.lsp_client {
                    let mut lsp = lsp.lock().await;
                    let lsp_params = lsp_types::CodeActionParams {
                        text_document: lsp_types::TextDocumentIdentifier {
                            uri: Url::from_file_path(&args.diagnostic_object.path).unwrap().to_string().parse().unwrap(),
                        },
                        range: args.diagnostic_object.diagnostic["range"]
                            .as_object()
                            .map(|_| {
                                serde_json::from_value(args.diagnostic_object.diagnostic["range"].clone()).unwrap()
                            })
                            .unwrap_or_default(),
                        context: lsp_types::CodeActionContext {
                            diagnostics: vec![serde_json::from_value(
                                args.diagnostic_object.diagnostic.clone(),
                            )
                            .unwrap()],
                            only: None,
                            trigger_kind: None,
                        },
                        work_done_progress_params: Default::default(),
                        partial_result_params: Default::default(),
                    };
                    let result = lsp
                        .send_request::<lsp_types::request::CodeActionRequest>(lsp_params)
                        .await
                        .map_err(|e| {
                            CallToolError(Box::new(
                                RpcError::internal_error().with_message(e.to_string()),
                            ))
                        })?;
                    Ok(CallToolResult {
                        content: vec![ContentBlock::TextContent(TextContent::new(
                            serde_json::to_string(&result).unwrap(),
                            None,
                            None,
                        ))],
                        is_error: Some(false),
                        meta: None,
                        structured_content: None,
                    })
                } else {
                    Err(CallToolError(Box::new(
                        RpcError::internal_error().with_message("LSP not available".to_string()),
                    )))
                }
            }
            "code_apply_action" => {
                let args: ApplyActionArgs = serde_json::from_value(serde_json::Value::Object(
                    params.arguments.unwrap_or_default(),
                ))
                .map_err(|e| CallToolError(Box::new(e)))?;

                if let Some(_lsp) = &self.lsp_client {
                    if let Some(edit) = args.action_object.edit {
                        let workspace_edit: lsp_types::WorkspaceEdit = serde_json::from_value(edit).unwrap();
                        let session_id = self.session_manager.start_session(format!("Quick Fix: {}", args.action_object.title));
                        let prop_id = self.session_manager.propose_change(&session_id, workspace_edit).unwrap();
                        
                        Ok(CallToolResult {
                            content: vec![ContentBlock::TextContent(TextContent::new(
                                serde_json::to_string(&serde_json::json!({
                                    "status": "SessionCreated",
                                    "sessionId": session_id,
                                    "proposalId": prop_id,
                                    "message": "The fix has been added to a refactoring session. Please approve it to apply."
                                }))
                                .unwrap(),
                                None,
                                None,
                            ))],
                            is_error: Some(false),
                            meta: None,
                            structured_content: None,
                        })
                    } else if let Some(command) = args.action_object.command {
                        let mut lsp = _lsp.lock().await;
                        let lsp_command: lsp_types::Command = serde_json::from_value(command).unwrap();
                        let execute_params = lsp_types::ExecuteCommandParams {
                            command: lsp_command.command,
                            arguments: lsp_command.arguments.unwrap_or_default(),
                            work_done_progress_params: Default::default(),
                        };
                        let result = lsp.send_request::<lsp_types::request::ExecuteCommand>(execute_params).await
                            .map_err(|e| CallToolError(Box::new(RpcError::internal_error().with_message(e.to_string()))))?;
                        Ok(CallToolResult {
                            content: vec![ContentBlock::TextContent(TextContent::new(
                                format!("Executed command for action '{}': {:?}", args.action_object.title, result),
                                None,
                                None,
                            ))],
                            is_error: Some(false),
                            meta: None,
                            structured_content: None,
                        })
                    } else {
                        Ok(CallToolResult {
                            content: vec![ContentBlock::TextContent(TextContent::new(
                                format!("Action '{}' has no edit or command to apply.", args.action_object.title),
                                None,
                                None,
                            ))],
                            is_error: Some(false),
                            meta: None,
                            structured_content: None,
                        })
                    }
                } else {
                    Err(CallToolError(Box::new(
                        RpcError::internal_error().with_message("LSP not available".to_string()),
                    )))
                }
            }
            "editor_get_definition" => {
                let args: PathLineCharArgs = serde_json::from_value(serde_json::Value::Object(
                    params.arguments.unwrap_or_default(),
                ))
                .map_err(|e| CallToolError(Box::new(e)))?;

                if let Some(lsp) = &self.lsp_client {
                    let mut lsp = lsp.lock().await;
                    let lsp_params = lsp_types::GotoDefinitionParams {
                        text_document_position_params: lsp_types::TextDocumentPositionParams {
                            text_document: lsp_types::TextDocumentIdentifier {
                                uri: Url::from_file_path(&args.path).unwrap().to_string().parse().unwrap(),
                            },
                            position: lsp_types::Position {
                                line: args.line,
                                character: args.character,
                            },
                        },
                        work_done_progress_params: Default::default(),
                        partial_result_params: Default::default(),
                    };
                    let result = lsp
                        .send_request::<lsp_types::request::GotoDefinition>(lsp_params)
                        .await
                        .map_err(|e| {
                            CallToolError(Box::new(
                                RpcError::internal_error().with_message(e.to_string()),
                            ))
                        })?;
                    
                    let mut results = Vec::new();
                    if let Some(result) = result {
                        match result {
                            lsp_types::GotoDefinitionResponse::Scalar(location) => {
                                let url = Url::parse(&location.uri.to_string()).unwrap();
                                results.push(models::Location {
                                    path: url.to_file_path().unwrap().to_string_lossy().to_string(),
                                    line: location.range.start.line,
                                    character: location.range.start.character,
                                    hover_info: None,
                                });
                            }
                            lsp_types::GotoDefinitionResponse::Array(locations) => {
                                for location in locations {
                                    let url = Url::parse(&location.uri.to_string()).unwrap();
                                    results.push(models::Location {
                                        path: url.to_file_path().unwrap().to_string_lossy().to_string(),
                                        line: location.range.start.line,
                                        character: location.range.start.character,
                                        hover_info: None,
                                    });
                                }
                            }
                            lsp_types::GotoDefinitionResponse::Link(links) => {
                                for link in links {
                                    let url = Url::parse(&link.target_uri.to_string()).unwrap();
                                    results.push(models::Location {
                                        path: url.to_file_path().unwrap().to_string_lossy().to_string(),
                                        line: link.target_range.start.line,
                                        character: link.target_range.start.character,
                                        hover_info: None,
                                    });
                                }
                            }
                        }
                    }

                    Ok(CallToolResult {
                        content: vec![ContentBlock::TextContent(TextContent::new(
                            serde_json::to_string(&results).unwrap(),
                            None,
                            None,
                        ))],
                        is_error: Some(false),
                        meta: None,
                        structured_content: None,
                    })
                } else {
                    Err(CallToolError(Box::new(
                        RpcError::internal_error().with_message("LSP not available".to_string()),
                    )))
                }
            }
            "editor_get_references" => {
                let args: PathLineCharArgs = serde_json::from_value(serde_json::Value::Object(
                    params.arguments.unwrap_or_default(),
                ))
                .map_err(|e| CallToolError(Box::new(e)))?;

                if let Some(lsp) = &self.lsp_client {
                    let mut lsp = lsp.lock().await;
                    let lsp_params = lsp_types::ReferenceParams {
                        text_document_position: lsp_types::TextDocumentPositionParams {
                            text_document: lsp_types::TextDocumentIdentifier {
                                uri: Url::from_file_path(&args.path).unwrap().to_string().parse().unwrap(),
                            },
                            position: lsp_types::Position {
                                line: args.line,
                                character: args.character,
                            },
                        },
                        work_done_progress_params: Default::default(),
                        partial_result_params: Default::default(),
                        context: lsp_types::ReferenceContext {
                            include_declaration: true,
                        },
                    };
                    let result = lsp
                        .send_request::<lsp_types::request::References>(lsp_params)
                        .await
                        .map_err(|e| {
                            CallToolError(Box::new(
                                RpcError::internal_error().with_message(e.to_string()),
                            ))
                        })?;
                    
                    let mut results = Vec::new();
                    if let Some(locations) = result {
                        for location in locations {
                            let url = Url::parse(&location.uri.to_string()).unwrap();
                            results.push(models::Location {
                                path: url.to_file_path().unwrap().to_string_lossy().to_string(),
                                line: location.range.start.line,
                                character: location.range.start.character,
                                hover_info: None,
                            });
                        }
                    }

                    Ok(CallToolResult {
                        content: vec![ContentBlock::TextContent(TextContent::new(
                            serde_json::to_string(&results).unwrap(),
                            None,
                            None,
                        ))],
                        is_error: Some(false),
                        meta: None,
                        structured_content: None,
                    })
                } else {
                    Err(CallToolError(Box::new(
                        RpcError::internal_error().with_message("LSP not available".to_string()),
                    )))
                }
            }
            "code_find_symbol" => {
                let args: FindSymbolArgs = serde_json::from_value(serde_json::Value::Object(
                    params.arguments.unwrap_or_default(),
                ))
                .map_err(|e| CallToolError(Box::new(e)))?;

                if let Some(lsp) = &self.lsp_client {
                    let mut lsp = lsp.lock().await;
                    
                    let mut search_file_uri = None;
                    if let Some(file_hint) = &args.file {
                        // Fuzzy search for file
                        let file_params = lsp_types::WorkspaceSymbolParams {
                            query: file_hint.clone(),
                            work_done_progress_params: Default::default(),
                            partial_result_params: Default::default(),
                        };
                        if let Ok(Some(lsp_types::WorkspaceSymbolResponse::Flat(symbols))) = lsp.send_request::<lsp_types::request::WorkspaceSymbolRequest>(file_params).await {
                             let mut matches = Vec::new();
                             for s in symbols {
                                 let uri = s.location.uri;
                                 if uri.to_string().contains(file_hint) {
                                     if !matches.contains(&uri) {
                                         matches.push(uri);
                                     }
                                 }
                             }
                             if matches.len() > 1 {
                                 return Err(CallToolError(Box::new(RpcError::internal_error().with_message(format!("Ambiguous file name '{}'. Matches: {:?}", file_hint, matches)))));
                             }
                             search_file_uri = matches.into_iter().next();
                        }
                    }

                    let mut lsp_results: Vec<(String, lsp_types::SymbolKind, lsp_types::OneOf<lsp_types::Location, lsp_types::Uri>)> = Vec::new();
                    if let Some(uri) = search_file_uri {
                        let doc_params = lsp_types::DocumentSymbolParams {
                            text_document: lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                            work_done_progress_params: Default::default(),
                            partial_result_params: Default::default(),
                        };
                        if let Ok(Some(response)) = lsp.send_request::<lsp_types::request::DocumentSymbolRequest>(doc_params).await {
                            match response {
                                lsp_types::DocumentSymbolResponse::Flat(symbols) => {
                                    for s in symbols {
                                        if s.name.contains(&args.symbol_name) {
                                            lsp_results.push((s.name, s.kind, lsp_types::OneOf::Left(s.location)));
                                        }
                                    }
                                }
                                lsp_types::DocumentSymbolResponse::Nested(symbols) => {
                                    self.collect_nested_matching_symbols(&symbols, &args.symbol_name, &uri, &mut lsp_results);
                                }
                            }
                        }
                        
                        // Use locationHint to disambiguate
                        if let Some(hint) = args.location_hint {
                            lsp_results.sort_by_key(|(_, _, loc)| {
                                if let lsp_types::OneOf::Left(l) = loc {
                                    let d_line = (l.range.start.line as i32 - hint.line as i32).abs();
                                    let d_char = (l.range.start.character as i32 - hint.character as i32).abs();
                                    d_line * 1000 + d_char
                                } else {
                                    i32::MAX
                                }
                            });
                        }
                    } else {
                        let query = if let Some(context) = &args.context_hint {
                            format!("{} {}", args.symbol_name, context)
                        } else {
                            args.symbol_name.clone()
                        };
                        let lsp_params = lsp_types::WorkspaceSymbolParams {
                            query,
                            work_done_progress_params: Default::default(),
                            partial_result_params: Default::default(),
                        };
                        if let Ok(Some(response)) = lsp.send_request::<lsp_types::request::WorkspaceSymbolRequest>(lsp_params).await {
                            match response {
                                lsp_types::WorkspaceSymbolResponse::Nested(s) => {
                                    for symbol in s {
                                        lsp_results.push((symbol.name, symbol.kind, match symbol.location {
                                            lsp_types::OneOf::Left(l) => lsp_types::OneOf::Left(l),
                                            lsp_types::OneOf::Right(l) => lsp_types::OneOf::Right(l.uri),
                                        }));
                                    }
                                }
                                lsp_types::WorkspaceSymbolResponse::Flat(s) => {
                                    for symbol in s {
                                        lsp_results.push((symbol.name, symbol.kind, lsp_types::OneOf::Left(symbol.location)));
                                    }
                                }
                            }
                        }
                    }

                    if args.feeling_lucky && !lsp_results.is_empty() {
                        lsp_results = vec![lsp_results.remove(0)];
                    }

                    let mut results = Vec::new();
                    for (_name, _kind, location) in lsp_results {
                        if let lsp_types::OneOf::Left(location) = location {
                            let url = Url::parse(&location.uri.to_string()).unwrap();
                            let mut hover_info = None;
                            
                            if args.hover_detail != "none" {
                                let hover_params = lsp_types::HoverParams {
                                    text_document_position_params: lsp_types::TextDocumentPositionParams {
                                        text_document: lsp_types::TextDocumentIdentifier { uri: location.uri.clone() },
                                        position: location.range.start,
                                    },
                                    work_done_progress_params: Default::default(),
                                };
                                if let Ok(Some(hover)) = lsp.send_request::<lsp_types::request::HoverRequest>(hover_params).await {
                                    hover_info = Some(format!("{:?}", hover.contents));
                                }
                            }

                            results.push(models::Location {
                                path: url.to_file_path().unwrap().to_string_lossy().to_string(),
                                line: location.range.start.line,
                                character: location.range.start.character,
                                hover_info,
                            });
                        }
                    }

                    Ok(CallToolResult {
                        content: vec![ContentBlock::TextContent(TextContent::new(
                            serde_json::to_string(&results).unwrap(),
                            None,
                            None,
                        ))],
                        is_error: Some(false),
                        meta: None,
                        structured_content: None,
                    })
                } else {
                    Err(CallToolError(Box::new(
                        RpcError::internal_error().with_message("LSP not available".to_string()),
                    )))
                }
            }
            "code_show_sub_symbol" => {
                let args: ShowSubSymbolArgs = serde_json::from_value(serde_json::Value::Object(
                    params.arguments.unwrap_or_default(),
                ))
                .map_err(|e| CallToolError(Box::new(e)))?;

                if let Some(lsp) = &self.lsp_client {
                    let mut lsp = lsp.lock().await;
                    let uri: lsp_types::Uri = Url::from_file_path(&args.symbol_path).unwrap().to_string().parse().unwrap();
                    let lsp_params = lsp_types::DocumentSymbolParams {
                        text_document: lsp_types::TextDocumentIdentifier {
                            uri: uri.clone(),
                        },
                        work_done_progress_params: Default::default(),
                        partial_result_params: Default::default(),
                    };
                    let result = lsp
                        .send_request::<lsp_types::request::DocumentSymbolRequest>(lsp_params)
                        .await
                        .map_err(|e| {
                            CallToolError(Box::new(
                                RpcError::internal_error().with_message(e.to_string()),
                            ))
                        })?;
                    
                    let mut results = Vec::new();
                    if let Some(response) = result {
                        let path = Url::parse(&uri.to_string()).unwrap().to_file_path().unwrap();
                        let content = tokio::fs::read_to_string(&path).await.unwrap_or_default();
                        let lines: Vec<&str> = content.lines().collect();

                        match response {
                            lsp_types::DocumentSymbolResponse::Flat(symbols) => {
                                for symbol in symbols {
                                    let signature = lines.get(symbol.location.range.start.line as usize).cloned().map(|l| self.clean_signature(l)).unwrap_or_default();
                                    results.push(models::SymbolMember {
                                        name: format!("{}::{}", args.symbol, symbol.name),
                                        signature,
                                        line: symbol.location.range.start.line,
                                        character: symbol.location.range.start.character,
                                        kind: format!("{:?}", symbol.kind),
                                    });
                                }
                            }
                            lsp_types::DocumentSymbolResponse::Nested(symbols) => {
                                self.collect_nested_symbol_members(&symbols, 0, args.level, &mut results, &lines);
                                // Prefix with parent symbol if it's a top-level request
                                for r in &mut results {
                                    r.name = format!("{}::{}", args.symbol, r.name);
                                }
                            }
                        }
                    }

                    Ok(CallToolResult {
                        content: vec![ContentBlock::TextContent(TextContent::new(
                            serde_json::to_string(&results).unwrap(),
                            None,
                            None,
                        ))],
                        is_error: Some(false),
                        meta: None,
                        structured_content: None,
                    })
                } else {
                    Err(CallToolError(Box::new(
                        RpcError::internal_error().with_message("LSP not available".to_string()),
                    )))
                }
            }
            "refactor_start_session" => {
                let args: StartSessionArgs = serde_json::from_value(serde_json::Value::Object(
                    params.arguments.unwrap_or_default(),
                ))
                .map_err(|e| CallToolError(Box::new(e)))?;
                let id = self.session_manager.start_session(args.description);
                Ok(CallToolResult {
                    content: vec![ContentBlock::TextContent(TextContent::new(
                        format!("Session started with ID: {}", id),
                        None,
                        None,
                    ))],
                    is_error: Some(false),
                    meta: None,
                    structured_content: None,
                })
            }
            "refactor_propose_change" => {
                let args: ProposeChangeArgs = serde_json::from_value(serde_json::Value::Object(
                    params.arguments.unwrap_or_default(),
                ))
                .map_err(|e| CallToolError(Box::new(e)))?;
                let edit: lsp_types::WorkspaceEdit = serde_json::from_value(args.change_object)
                    .map_err(|e| CallToolError(Box::new(e)))?;
                if let Some(prop_id) = self.session_manager.propose_change(&args.session_id, edit) {
                    Ok(CallToolResult {
                        content: vec![ContentBlock::TextContent(TextContent::new(
                            format!("Proposal added: {}", prop_id),
                            None,
                            None,
                        ))],
                        is_error: Some(false),
                        meta: None,
                        structured_content: None,
                    })
                } else {
                    Ok(CallToolResult {
                        content: vec![ContentBlock::TextContent(TextContent::new(
                            format!("Session {} not found", args.session_id),
                            None,
                            None,
                        ))],
                        is_error: Some(true),
                        meta: None,
                        structured_content: None,
                    })
                }
            }
            "refactor_apply" => {
                let args: SessionIdArgs = serde_json::from_value(serde_json::Value::Object(
                    params.arguments.unwrap_or_default(),
                ))
                .map_err(|e| CallToolError(Box::new(e)))?;
                if let Some(changes) = self.session_manager.apply_approved(&args.session_id).await {
                    Ok(CallToolResult {
                        content: vec![ContentBlock::TextContent(TextContent::new(
                            format!(
                                "Applied {} approved changes for session {}.",
                                changes.len(),
                                args.session_id
                            ),
                            None,
                            None,
                        ))],
                        is_error: Some(false),
                        meta: None,
                        structured_content: None,
                    })
                } else {
                    Ok(CallToolResult {
                        content: vec![ContentBlock::TextContent(TextContent::new(
                            format!("Session {} not found", args.session_id),
                            None,
                            None,
                        ))],
                        is_error: Some(true),
                        meta: None,
                        structured_content: None,
                    })
                }
            }
            "refactor_approve_change" => {
                let args: ApproveChangeArgs = serde_json::from_value(serde_json::Value::Object(
                    params.arguments.unwrap_or_default(),
                ))
                .map_err(|e| CallToolError(Box::new(e)))?;
                if self
                    .session_manager
                    .approve_proposal(&args.session_id, &args.proposal_id)
                {
                    Ok(CallToolResult {
                        content: vec![ContentBlock::TextContent(TextContent::new(
                            format!(
                                "Proposal {} approved for session {}.",
                                args.proposal_id, args.session_id
                            ),
                            None,
                            None,
                        ))],
                        is_error: Some(false),
                        meta: None,
                        structured_content: None,
                    })
                } else {
                    Ok(CallToolResult {
                        content: vec![ContentBlock::TextContent(TextContent::new(
                            format!(
                                "Proposal {} or Session {} not found",
                                args.proposal_id, args.session_id
                            ),
                            None,
                            None,
                        ))],
                        is_error: Some(true),
                        meta: None,
                        structured_content: None,
                    })
                }
            }
            "refactor_interactive_rename" => {
                let args: InteractiveRenameArgs = serde_json::from_value(
                    serde_json::Value::Object(params.arguments.unwrap_or_default()),
                )
                .map_err(|e| CallToolError(Box::new(e)))?;

                if let Some(lsp) = &self.lsp_client {
                    let mut lsp = lsp.lock().await;
                    let position =
                        args.symbol_to_find.location_hint.clone().unwrap_or(models::Position {
                            line: 0,
                            character: 0,
                        });
                    let lsp_params = lsp_types::RenameParams {
                        text_document_position: lsp_types::TextDocumentPositionParams {
                            text_document: lsp_types::TextDocumentIdentifier {
                                uri: Url::from_file_path(&args.path).unwrap().to_string().parse().unwrap(),
                            },
                            position: lsp_types::Position {
                                line: position.line,
                                character: position.character,
                            },
                        },
                        new_name: args.new_name.clone(),
                        work_done_progress_params: Default::default(),
                    };

                    let result = lsp
                        .send_request::<lsp_types::request::Rename>(lsp_params)
                        .await
                        .map_err(|e| {
                            CallToolError(Box::new(
                                RpcError::internal_error().with_message(e.to_string()),
                            ))
                        })?;

                    if let Some(edit) = result {
                        let session_id = self.session_manager.start_session(format!(
                            "Rename {} to {}",
                            args.symbol_to_find.symbol_name, args.new_name
                        ));
                        self.session_manager.propose_change(&session_id, edit);
                        Ok(CallToolResult {
                            content: vec![ContentBlock::TextContent(TextContent::new(
                                serde_json::to_string(&serde_json::json!({
                                    "status": "Initiated",
                                    "sessionId": session_id,
                                    "message": "Workspace edits proposed and added to session."
                                }))
                                .unwrap(),
                                None,
                                None,
                            ))],
                            is_error: Some(false),
                            meta: None,
                            structured_content: None,
                        })
                    } else {
                        Ok(CallToolResult {
                            content: vec![ContentBlock::TextContent(TextContent::new(
                                "LSP returned no edits for rename.".to_string(),
                                None,
                                None,
                            ))],
                            is_error: Some(true),
                            meta: None,
                            structured_content: None,
                        })
                    }
                } else {
                    Err(CallToolError(Box::new(
                        RpcError::internal_error().with_message("LSP not available".to_string()),
                    )))
                }
            }
            "shell_create_terminal" => {
                let args: CreateTerminalArgs = serde_json::from_value(serde_json::Value::Object(
                    params.arguments.unwrap_or_default(),
                ))
                .map_err(|e| CallToolError(Box::new(e)))?;
                let id = self.terminal_manager.create_terminal().map_err(|e| {
                    CallToolError(Box::new(
                        RpcError::internal_error().with_message(e.to_string()),
                    ))
                })?;
                Ok(CallToolResult {
                    content: vec![ContentBlock::TextContent(TextContent::new(
                        format!(
                            "Terminal '{}' created with ID: {}",
                            args.name.unwrap_or_default(),
                            id
                        ),
                        None,
                        None,
                    ))],
                    is_error: Some(false),
                    meta: None,
                    structured_content: None,
                })
            }
            "terminal_run_command" => {
                let args: RunCommandArgs = serde_json::from_value(serde_json::Value::Object(
                    params.arguments.unwrap_or_default(),
                ))
                .map_err(|e| CallToolError(Box::new(e)))?;
                match self
                    .terminal_manager
                    .run_command(&args.terminal_id, &args.command)
                    .await
                {
                    Ok((stdout, stderr)) => Ok(CallToolResult {
                        content: vec![ContentBlock::TextContent(TextContent::new(
                            format!(
                                "Terminal {}: stdout: {}\nstderr: {}",
                                args.terminal_id, stdout, stderr
                            ),
                            None,
                            None,
                        ))],
                        is_error: Some(false),
                        meta: None,
                        structured_content: None,
                    }),
                    Err(e) => Ok(CallToolResult {
                        content: vec![ContentBlock::TextContent(TextContent::new(
                            format!("Terminal {}: error: {}", args.terminal_id, e),
                            None,
                            None,
                        ))],
                        is_error: Some(true),
                        meta: None,
                        structured_content: None,
                    }),
                }
            }
            "terminal_close" => {
                let args: TerminalIdArgs = serde_json::from_value(serde_json::Value::Object(
                    params.arguments.unwrap_or_default(),
                ))
                .map_err(|e| CallToolError(Box::new(e)))?;
                let closed = self.terminal_manager.close_terminal(&args.terminal_id);
                Ok(CallToolResult {
                    content: vec![ContentBlock::TextContent(TextContent::new(
                        format!("Terminal {} closed: {}", args.terminal_id, closed),
                        None,
                        None,
                    ))],
                    is_error: Some(false),
                    meta: None,
                    structured_content: None,
                })
            }
            "ui_show_workspace_diagnostics" => {
                // In a real implementation, this would send a notification to the client to open the diagnostics UI.
                let runtime = self.mcp_runtime.lock().await;
                if let Some(runtime) = runtime.as_ref() {
                    let _ = runtime
                        .send_notification(
                            rust_mcp_sdk::schema::NotificationFromServer::CustomNotification(
                                rust_mcp_sdk::schema::CustomNotification {
                                    method: "ui/showDiagnostics".to_string(),
                                    params: None,
                                },
                            ),
                        )
                        .await;
                }
                Ok(CallToolResult {
                    content: vec![ContentBlock::TextContent(TextContent::new(
                        "Workspace diagnostics UI opened.".to_string(),
                        None,
                        None,
                    ))],
                    is_error: Some(false),
                    meta: None,
                    structured_content: None,
                })
            }
            "filesystem_read_file" => {
                let args: ReadFileArgs = serde_json::from_value(serde_json::Value::Object(
                    params.arguments.unwrap_or_default(),
                ))
                .map_err(|e| CallToolError(Box::new(e)))?;
                let content = tokio::fs::read_to_string(&args.path).await.map_err(|e| {
                    CallToolError(Box::new(
                        RpcError::internal_error().with_message(e.to_string()),
                    ))
                })?;
                Ok(CallToolResult {
                    content: vec![ContentBlock::TextContent(TextContent::new(
                        content,
                        None,
                        None,
                    ))],
                    is_error: Some(false),
                    meta: None,
                    structured_content: None,
                })
            }
            _ => Err(CallToolError(Box::new(
                RpcError::method_not_found()
                    .with_message(format!("Tool {} not found", params.name)),
            ))),
        }
    }
}

use url::Url;

#[tokio::main]
async fn main() -> Result<()> {
    let (notification_tx, mut notification_rx) = tokio::sync::mpsc::channel::<serde_json::Value>(100);

    let lsp_client = match lsp::LspClient::start("rust-analyzer", &[], notification_tx).await {
        Ok(client) => Some(Arc::new(Mutex::new(client))),
        Err(e) => {
            eprintln!("Warning: Could not start rust-analyzer: {}", e);
            None
        }
    };

    let session_manager = Arc::new(sessions::SessionManager::new());
    let terminal_manager = Arc::new(terminals::TerminalManager::new());
    let mcp_runtime: Arc<Mutex<Option<Arc<dyn McpServer>>>> = Arc::new(Mutex::new(None));

    let handler = MyHandler {
        lsp_client: lsp_client.clone(),
        session_manager: session_manager.clone(),
        terminal_manager: terminal_manager.clone(),
        subscribed_to_diagnostics: AtomicBool::new(false),
        mcp_runtime: mcp_runtime.clone(),
    };

    let runtime_for_notifications = mcp_runtime.clone();
    tokio::spawn(async move {
        while let Some(mut notif) = notification_rx.recv().await {
            if let Some(method) = notif.get("method").and_then(|m| m.as_str()) {
                if method == "textDocument/publishDiagnostics" {
                    // Enrich diagnostics
                    if let Some(params) = notif.get_mut("params") {
                        let uri_str = params.get("uri").and_then(|u| u.as_str()).map(|s| s.to_string());
                        if let (Some(uri_str), Some(diagnostics)) = (
                            uri_str,
                            params.get_mut("diagnostics").and_then(|d| d.as_array_mut()),
                        ) {
                            if let Ok(url) = Url::parse(&uri_str) {
                                if let Ok(path) = url.to_file_path() {
                                    if let Ok(content) = tokio::fs::read_to_string(&path).await {
                                        let lines: Vec<&str> = content.lines().collect();
                                        for diag in diagnostics {
                                            if let Some(range) = diag.get("range") {
                                                let start_line = range["start"]["line"].as_u64().unwrap_or(0) as usize;
                                                let start_char = range["start"]["character"].as_u64().unwrap_or(0) as usize;
                                                let end_char = range["end"]["character"].as_u64().unwrap_or(0) as usize;
                                                
                                                if start_line < lines.len() {
                                                    let line = lines[start_line];
                                                    diag["line_content"] = serde_json::json!(line);
                                                    
                                                    // Try to extract symbol_name
                                                    if end_char > start_char && end_char <= line.len() {
                                                        diag["symbol_name"] = serde_json::json!(&line[start_char..end_char]);
                                                    }
                                                }
                                            }
                                            diag["path"] = serde_json::json!(path.to_string_lossy());
                                        }
                                    }
                                }
                            }
                        }
                    }

                    let runtime = runtime_for_notifications.lock().await;
                    if let Some(runtime) = runtime.as_ref() {
                        let _ = runtime
                            .send_notification(
                                rust_mcp_sdk::schema::NotificationFromServer::CustomNotification(
                                    rust_mcp_sdk::schema::CustomNotification {
                                        method: "notifications/diagnostics".to_string(),
                                        params: Some(notif["params"].as_object().cloned().unwrap_or_default()),
                                    },
                                ),
                            )
                            .await;
                    }
                }
            }
        }
    });

    let transport = StdioTransport::new(TransportOptions::default())
        .map_err(|e| anyhow::anyhow!("Failed to create StdioTransport: {:?}", e))?;

    let options = McpServerOptions {
        server_details: InitializeResult {
            protocol_version: "2024-11-05".to_string(),
            capabilities: rust_mcp_sdk::schema::ServerCapabilities {
                prompts: Some(rust_mcp_sdk::schema::ServerCapabilitiesPrompts {
                    list_changed: Some(false),
                }),
                ..Default::default()
            },
            server_info: Implementation {
                name: "lsp-mcp".into(),
                version: "1.16".into(),
                description: Some(
                    "LSP-MCP server implementing AI Development Specification 1.16".to_string(),
                ),
                icons: vec![],
                title: Some("LSP MCP Server".to_string()),
                website_url: None,
            },
            instructions: Some("This server provides a high-level, semantic bridge to the editor's Language Server. 
[System Note: To debug a failing test, your first step should be to create a terminal and run the test command to capture its output. Do not guess the cause of the failure.]
[System Note: Upon receiving a 'symbol not found' diagnostic, use code_find_symbol to check for misspellings or related symbols in other files.]".to_string()),
            meta: None,
        },
        transport,
        handler: rust_mcp_sdk::mcp_server::ToMcpServerHandler::to_mcp_server_handler(handler),
        task_store: None,
        client_task_store: None,
        message_observer: None,
    };

    let server = create_server(options);

    eprintln!("LSP-MCP server running on stdio...");
    server
        .start()
        .await
        .map_err(|e| anyhow::anyhow!("Server error: {:?}", e))?;

    Ok(())
}
