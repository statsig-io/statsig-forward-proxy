use chrono::Local;
use statsig_forward_proxy::statsig_forward_proxy_client::StatsigForwardProxyClient;
use statsig_forward_proxy::ConfigSpecRequest;

pub mod statsig_forward_proxy {
    tonic::include_proto!("statsig_forward_proxy");
}

fn last_500_char(spec: &String) -> String {
    // only print last 500 characters
    let len = spec.len();
    let start = if len > 500 { len - 500 } else { 0 };
    spec[start..].to_string()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut client = StatsigForwardProxyClient::connect("http://0.0.0.0:50051")
        .await?
        // 16mb -- default is 4mb
        .max_decoding_message_size(16777216);

    // Non-Streaming
    for version in 0..3 {
        let request = tonic::Request::new(ConfigSpecRequest {
            since_time: Some(1234),
            sdk_key: "1234".into(),
            version: Some(version),
        });
        let response: tonic::Response<statsig_forward_proxy::ConfigSpecResponse> =
            client.get_config_spec(request).await?;
        let config_response = response.into_inner();
        println!(
            "Version={}, RESPONSE={:?}, CURRENT TIME={}, SPEC={}\n",
            version,
            &config_response.last_updated,
            Local::now(),
            last_500_char(&config_response.spec)
        );
    }

    // Streaming
    let request = tonic::Request::new(ConfigSpecRequest {
        since_time: Some(1234),
        sdk_key: "1234".into(),
        version: Some(2),
    });
    let response = client.stream_config_spec(request).await?;
    println!("Metadata={:?}", response.metadata());
    let mut stream = response.into_inner();

    loop {
        match stream.message().await {
            Ok(Some(value)) => {
                println!(
                    "STREAMING={:?}, CURRENT TIME={}, SPEC={}",
                    value.last_updated,
                    Local::now(),
                    last_500_char(&value.spec)
                );
            }
            Ok(None) => {
                println!("STREAMING DONE");
                break;
            }
            Err(e) => {
                println!("CURRENT TIME={}", Local::now());
                println!("Error={:?}", e);
                break;
            }
        }
    }
    Ok(())
}
