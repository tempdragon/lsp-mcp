use rmcp::service::Peer;
use rmcp::service::RoleServer;
use rmcp::service::serve_directly;

pub fn dummy_peer<S: rmcp::service::Service<RoleServer> + Clone>(handler: S) -> Peer<RoleServer> {
    let transport = (tokio::io::empty(), tokio::io::sink());
    let server = serve_directly(handler, transport, None);
    server.peer().clone()
}
