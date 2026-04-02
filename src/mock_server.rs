use rmcp::{
    ServerHandler,
    service::{Peer, RoleServer, serve_directly},
};

pub struct MockMcpServer;

impl MockMcpServer {
    pub fn new() -> Self {
        Self
    }
}

pub async fn dummy_peer<S: ServerHandler + Clone>(handler: S) -> Peer<RoleServer> {
    let transport = (tokio::io::empty(), tokio::io::sink());
    let service = serve_directly(handler, transport, None);
    service.peer().clone()
}
