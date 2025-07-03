use std::fs;

use chrono::Local;
use statsig_forward_proxy::statsig_forward_proxy_client::StatsigForwardProxyClient;
use statsig_forward_proxy::ConfigSpecRequest;

use tonic::transport::{Certificate, Channel, ClientTlsConfig};

pub mod statsig_forward_proxy {
    tonic::include_proto!("statsig_forward_proxy");
}

fn last_500_char(spec: &str) -> String {
    // only print last 500 characters
    let len = spec.len();
    let start = len.saturating_sub(500);
    spec[start..].to_string()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let pem = std::fs::read_to_string("./x509_test_certs/root/certs/root_ca.crt")?;
    let cert = fs::read_to_string("./x509_test_certs/intermediate/certs/client.crt")?;
    let key = fs::read_to_string("./x509_test_certs/intermediate/private/client.key")?;
    let client_tls_id = tonic::transport::Identity::from_pem(cert.as_bytes(), key.as_bytes());
    let ca = Certificate::from_pem(pem);
    let tls = ClientTlsConfig::new()
        .ca_certificate(ca)
        .identity(client_tls_id);
    let channel = Channel::from_static("http://0.0.0.0:50051")
        .tls_config(tls)?
        .connect()
        .await?;
    let sdk_key = std::env::var("STATSIG_SERVER_SDK_KEY").unwrap().to_string();

    let mut client = StatsigForwardProxyClient::new(channel).max_decoding_message_size(100870203);

    // Non-Streaming
    for version in 0..3 {
        let request = tonic::Request::new(ConfigSpecRequest {
            since_time: Some(1234),
            sdk_key: sdk_key.clone(),
            version: Some(version),
            zstd_dict_id: Some("test".to_string()),
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
        sdk_key: sdk_key.clone(),
        version: Some(2),
        zstd_dict_id: Some("test".to_string()),
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
