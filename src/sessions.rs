use crate::models::{RefactorProposal, RefactorSession};
use dashmap::DashMap;
use lsp_types::WorkspaceEdit;
use url::Url;
use uuid::Uuid;

pub struct SessionManager {
    sessions: DashMap<String, RefactorSession>,
}

impl SessionManager {
    pub fn new() -> Self {
        Self {
            sessions: DashMap::new(),
        }
    }

    pub fn start_session(&self, description: String) -> String {
        let id = Uuid::new_v4().to_string();
        let session = RefactorSession {
            id: id.clone(),
            description,
            proposals: Vec::new(),
        };
        self.sessions.insert(id.clone(), session);
        id
    }

    pub fn propose_change(&self, session_id: &str, change: WorkspaceEdit) -> Option<String> {
        if let Some(mut session) = self.sessions.get_mut(session_id) {
            let proposal_id = Uuid::new_v4().to_string();
            session.proposals.push(RefactorProposal {
                id: proposal_id.clone(),
                change: Some(serde_json::to_value(change).unwrap()),
                command: None,
                approved: false,
            });
            Some(proposal_id)
        } else {
            None
        }
    }

    pub fn propose_command(&self, session_id: &str, command: lsp_types::Command) -> Option<String> {
        if let Some(mut session) = self.sessions.get_mut(session_id) {
            let proposal_id = Uuid::new_v4().to_string();
            session.proposals.push(RefactorProposal {
                id: proposal_id.clone(),
                change: None,
                command: Some(serde_json::to_value(command).unwrap()),
                approved: false,
            });
            Some(proposal_id)
        } else {
            None
        }
    }

    pub fn approve_proposal(&self, session_id: &str, proposal_id: &str) -> bool {
        if let Some(mut session) = self.sessions.get_mut(session_id) {
            if let Some(proposal) = session.proposals.iter_mut().find(|p| p.id == proposal_id) {
                proposal.approved = true;
                return true;
            }
        }
        false
    }

    pub async fn apply_approved(&self, session_id: &str) -> Option<(Vec<serde_json::Value>, Vec<serde_json::Value>)> {
        if let Some(session) = self.sessions.remove(session_id) {
            let mut applied_edits = Vec::new();
            let mut applied_commands = Vec::new();

            for proposal in session.1.proposals.into_iter().filter(|p| p.approved) {
                if let Some(change_val) = proposal.change {
                    let edit: WorkspaceEdit = serde_json::from_value(change_val.clone()).unwrap();
                    if self.apply_workspace_edit(&edit).await.is_ok() {
                        applied_edits.push(change_val);
                    }
                }
                if let Some(command_val) = proposal.command {
                    applied_commands.push(command_val);
                }
            }

            Some((applied_edits, applied_commands))
        } else {
            None
        }
    }

    pub async fn apply_workspace_edit(&self, edit: &WorkspaceEdit) -> anyhow::Result<()> {
        if let Some(changes) = &edit.changes {
            for (uri, edits) in changes {
                self.apply_text_edits_to_uri(uri, edits).await?;
            }
        }
        if let Some(document_changes) = &edit.document_changes {
            match document_changes {
                lsp_types::DocumentChanges::Edits(edits) => {
                    for edit in edits {
                        self.apply_text_edits_to_uri(&edit.text_document.uri, &edit.edits.iter().cloned().map(|e| match e {
                            lsp_types::OneOf::Left(te) => te,
                            lsp_types::OneOf::Right(ae) => ae.text_edit,
                        }).collect::<Vec<_>>()).await?;
                    }
                }
                lsp_types::DocumentChanges::Operations(ops) => {
                    for op in ops {
                        match op {
                            lsp_types::DocumentChangeOperation::Edit(edit) => {
                                self.apply_text_edits_to_uri(&edit.text_document.uri, &edit.edits.iter().cloned().map(|e| match e {
                                    lsp_types::OneOf::Left(te) => te,
                                    lsp_types::OneOf::Right(ae) => ae.text_edit,
                                }).collect::<Vec<_>>()).await?;
                            }
                            lsp_types::DocumentChangeOperation::Op(op) => {
                                match op {
                                    lsp_types::ResourceOp::Create(create) => {
                                        let url = Url::parse(&create.uri.to_string())?;
                                        if let Ok(path) = url.to_file_path() {
                                            tokio::fs::write(&path, "").await?;
                                        }
                                    }
                                    lsp_types::ResourceOp::Rename(rename) => {
                                        let old_url = Url::parse(&rename.old_uri.to_string())?;
                                        let new_url = Url::parse(&rename.new_uri.to_string())?;
                                        if let (Ok(old_path), Ok(new_path)) = (old_url.to_file_path(), new_url.to_file_path()) {
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
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    async fn apply_text_edits_to_uri(&self, uri: &lsp_types::Uri, edits: &[lsp_types::TextEdit]) -> anyhow::Result<()> {
        let url = Url::parse(&uri.to_string())?;
        if let Ok(path) = url.to_file_path() {
            let mut content = tokio::fs::read_to_string(&path).await?;
            let mut lines: Vec<String> = content.lines().map(|s| s.to_string()).collect();

            // Apply edits in reverse order to maintain line/char offsets
            let mut sorted_edits = edits.to_vec();
            sorted_edits.sort_by(|a, b| {
                b.range
                    .start
                    .line
                    .cmp(&a.range.start.line)
                    .then(b.range.start.character.cmp(&a.range.start.character))
            });

            for edit in sorted_edits {
                self.apply_text_edit(&mut lines, &edit);
            }

            content = lines.join("\n");
            tokio::fs::write(&path, content).await?;
        }
        Ok(())
    }

    fn apply_text_edit(&self, lines: &mut Vec<String>, edit: &lsp_types::TextEdit) {
        let start_line = edit.range.start.line as usize;
        let start_char = edit.range.start.character as usize;
        let end_line = edit.range.end.line as usize;
        let end_char = edit.range.end.character as usize;

        if start_line == end_line {
            if let Some(line) = lines.get_mut(start_line) {
                let mut new_line = String::new();
                new_line.push_str(&line[..start_char]);
                new_line.push_str(&edit.new_text);
                new_line.push_str(&line[end_char..]);
                *line = new_line;
            }
        } else {
            // Multi-line edit
            let first_part = lines[start_line][..start_char].to_string();
            let last_part = lines[end_line][end_char..].to_string();

            let mut new_text_lines: Vec<String> =
                edit.new_text.lines().map(|s| s.to_string()).collect();
            if edit.new_text.ends_with('\n') {
                new_text_lines.push(String::new());
            }

            if new_text_lines.is_empty() {
                new_text_lines.push(first_part + &last_part);
            } else {
                new_text_lines[0] = first_part + &new_text_lines[0];
                let last_idx = new_text_lines.len() - 1;
                new_text_lines[last_idx].push_str(&last_part);
            }

            lines.splice(start_line..=end_line, new_text_lines);
        }
    }
}
