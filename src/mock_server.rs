use async_trait::async_trait;
use rust_mcp_sdk::auth::AuthInfo;
use rust_mcp_sdk::error::McpSdkError;
use rust_mcp_sdk::schema::{
    ClientMessage, InitializeRequestParams, InitializeResult, MessageFromServer,
    NotificationFromServer, RequestId, ServerMessage,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLockReadGuard;

pub struct MockMcpServer {
    pub info: InitializeResult,
}
impl MockMcpServer {
    pub fn new() -> Self {
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