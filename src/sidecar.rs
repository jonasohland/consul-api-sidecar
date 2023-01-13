use consul_api_sidecar::client;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    _main().await
}

async fn _main() {

    tracing_subscriber::fmt().with_max_level(tracing::Level::TRACE).init();

    let config = client::Config {
        socket_path: "/tmp/consul-api-sidecar".to_owned(),
        name: "test-client".to_owned()
    };

    let client = client::Client::connect(config).await.unwrap();
    tokio::signal::ctrl_c().await.unwrap();

    client.shutdown().await;


}
