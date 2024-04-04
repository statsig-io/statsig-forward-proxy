use statsig_forward_proxy::statsig_forward_proxy_client::StatsigForwardProxyClient;
use statsig_forward_proxy::ConfigSpecRequest;

pub mod statsig_forward_proxy {
    tonic::include_proto!("statsig_forward_proxy");
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut client = StatsigForwardProxyClient::connect("http://[::1]:50051").await?;

    let request = tonic::Request::new(ConfigSpecRequest {
        since_time: Some(1234),
        sdk_key: "1234".into(),
    });

    let response = client.get_config_spec(request).await?;

    println!("RESPONSE={:?}", response);

    Ok(())
}
