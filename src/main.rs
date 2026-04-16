mod lsp;
mod mock_server;
mod models;

#[cfg(test)]
mod tests;

use anyhow::Result;
use clap::Parser;
use rmcp::{
    ErrorData, RoleServer, ServerHandler, ServiceExt,
    handler::server::{
        router::{prompt::PromptRouter, tool::ToolRouter},
        wrapper::Parameters,
    },
    model::*,
    prompt, prompt_handler, prompt_router,
    service::RequestContext,
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Mutex;
use url::Url;

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct FindSymbolArgs {
    /// The name of the symbol to find (e.g., a function or class name).
    symbol_name: String,
    /// A full or partial path to a file to search within. If omitted, the search is performed across the entire workspace.
    file: Option<String>,
    /// A `{line, character}` object providing a best-guess location. (Note: All position values are 0-based.)
    location_hint: Option<models::Position>,
    /// A natural language description of the symbol's context.
    context_hint: Option<String>,
    /// Determines if hover info is included. Can be "none", "signature", or "full".
    #[serde(default = "default_hover_detail")]
    hover_detail: String,
    /// If True, the tool returns the single best match. If False and the result is ambiguous, it returns a list of choices.
    #[serde(default = "default_true")]
    feeling_lucky: bool,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct PathLineCharArgs {
    /// The absolute path to the file.
    path: String,
    /// The 0-based line number.
    line: u32,
    /// The 0-based character offset.
    character: u32,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct ShowSubSymbolArgs {
    /// The name of the symbol to inspect (e.g., a class name).
    symbol: String,
    /// The path to the file containing the symbol.
    symbol_path: String,
    /// The depth of the symbol hierarchy to explore. A level of 1 shows only the direct children of the symbol. A level of 2 or more will show nested symbols.
    #[serde(default = "default_one")]
    level: u32,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct GetActionsArgs {
    /// The full, enriched diagnostic object. (Note: The range within this object uses 0-based indexing.)
    diagnostic_object: models::EnrichedDiagnostic,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct ApplyActionArgs {
    /// The specific action object to execute, as returned by code_get_actions_for_diagnostic.
    action_object: models::CodeAction,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct InteractiveRenameArgs {
    /// The path to a file containing an instance of the symbol.
    path: String,
    /// An object containing the symbolName and a locationHint or contextHint to identify the symbol to rename. (Note: All position values are 0-based.)
    symbol_to_find: FindSymbolArgs,
    /// The new name for the symbol.
    new_name: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct GetCompletionsArgs {
    /// The absolute or relative path to the file.
    path: String,
    /// The 0-based line number.
    line: u32,
    /// The 0-based character offset.
    character: u32,
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

#[derive(Clone)]
struct MyHandler {
    lsp_client: Arc<Mutex<Option<Arc<Mutex<lsp::LspClient>>>>>,
    notification_tx: tokio::sync::mpsc::Sender<serde_json::Value>,
    root_uri: Option<lsp_types::Uri>,
    lsp_command: String,
    subscribed_to_diagnostics: Arc<AtomicBool>,
    tool_router: ToolRouter<Self>,
    prompt_router: PromptRouter<Self>,
}

impl MyHandler {
    fn new(
        notification_tx: tokio::sync::mpsc::Sender<serde_json::Value>,
        root_uri: Option<lsp_types::Uri>,
        lsp_command: String,
        pre_initialized: Option<Arc<Mutex<lsp::LspClient>>>,
    ) -> Self {
        Self {
            lsp_client: Arc::new(Mutex::new(pre_initialized)),
            notification_tx,
            root_uri,
            lsp_command,
            subscribed_to_diagnostics: Arc::new(AtomicBool::new(false)),
            tool_router: Self::tool_router(),
            prompt_router: Self::prompt_router(),
        }
    }

    async fn get_lsp_client(&self) -> Option<Arc<Mutex<lsp::LspClient>>> {
        let mut client_lock = self.lsp_client.lock().await;
        if let Some(client) = client_lock.as_ref() {
            return Some(client.clone());
        }

        match lsp::LspClient::start(
            &self.lsp_command,
            &[],
            self.notification_tx.clone(),
            self.root_uri.clone(),
        )
        .await
        {
            Ok(client) => {
                let arc_client = Arc::new(Mutex::new(client));
                *client_lock = Some(arc_client.clone());
                Some(arc_client)
            }
            Err(e) => {
                eprintln!("Warning: Could not start rust-analyzer: {}", e);
                None
            }
        }
    }

    fn to_absolute_path(&self, path_str: &str) -> std::path::PathBuf {
        let path = std::path::Path::new(path_str);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            let base_dir = self
                .root_uri
                .as_ref()
                .and_then(|uri| Url::parse(&uri.to_string()).ok()?.to_file_path().ok())
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
            base_dir.join(path)
        }
    }

    fn clean_signature(line: &str) -> String {
        line.trim()
            .trim_end_matches('{')
            .trim_end_matches(';')
            .trim()
            .to_string()
    }

    fn find_files(dir: &std::path::Path, query: &str) -> Vec<std::path::PathBuf> {
        let mut matches = Vec::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    if name == "target"
                        || name == ".git"
                        || name == "node_modules"
                        || name.starts_with('.')
                    {
                        continue;
                    }
                    matches.extend(Self::find_files(&path, query));
                } else if path.to_string_lossy().contains(query) {
                    matches.push(path);
                }
            }
        }
        matches
    }

    fn collect_nested_matching_symbols(
        symbols: &[lsp_types::DocumentSymbol],
        name_filter: &str,
        uri: &lsp_types::Uri,
        results: &mut Vec<(
            String,
            lsp_types::SymbolKind,
            lsp_types::OneOf<lsp_types::Location, lsp_types::Uri>,
        )>,
    ) {
        for s in symbols {
            if s.name.contains(name_filter) {
                results.push((
                    s.name.clone(),
                    s.kind,
                    lsp_types::OneOf::Left(lsp_types::Location {
                        uri: Url::parse(&uri.to_string())
                            .unwrap()
                            .to_string()
                            .parse()
                            .unwrap(),
                        range: s.range,
                    }),
                ));
            }
            if let Some(children) = &s.children {
                Self::collect_nested_matching_symbols(children, name_filter, uri, results);
            }
        }
    }

    fn find_symbol_in_hierarchy<'a>(
        symbols: &'a [lsp_types::DocumentSymbol],
        target_name: &str,
    ) -> Option<&'a lsp_types::DocumentSymbol> {
        let parts: Vec<&str> = target_name.split("::").collect();
        if parts.is_empty() {
            return None;
        }

        let mut current_symbols = symbols;
        let mut current_found = None;

        for part in parts {
            let mut found = None;
            for s in current_symbols {
                if s.name == part {
                    found = Some(s);
                    break;
                }
            }
            if let Some(s) = found {
                current_found = Some(s);
                if let Some(children) = &s.children {
                    current_symbols = children;
                } else {
                    current_symbols = &[];
                }
            } else {
                // If we didn't find the exact part at this level, let's try searching recursively
                // just in case it's nested without all parents specified.
                for s in current_symbols {
                    if let Some(children) = &s.children {
                        if let Some(found_nested) =
                            Self::find_symbol_in_hierarchy(children, target_name)
                        {
                            return Some(found_nested);
                        }
                    }
                }
                return None;
            }
        }
        current_found
    }

    fn collect_nested_symbol_members(
        symbols: &[lsp_types::DocumentSymbol],
        current_level: u32,
        max_level: u32,
        results: &mut Vec<models::SymbolMember>,
        lines: &[&str],
    ) {
        for s in symbols {
            let signature = lines
                .get(s.selection_range.start.line as usize)
                .cloned()
                .map(Self::clean_signature)
                .unwrap_or_default();
            results.push(models::SymbolMember {
                name: s.name.clone(),
                signature,
                line: s.selection_range.start.line,
                character: s.selection_range.start.character,
                kind: format!("{:?}", s.kind),
            });
            if current_level + 1 < max_level
                && let Some(children) = &s.children
            {
                Self::collect_nested_symbol_members(
                    children,
                    current_level + 1,
                    max_level,
                    results,
                    lines,
                );
            }
        }
    }

    fn enrich_diagnostics(
        diagnostics: &mut Vec<serde_json::Value>,
        lines: &[&str],
        doc_symbols: &[(String, lsp_types::Range)],
        path: &std::path::Path,
    ) {
        for diag in diagnostics {
            if let Some(range) = diag.get("range") {
                let start_line = range["start"]["line"].as_u64().unwrap_or(0) as usize;
                let start_char = range["start"]["character"].as_u64().unwrap_or(0) as usize;
                let end_char = range["end"]["character"].as_u64().unwrap_or(0) as usize;

                if start_line < lines.len() {
                    let line = lines[start_line];
                    diag["line_content"] = serde_json::json!(line);
                    let diag_range: lsp_types::Range =
                        serde_json::from_value(diag["range"].clone()).unwrap_or_default();
                    let semantic_name = doc_symbols
                        .iter()
                        .find(|(_, r)| {
                            r.start.line <= diag_range.start.line
                                && r.end.line >= diag_range.end.line
                                && (r.start.line != diag_range.start.line
                                    || r.start.character <= diag_range.start.character)
                                && (r.end.line != diag_range.end.line
                                    || r.end.character >= diag_range.end.character)
                        })
                        .map(|(n, _)| n.clone());
                    if let Some(name) = semantic_name {
                        diag["symbol_name"] = serde_json::json!(name);
                        diag["symbol_name_source"] = serde_json::json!("semantic");
                    } else if end_char > start_char && end_char <= line.len() {
                        diag["symbol_name"] = serde_json::json!(&line[start_char..end_char]);
                        diag["symbol_name_source"] = serde_json::json!("textual");
                    }
                }
            }
            diag["path"] = serde_json::json!(path.to_string_lossy());
        }
    }

    pub async fn apply_workspace_edit(
        &self,
        edit: &lsp_types::WorkspaceEdit,
    ) -> anyhow::Result<()> {
        if let Some(changes) = &edit.changes {
            for (uri, edits) in changes {
                self.apply_text_edits_to_uri(uri, edits).await?;
            }
        }
        if let Some(document_changes) = &edit.document_changes {
            match document_changes {
                lsp_types::DocumentChanges::Edits(edits) => {
                    for edit in edits {
                        self.apply_text_edits_to_uri(
                            &edit.text_document.uri,
                            &edit
                                .edits
                                .iter()
                                .cloned()
                                .map(|e| match e {
                                    lsp_types::OneOf::Left(te) => te,
                                    lsp_types::OneOf::Right(ae) => ae.text_edit,
                                })
                                .collect::<Vec<_>>(),
                        )
                        .await?;
                    }
                }
                lsp_types::DocumentChanges::Operations(ops) => {
                    for op in ops {
                        match op {
                            lsp_types::DocumentChangeOperation::Edit(edit) => {
                                self.apply_text_edits_to_uri(
                                    &edit.text_document.uri,
                                    &edit
                                        .edits
                                        .iter()
                                        .cloned()
                                        .map(|e| match e {
                                            lsp_types::OneOf::Left(te) => te,
                                            lsp_types::OneOf::Right(ae) => ae.text_edit,
                                        })
                                        .collect::<Vec<_>>(),
                                )
                                .await?;
                            }
                            lsp_types::DocumentChangeOperation::Op(op) => match op {
                                lsp_types::ResourceOp::Create(create) => {
                                    let url = Url::parse(&create.uri.to_string())?;
                                    if let Ok(path) = url.to_file_path() {
                                        tokio::fs::write(&path, "").await?;
                                    }
                                }
                                lsp_types::ResourceOp::Rename(rename) => {
                                    let old_url = Url::parse(&rename.old_uri.to_string())?;
                                    let new_url = Url::parse(&rename.new_uri.to_string())?;
                                    if let (Ok(old_path), Ok(new_path)) =
                                        (old_url.to_file_path(), new_url.to_file_path())
                                    {
                                        tokio::fs::rename(old_path, new_path).await?;
                                    }
                                }
                                lsp_types::ResourceOp::Delete(delete) => {
                                    let url = Url::parse(&delete.uri.to_string())?;
                                    if let Ok(path) = url.to_file_path() {
                                        if path.is_file() {
                                            tokio::fs::remove_file(&path).await?;
                                        } else if path.is_dir() {
                                            tokio::fs::remove_dir_all(&path).await?;
                                        }
                                    }
                                }
                            },
                        }
                    }
                }
            }
        }
        Ok(())
    }

    async fn apply_text_edits_to_uri(
        &self,
        uri: &lsp_types::Uri,
        edits: &[lsp_types::TextEdit],
    ) -> anyhow::Result<()> {
        let url = Url::parse(&uri.to_string())?;
        if let Ok(path) = url.to_file_path() {
            let content = tokio::fs::read_to_string(&path).await?;
            let mut rope = ropey::Rope::from_str(&content);

            let mut sorted_edits = edits.to_vec();
            // Sort in reverse order (bottom-up, right-to-left) to avoid invalidating offsets
            sorted_edits.sort_by(|a, b| {
                b.range
                    .start
                    .line
                    .cmp(&a.range.start.line)
                    .then(b.range.start.character.cmp(&a.range.start.character))
            });

            for edit in sorted_edits {
                let start_line = edit.range.start.line as usize;
                let start_cu = edit.range.start.character as usize;
                let end_line = edit.range.end.line as usize;
                let end_cu = edit.range.end.character as usize;

                let start_char_idx = if start_line < rope.len_lines() {
                    let line_start = rope.line_to_char(start_line);
                    let line_slice = rope.line(start_line);
                    let cu_offset = start_cu.min(line_slice.len_utf16_cu());
                    line_start + line_slice.utf16_cu_to_char(cu_offset)
                } else {
                    rope.len_chars()
                };

                let end_char_idx = if end_line < rope.len_lines() {
                    let line_start = rope.line_to_char(end_line);
                    let line_slice = rope.line(end_line);
                    let cu_offset = end_cu.min(line_slice.len_utf16_cu());
                    line_start + line_slice.utf16_cu_to_char(cu_offset)
                } else {
                    rope.len_chars()
                };

                if start_char_idx <= end_char_idx {
                    rope.remove(start_char_idx..end_char_idx);
                    rope.insert(start_char_idx, &edit.new_text);
                }
            }

            tokio::fs::write(&path, rope.to_string()).await?;
        }
        Ok(())
    }
}

#[tool_router]
impl MyHandler {
    #[tool(
        description = "Subscribes the AI to receive proactive notifications of diagnostics. When a new error appears, the system will send an Enriched Diagnostic Object that can be used with other tools."
    )]
    async fn editor_subscribe_to_diagnostics(&self) -> String {
        self.subscribed_to_diagnostics.store(true, Ordering::SeqCst);
        serde_json::to_string(&serde_json::json!({
            "status": "Subscribed",
            "range": { "start": { "line": 0, "character": 0 }, "end": { "line": u32::MAX, "character": u32::MAX } },
            "scope": "workspace"
        }))
        .unwrap()
    }

    #[tool(
        description = "For a given diagnostic object, this tool fetches a list of potential 'Code Actions' or 'Quick Fixes' that the Language Server recommends."
    )]
    async fn code_get_actions_for_diagnostic(
        &self,
        Parameters(args): Parameters<GetActionsArgs>,
    ) -> Result<String, ErrorData> {
        if let Some(lsp) = self.get_lsp_client().await {
            let mut lsp = lsp.lock().await;
            let abs_path = self.to_absolute_path(&args.diagnostic_object.path);
            let _ = lsp.ensure_file_open(&abs_path).await;
            let lsp_params = lsp_types::CodeActionParams {
                text_document: lsp_types::TextDocumentIdentifier {
                    uri: Url::from_file_path(abs_path)
                        .unwrap()
                        .to_string()
                        .parse()
                        .unwrap(),
                },
                range: serde_json::from_value(args.diagnostic_object.diagnostic["range"].clone())
                    .unwrap_or_default(),
                context: lsp_types::CodeActionContext {
                    diagnostics: vec![
                        serde_json::from_value(args.diagnostic_object.diagnostic.clone()).unwrap(),
                    ],
                    only: None,
                    trigger_kind: None,
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            };
            let result = lsp
                .send_request::<lsp_types::request::CodeActionRequest>(lsp_params)
                .await
                .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
            Ok(serde_json::to_string(&result).unwrap())
        } else {
            Err(ErrorData::internal_error("LSP not available", None))
        }
    }

    #[tool(
        description = "Applies a specific Code Action chosen from the list provided by code_get_actions_for_diagnostic. This tool executes the change directly to the codebase."
    )]
    async fn code_apply_action(
        &self,
        Parameters(args): Parameters<ApplyActionArgs>,
    ) -> Result<String, ErrorData> {
        if let Some(lsp_client) = self.get_lsp_client().await {
            if let Some(edit) = args.action_object.edit {
                let workspace_edit: lsp_types::WorkspaceEdit =
                    serde_json::from_value(edit).unwrap();
                self.apply_workspace_edit(&workspace_edit)
                    .await
                    .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
                Ok(format!(
                    "Applied edit for action '{}'.",
                    args.action_object.title
                ))
            } else if let Some(command) = args.action_object.command {
                let mut lsp = lsp_client.lock().await;
                let lsp_command: lsp_types::Command = serde_json::from_value(command).unwrap();
                let execute_params = lsp_types::ExecuteCommandParams {
                    command: lsp_command.command,
                    arguments: lsp_command.arguments.unwrap_or_default(),
                    work_done_progress_params: Default::default(),
                };
                let result = lsp
                    .send_request::<lsp_types::request::ExecuteCommand>(execute_params)
                    .await
                    .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
                Ok(format!(
                    "Executed command for action '{}': {:?}",
                    args.action_object.title, result
                ))
            } else {
                Ok(format!(
                    "Action '{}' has no edit or command to apply.",
                    args.action_object.title
                ))
            }
        } else {
            Err(ErrorData::internal_error("LSP not available", None))
        }
    }

    #[tool(
        description = "Finds the definition of the symbol at a specific cursor position. Use this to navigate directly to its source to understand what it does."
    )]
    async fn editor_get_definition(
        &self,
        Parameters(args): Parameters<PathLineCharArgs>,
    ) -> Result<String, ErrorData> {
        if let Some(lsp) = self.get_lsp_client().await {
            let mut lsp = lsp.lock().await;
            let abs_path = self.to_absolute_path(&args.path);
            let _ = lsp.ensure_file_open(&abs_path).await;
            let lsp_params = lsp_types::GotoDefinitionParams {
                text_document_position_params: lsp_types::TextDocumentPositionParams {
                    text_document: lsp_types::TextDocumentIdentifier {
                        uri: Url::from_file_path(abs_path)
                            .unwrap()
                            .to_string()
                            .parse()
                            .unwrap(),
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
                .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
            let mut results = Vec::new();
            if let Some(res) = result {
                match res {
                    lsp_types::GotoDefinitionResponse::Scalar(location) => {
                        results.push(models::Location {
                            path: Url::parse(&location.uri.to_string())
                                .unwrap()
                                .to_file_path()
                                .unwrap()
                                .to_string_lossy()
                                .to_string(),
                            line: location.range.start.line,
                            character: location.range.start.character,
                            hover_info: None,
                        })
                    }
                    lsp_types::GotoDefinitionResponse::Array(locations) => {
                        for loc in locations {
                            results.push(models::Location {
                                path: Url::parse(&loc.uri.to_string())
                                    .unwrap()
                                    .to_file_path()
                                    .unwrap()
                                    .to_string_lossy()
                                    .to_string(),
                                line: loc.range.start.line,
                                character: loc.range.start.character,
                                hover_info: None,
                            })
                        }
                    }
                    lsp_types::GotoDefinitionResponse::Link(links) => {
                        for link in links {
                            results.push(models::Location {
                                path: Url::parse(&link.target_uri.to_string())
                                    .unwrap()
                                    .to_file_path()
                                    .unwrap()
                                    .to_string_lossy()
                                    .to_string(),
                                line: link.target_range.start.line,
                                character: link.target_range.start.character,
                                hover_info: None,
                            })
                        }
                    }
                }
            }
            Ok(serde_json::to_string(&results).unwrap())
        } else {
            Err(ErrorData::internal_error("LSP not available", None))
        }
    }

    #[tool(
        description = "Finds all references to the symbol at a specific cursor position throughout the workspace. Use this for critical impact analysis before proposing changes."
    )]
    async fn editor_get_references(
        &self,
        Parameters(args): Parameters<PathLineCharArgs>,
    ) -> Result<String, ErrorData> {
        if let Some(lsp) = self.get_lsp_client().await {
            let mut lsp = lsp.lock().await;
            let abs_path = self.to_absolute_path(&args.path);
            let _ = lsp.ensure_file_open(&abs_path).await;
            let lsp_params = lsp_types::ReferenceParams {
                text_document_position: lsp_types::TextDocumentPositionParams {
                    text_document: lsp_types::TextDocumentIdentifier {
                        uri: Url::from_file_path(abs_path)
                            .unwrap()
                            .to_string()
                            .parse()
                            .unwrap(),
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
                .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
            let mut results = Vec::new();
            if let Some(locations) = result {
                for location in locations {
                    results.push(models::Location {
                        path: Url::parse(&location.uri.to_string())
                            .unwrap()
                            .to_file_path()
                            .unwrap()
                            .to_string_lossy()
                            .to_string(),
                        line: location.range.start.line,
                        character: location.range.start.character,
                        hover_info: None,
                    });
                }
            }
            Ok(serde_json::to_string(&results).unwrap())
        } else {
            Err(ErrorData::internal_error("LSP not available", None))
        }
    }

    #[tool(
        description = "A powerful, unified tool for locating symbols and retrieving their information. It can search within a specific file or across the entire workspace. Use this as your canonical tool for finding code."
    )]
    async fn code_find_symbol(
        &self,
        Parameters(args): Parameters<FindSymbolArgs>,
    ) -> Result<String, ErrorData> {
        if let Some(lsp_client) = self.get_lsp_client().await {
            let mut lsp = lsp_client.lock().await;
            let mut search_file_uri: Option<lsp_types::Uri> = None;
            if let Some(file_hint) = &args.file {
                let abs_path = self.to_absolute_path(file_hint);
                let matches = if abs_path.exists() {
                    vec![abs_path]
                } else {
                    let root_dir = self
                        .root_uri
                        .as_ref()
                        .and_then(|u| Url::parse(&u.to_string()).ok()?.to_file_path().ok())
                        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                    Self::find_files(&root_dir, file_hint)
                };

                if matches.len() > 1 {
                    return Err(ErrorData::internal_error(
                        format!(
                            "Ambiguous file name '{}'. Matches: {:?}",
                            file_hint, matches
                        ),
                        None,
                    ));
                } else if matches.is_empty() {
                    return Err(ErrorData::internal_error(
                        format!("File '{}' not found", file_hint),
                        None,
                    ));
                }
                search_file_uri = Some(
                    Url::from_file_path(&matches[0])
                        .unwrap()
                        .to_string()
                        .parse()
                        .unwrap(),
                );
            }
            let mut lsp_results: Vec<(
                String,
                lsp_types::SymbolKind,
                lsp_types::OneOf<lsp_types::Location, lsp_types::Uri>,
            )> = Vec::new();
            if let Some(uri) = search_file_uri {
                let doc_params = lsp_types::DocumentSymbolParams {
                    text_document: lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                    work_done_progress_params: Default::default(),
                    partial_result_params: Default::default(),
                };
                if let Ok(Some(response)) = lsp
                    .send_request::<lsp_types::request::DocumentSymbolRequest>(doc_params)
                    .await
                {
                    match response {
                        lsp_types::DocumentSymbolResponse::Flat(symbols) => {
                            for s in symbols {
                                if s.name.contains(&args.symbol_name) {
                                    lsp_results.push((
                                        s.name,
                                        s.kind,
                                        lsp_types::OneOf::Left(s.location),
                                    ));
                                }
                            }
                        }
                        lsp_types::DocumentSymbolResponse::Nested(symbols) => {
                            MyHandler::collect_nested_matching_symbols(
                                &symbols,
                                &args.symbol_name,
                                &uri,
                                &mut lsp_results,
                            )
                        }
                    }
                }
                if let Some(hint) = args.location_hint {
                    lsp_results.sort_by_key(|(_, _, loc)| {
                        if let lsp_types::OneOf::Left(l) = loc {
                            let d_line = (l.range.start.line as i32 - hint.line as i32).abs();
                            let d_char =
                                (l.range.start.character as i32 - hint.character as i32).abs();
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
                if let Ok(Some(response)) = lsp
                    .send_request::<lsp_types::request::WorkspaceSymbolRequest>(lsp_params)
                    .await
                {
                    match response {
                        lsp_types::WorkspaceSymbolResponse::Nested(s) => {
                            for symbol in s {
                                lsp_results.push((
                                    symbol.name,
                                    symbol.kind,
                                    match symbol.location {
                                        lsp_types::OneOf::Left(l) => lsp_types::OneOf::Left(l),
                                        lsp_types::OneOf::Right(l) => {
                                            lsp_types::OneOf::Right(l.uri)
                                        }
                                    },
                                ));
                            }
                        }
                        lsp_types::WorkspaceSymbolResponse::Flat(s) => {
                            for symbol in s {
                                lsp_results.push((
                                    symbol.name,
                                    symbol.kind,
                                    lsp_types::OneOf::Left(symbol.location),
                                ));
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
                                text_document: lsp_types::TextDocumentIdentifier {
                                    uri: location.uri.clone(),
                                },
                                position: location.range.start,
                            },
                            work_done_progress_params: Default::default(),
                        };
                        if let Ok(Some(hover)) = lsp
                            .send_request::<lsp_types::request::HoverRequest>(hover_params)
                            .await
                        {
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
            Ok(serde_json::to_string(&results).unwrap())
        } else {
            Err(ErrorData::internal_error("LSP not available", None))
        }
    }

    #[tool(
        description = "Shows the methods, attributes, or functions defined within a given module, class, or other symbol. Use this to understand the internal structure without reading the entire body."
    )]
    async fn code_show_sub_symbol(
        &self,
        Parameters(args): Parameters<ShowSubSymbolArgs>,
    ) -> Result<String, ErrorData> {
        if let Some(lsp_client) = self.get_lsp_client().await {
            let mut lsp = lsp_client.lock().await;
            let abs_path = self.to_absolute_path(&args.symbol_path);
            let _ = lsp.ensure_file_open(&abs_path).await;
            let uri: lsp_types::Uri = Url::from_file_path(abs_path)
                .unwrap()
                .to_string()
                .parse()
                .unwrap();
            let lsp_params = lsp_types::DocumentSymbolParams {
                text_document: lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            };
            let result = lsp
                .send_request::<lsp_types::request::DocumentSymbolRequest>(lsp_params)
                .await
                .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
            let mut results = Vec::new();
            if let Some(response) = result {
                let path = Url::parse(&uri.to_string())
                    .unwrap()
                    .to_file_path()
                    .unwrap();
                let content = tokio::fs::read_to_string(&path).await.unwrap_or_default();
                let lines: Vec<&str> = content.lines().collect();
                match response {
                    lsp_types::DocumentSymbolResponse::Flat(symbols) => {
                        for symbol in symbols {
                            let signature = lines
                                .get(symbol.location.range.start.line as usize)
                                .cloned()
                                .map(MyHandler::clean_signature)
                                .unwrap_or_default();
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
                        if let Some(target) =
                            MyHandler::find_symbol_in_hierarchy(&symbols, &args.symbol)
                        {
                            if let Some(children) = &target.children {
                                MyHandler::collect_nested_symbol_members(
                                    children,
                                    0,
                                    args.level,
                                    &mut results,
                                    &lines,
                                );
                                for r in &mut results {
                                    r.name = format!("{}::{}", args.symbol, r.name);
                                }
                            }
                        } else {
                            return Err(ErrorData::internal_error(
                                format!("Symbol '{}' not found in file", args.symbol),
                                None,
                            ));
                        }
                    }
                }
            }
            Ok(serde_json::to_string(&results).unwrap())
        } else {
            Err(ErrorData::internal_error("LSP not available", None))
        }
    }

    #[tool(
        description = "Retrieves a list of suggested code completions at a specific cursor position. Use this when completing partial names or exploring available members."
    )]
    async fn code_get_completions(
        &self,
        Parameters(args): Parameters<GetCompletionsArgs>,
    ) -> Result<String, ErrorData> {
        if let Some(lsp_client) = self.get_lsp_client().await {
            let mut lsp = lsp_client.lock().await;
            let abs_path = self.to_absolute_path(&args.path);
            let _ = lsp.ensure_file_open(&abs_path).await;
            let lsp_params = lsp_types::CompletionParams {
                text_document_position: lsp_types::TextDocumentPositionParams {
                    text_document: lsp_types::TextDocumentIdentifier {
                        uri: Url::from_file_path(abs_path)
                            .unwrap()
                            .to_string()
                            .parse()
                            .unwrap(),
                    },
                    position: lsp_types::Position {
                        line: args.line,
                        character: args.character,
                    },
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
                context: None,
            };
            let result = lsp
                .send_request::<lsp_types::request::Completion>(lsp_params)
                .await
                .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

            let mut results = Vec::new();
            if let Some(response) = result {
                let items = match response {
                    lsp_types::CompletionResponse::Array(items) => items,
                    lsp_types::CompletionResponse::List(list) => list.items,
                };
                for item in items {
                    results.push(models::CompletionItem {
                        label: item.label,
                        kind: item.kind.map(|k| format!("{:?}", k)),
                        detail: item.detail,
                        documentation: item.documentation.map(|d| match d {
                            lsp_types::Documentation::String(s) => s,
                            lsp_types::Documentation::MarkupContent(m) => m.value,
                        }),
                        sort_text: item.sort_text,
                        filter_text: item.filter_text,
                        insert_text: item.insert_text,
                    });
                }
            }
            Ok(serde_json::to_string(&results).unwrap())
        } else {
            Err(ErrorData::internal_error("LSP not available", None))
        }
    }

    #[tool(
        description = "Initiates a safe, workspace-wide rename of a symbol. This handles finding the symbol, getting all references, and applying changes autonomously."
    )]
    async fn refactor_interactive_rename(
        &self,
        Parameters(args): Parameters<InteractiveRenameArgs>,
    ) -> Result<String, ErrorData> {
        if let Some(lsp_client) = self.get_lsp_client().await {
            let mut lsp = lsp_client.lock().await;
            let abs_path = self.to_absolute_path(&args.path);
            let _ = lsp.ensure_file_open(&abs_path).await;
            let position = args
                .symbol_to_find
                .location_hint
                .clone()
                .unwrap_or(models::Position {
                    line: 0,
                    character: 0,
                });
            let lsp_params = lsp_types::RenameParams {
                text_document_position: lsp_types::TextDocumentPositionParams {
                    text_document: lsp_types::TextDocumentIdentifier {
                        uri: Url::from_file_path(abs_path)
                            .unwrap()
                            .to_string()
                            .parse()
                            .unwrap(),
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
                .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
            if let Some(edit) = result {
                self.apply_workspace_edit(&edit)
                    .await
                    .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
                Ok(serde_json::to_string(
                    &serde_json::json!({ "status": "Applied", "message": "Rename successful." }),
                )
                .unwrap())
            } else {
                Ok("LSP returned no edits for rename".to_string())
            }
        } else {
            Err(ErrorData::internal_error("LSP not available", None))
        }
    }

    #[tool(
        description = "Commands the LSP to display all diagnostics (errors and warnings) for the entire workspace to the AI."
    )]
    async fn ui_show_workspace_diagnostics(
        &self,
        _context: RequestContext<RoleServer>,
    ) -> Result<String, ErrorData> {
        let _ = _context
            .peer
            .send_notification(ServerNotification::CustomNotification(CustomNotification {
                method: "ui/showDiagnostics".to_string(),
                params: None,
                extensions: Default::default(),
            }))
            .await;
        Ok("Workspace diagnostics UI opened".to_string())
    }
}

#[prompt_router]
impl MyHandler {
    #[prompt(
        name = "dynamic_guidance",
        description = "Provides just-in-time guidance based on context."
    )]
    async fn dynamic_guidance(
        &self,
        Parameters(args): Parameters<serde_json::Value>,
    ) -> Vec<PromptMessage> {
        let query = args
            .get("query")
            .and_then(|q| q.as_str())
            .unwrap_or_default();
        let mut hints = Vec::new();
        if query.contains("test") || query.contains("failing") {
            hints.push("[System Note: To debug a failing test, your first step should be to create a terminal and run the test command to capture its output. Do not guess the cause of the failure.]");
        }
        if query.contains("not found") || query.contains("undefined") {
            hints.push("[System Note: Upon receiving a 'symbol not found' diagnostic, use code_find_symbol to check for misspellings or related symbols in other files.]");
        }
        vec![PromptMessage::new_text(
            PromptMessageRole::User,
            hints.join("\n"),
        )]
    }
}

#[tool_handler]
#[prompt_handler]
impl ServerHandler for MyHandler {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::new(
            ServerCapabilities::builder()
                .enable_prompts()
                .enable_tools()
                .build(),
        );
        info.server_info.name = "lsp-mcp".into();
        info.server_info.version = "1.16".into();
        info.instructions = Some("You are Gemini CLI, acting as an advanced semantic agent.
- DOCTRINE 1 (PLAN): Generate and present a multi-step plan before non-trivial tool calls.
- DOCTRINE 2 (VERIFY): After writing files, verify using terminal commands.
- DOCTRINE 3 (EXECUTE): Execute deterministic tasks (e.g., LSP-based refactorings or fixes) directly without awaiting human approval for each step.
- DOCTRINE 4 (DEBUG): Use 'code_find_symbol' as the first step for 'undefined' or 'not found' errors.
[System Action: The server provides context-aware hints via the 'dynamic_guidance' prompt tool when you detect keywords like 'test', 'failing', or 'not found'].".to_string());
        info.server_info.description =
            Some("LSP-MCP server implementing AI Development Specification 1.16".to_string());
        info.server_info.title = Some("LSP MCP Server".to_string());
        info
    }
}

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Start in manual mode
    #[arg(long)]
    manual: bool,

    /// One-shot tool call (manual mode only)
    #[arg(long)]
    call: Option<String>,

    /// JSON arguments for the tool call (manual mode only)
    #[arg(long)]
    args: Option<String>,

    /// The command to run the LSP server
    #[arg(long, default_value = "rust-analyzer")]
    lsp_command: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let (notification_tx, mut notification_rx) =
        tokio::sync::mpsc::channel::<serde_json::Value>(100);
    let current_dir = std::env::current_dir()?;
    let root_uri: Option<lsp_types::Uri> = Url::from_directory_path(&current_dir)
        .ok()
        .map(|u| u.to_string().parse().unwrap());

    let handler = MyHandler::new(notification_tx, root_uri, cli.lsp_command.clone(), None);

    if cli.manual {
        let peer = mock_server::dummy_peer(handler.clone()).await;
        if let Some(tool_name) = cli.call {
            let arguments: Option<serde_json::Map<String, serde_json::Value>> =
                cli.args.as_ref().and_then(|a| serde_json::from_str(a).ok());
            let mut params = CallToolRequestParams::new(tool_name);
            params.arguments = arguments.map(|a| a.into_iter().collect());
            let context = RequestContext::new(RequestId::Number(0), peer.clone());
            match handler.call_tool(params, context).await {
                Ok(res) => println!("{}", serde_json::to_string_pretty(&res).unwrap()),
                Err(e) => {
                    eprintln!("Error: {:?}", e);
                    if let Some(client) = handler.lsp_client.lock().await.as_ref() {
                        let mut lsp = client.lock().await;
                        let _ = lsp.shutdown().await;
                    }
                    std::process::exit(1);
                }
            }
            if let Some(client) = handler.lsp_client.lock().await.as_ref() {
                let mut lsp = client.lock().await;
                let _ = lsp.shutdown().await;
            }
            return Ok(());
        }

        eprintln!("Manual mode started. Type 'help' for available tools or 'exit' to quit.");
        let mut lines = std::io::stdin().lines();
        while let Some(Ok(line)) = lines.next() {
            let line = line.trim();
            if line == "exit" || line == "quit" {
                break;
            }
            if line == "help" {
                let context = RequestContext::new(RequestId::Number(0), peer.clone());
                let tools = handler.list_tools(None, context).await.unwrap();
                for t in tools.tools {
                    eprintln!("- {}: {}", t.name, t.description.unwrap_or_default());
                }
                continue;
            }
            if let Some((name, json_args)) = line.split_once(' ') {
                let arguments: Option<serde_json::Map<String, serde_json::Value>> =
                    serde_json::from_str(json_args).ok();
                let mut params = CallToolRequestParams::new(name.to_string());
                params.arguments = arguments.map(|a| a.into_iter().collect());
                let context = RequestContext::new(RequestId::Number(0), peer.clone());
                match handler.call_tool(params, context).await {
                    Ok(res) => println!("{}", serde_json::to_string_pretty(&res).unwrap()),
                    Err(e) => eprintln!("Error: {:?}", e),
                }
            } else {
                eprintln!("Usage: <tool_name> <json_arguments>");
            }
        }
        if let Some(client) = handler.lsp_client.lock().await.as_ref() {
            let mut lsp = client.lock().await;
            let _ = lsp.shutdown().await;
        }
        return Ok(());
    }

    let handler_for_notif = handler.clone();

    let transport = rmcp::transport::io::stdio();
    let server = handler.serve(transport).await?;
    let peer = server.clone();

    tokio::spawn(async move {
        while let Some(mut notif) = notification_rx.recv().await {
            if let Some(method) = notif.get("method").and_then(|m| m.as_str())
                && method == "textDocument/publishDiagnostics"
                && let Some(params) = notif.get_mut("params")
            {
                let uri_str = params
                    .get("uri")
                    .and_then(|u| u.as_str())
                    .map(|s| s.to_string());
                if let (Some(uri_str), Some(diagnostics)) = (
                    uri_str,
                    params.get_mut("diagnostics").and_then(|d| d.as_array_mut()),
                ) && let Ok(url) = Url::parse(&uri_str)
                    && let Ok(path) = url.to_file_path()
                    && let Ok(content) = tokio::fs::read_to_string(&path).await
                {
                    let lines: Vec<&str> = content.lines().collect();
                    let mut doc_symbols = Vec::new();
                    let active_lsp_client =
                        handler_for_notif.lsp_client.lock().await.as_ref().cloned();
                    if let Some(lsp_client) = active_lsp_client {
                        let mut lsp = lsp_client.lock().await;
                        let lsp_uri: lsp_types::Uri = uri_str.parse().unwrap();
                        let symbol_params = lsp_types::DocumentSymbolParams {
                            text_document: lsp_types::TextDocumentIdentifier { uri: lsp_uri },
                            work_done_progress_params: Default::default(),
                            partial_result_params: Default::default(),
                        };
                        if let Ok(Some(response)) = lsp
                            .send_request::<lsp_types::request::DocumentSymbolRequest>(
                                symbol_params,
                            )
                            .await
                        {
                            match response {
                                lsp_types::DocumentSymbolResponse::Flat(s) => {
                                    doc_symbols = s
                                        .into_iter()
                                        .map(|si| (si.name, si.location.range))
                                        .collect()
                                }
                                lsp_types::DocumentSymbolResponse::Nested(s) => {
                                    fn flatten(
                                        symbols: Vec<lsp_types::DocumentSymbol>,
                                        target: &mut Vec<(String, lsp_types::Range)>,
                                    ) {
                                        for s in symbols {
                                            target.push((s.name, s.range));
                                            if let Some(children) = s.children {
                                                flatten(children, target);
                                            }
                                        }
                                    }
                                    flatten(s, &mut doc_symbols);
                                }
                            }
                        }
                    }
                    MyHandler::enrich_diagnostics(diagnostics, &lines, &doc_symbols, &path);
                }

                if handler_for_notif
                    .subscribed_to_diagnostics
                    .load(Ordering::SeqCst)
                {
                    let _ = peer
                        .send_notification(ServerNotification::CustomNotification(
                            CustomNotification {
                                method: "notifications/diagnostics".to_string(),
                                params: Some(serde_json::Value::Object(
                                    notif["params"].as_object().cloned().unwrap_or_default(),
                                )),
                                extensions: Default::default(),
                            },
                        ))
                        .await;
                }
            }
        }
    });

    eprintln!("LSP-MCP server running on stdio...");
    server.waiting().await?;

    Ok(())
}
