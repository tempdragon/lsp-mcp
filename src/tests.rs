use crate::sessions::SessionManager;
use crate::terminals::TerminalManager;
use lsp_types::WorkspaceEdit;
use std::collections::HashMap;
use url::Url;

#[tokio::test]
async fn test_session_manager() {
    let sm = SessionManager::new();
    let id = sm.start_session("Test session".to_string());
    assert!(!id.is_empty());

    let edit = WorkspaceEdit {
        changes: Some(HashMap::new()),
        document_changes: None,
        change_annotations: None,
    };

    let prop_id = sm.propose_change(&id, edit).unwrap();
    assert!(!prop_id.is_empty());

    // Test approval
    assert!(sm.approve_proposal(&id, &prop_id));
    
    let approved = sm.apply_approved(&id).await.unwrap();
    assert_eq!(approved.len(), 1);
}

#[tokio::test]
async fn test_apply_workspace_edit() {
    let sm = SessionManager::new();
    let temp_file = std::env::temp_dir().join("test_edit.txt");
    tokio::fs::write(&temp_file, "line1\nline2\nline3").await.unwrap();

    let uri = Url::from_file_path(&temp_file).unwrap();
    let mut changes = HashMap::new();
    changes.insert(
        uri.to_string().parse().unwrap(),
        vec![lsp_types::TextEdit {
            range: lsp_types::Range {
                start: lsp_types::Position {
                    line: 1,
                    character: 0,
                },
                end: lsp_types::Position {
                    line: 1,
                    character: 5,
                },
            },
            new_text: "modified".to_string(),
        }],
    );

    let edit = WorkspaceEdit {
        changes: Some(changes),
        document_changes: None,
        change_annotations: None,
    };

    sm.apply_workspace_edit(&edit).await.unwrap();

    let content = tokio::fs::read_to_string(&temp_file).await.unwrap();
    assert_eq!(content, "line1\nmodified\nline3");

    tokio::fs::remove_file(temp_file).await.unwrap();
}

#[tokio::test]
async fn test_terminal_manager() {
    let tm = TerminalManager::new();
    let id = tm.create_terminal().unwrap();
    assert!(!id.is_empty());

    let (stdout, stderr) = tm.run_command(&id, "ls").await.unwrap();
    assert!(!stdout.is_empty() || !stderr.is_empty());

    assert!(tm.close_terminal(&id));
}
