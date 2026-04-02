use rmcp::{
    ServerHandler,
    service::{Peer, RoleServer, ServiceExt},
};

pub struct MockMcpServer;

impl MockMcpServer {
    pub fn new() -> Self {
        Self
    }
}

pub async fn dummy_peer<S: ServerHandler + Clone>(handler: S) -> Peer<RoleServer> {
    let (_client_stream, server_stream) = tokio::io::duplex(1024);
    let service = handler
        .serve(server_stream)
        .await
        .expect("Failed to serve mock server");

    // We don't need to do anything with client_stream for now,
    // just having it connected is enough to get a Peer from the server side.

    service.peer().clone()
}
