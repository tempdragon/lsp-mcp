use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EnrichedDiagnostic {
    pub diagnostic: serde_json::Value,
    pub symbol_name: Option<String>,
    pub line_content: Option<String>,
    pub path: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Location {
    pub path: String,
    pub line: u32,
    pub character: u32,
    pub hover_info: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CodeAction {
    pub title: String,
    pub kind: Option<String>,
    pub command: Option<serde_json::Value>,
    pub edit: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SymbolMember {
    pub name: String,
    pub signature: String,
    pub line: u32,
    pub character: u32,
    pub kind: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RefactorSession {
    pub id: String,
    pub description: String,
    pub proposals: Vec<RefactorProposal>,
}

#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RefactorProposal {
    pub id: String,
    pub change: serde_json::Value,
    pub approved: bool,
}
