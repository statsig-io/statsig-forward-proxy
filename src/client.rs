use chrono::Local;
use statsig_forward_proxy::statsig_forward_proxy_client::StatsigForwardProxyClient;
use statsig_forward_proxy::ConfigSpecRequest;

pub mod statsig_forward_proxy {
    tonic::include_proto!("statsig_forward_proxy");
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut client = StatsigForwardProxyClient::connect("http://0.0.0.0:50051")
        .await?
        // 16mb -- default is 4mb
        .max_decoding_message_size(16777216);

    // Non-Streaming
    let request = tonic::Request::new(ConfigSpecRequest {
        since_time: Some(1234),
        sdk_key: "1234".into(),
    });
    let response: tonic::Response<statsig_forward_proxy::ConfigSpecResponse> =
        client.get_config_spec(request).await?;
    println!(
        "RESPONSE={:?}, CURRENT TIME={}",
        response.into_inner().last_updated,
        Local::now()
    );
    // Streaming
    let request = tonic::Request::new(ConfigSpecRequest {
        since_time: Some(1234),
        sdk_key: "1234".into(),
    });
    let mut stream = client.stream_config_spec(request).await?.into_inner();
    while let Some(value) = stream.message().await? {
        println!(
            "STREAMING={:?}, CURRENT TIME={}",
            value.last_updated,
            Local::now()
        );
    }

    Ok(())
}
