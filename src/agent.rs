use consul_api_sidecar::server;



#[tokio::main(flavor = "current_thread")]
pub async fn main() {
    _main().await
}


async fn _main() {

    tracing_subscriber::fmt().with_max_level(tracing::Level::TRACE).init();

    let config = server::Config {
        name: "test-server".to_owned(),
        socket_path: "/tmp/consul-api-sidecar".to_owned()
    };

    let server = server::Server::bind(config).await.unwrap();
    tokio::signal::ctrl_c().await.unwrap();
    
    server.shutdown().await.unwrap();
}
