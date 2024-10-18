use std::fs;
use tokio::sync::{mpsc, RwLock};

use tokio_stream::wrappers::ReceiverStream;

use tonic::{transport::Server, Request, Response, Status};

use crate::datastore::config_spec_store::ConfigSpecStore;

use crate::datastore::{self};
use crate::observers::http_data_provider_observer::HttpDataProviderObserver;
use crate::observers::proxy_event_observer::ProxyEventObserver;
use crate::observers::OperationType;
use crate::observers::{EventStat, HttpDataProviderObserverTrait};
use crate::observers::{ProxyEvent, ProxyEventType};
use crate::servers::streaming_channel::StreamingChannel;
use crate::Cli;
use statsig_forward_proxy::statsig_forward_proxy_server::{
    StatsigForwardProxy, StatsigForwardProxyServer,
};
use statsig_forward_proxy::{ConfigSpecRequest, ConfigSpecResponse};
use std::env;
use std::str::FromStr;
use tonic::metadata::MetadataValue;

use std::collections::HashMap;
use std::sync::Arc;

use super::authorized_request_context::AuthorizedRequestContextCache;
use crate::servers::authorized_request_context::AuthorizedRequestContext;

pub mod statsig_forward_proxy {
    tonic::include_proto!("statsig_forward_proxy");
}

#[derive(Clone)]
pub struct StatsigForwardProxyServerImpl {
    config_spec_store: Arc<datastore::config_spec_store::ConfigSpecStore>,
    dcs_observer: Arc<HttpDataProviderObserver>,
    update_broadcast_cache:
        Arc<RwLock<HashMap<Arc<AuthorizedRequestContext>, Arc<StreamingChannel>>>>,
    rc_cache: Arc<AuthorizedRequestContextCache>,
}

impl StatsigForwardProxyServerImpl {
    fn new(
        config_spec_store: Arc<ConfigSpecStore>,
        dcs_observer: Arc<HttpDataProviderObserver>,
        update_broadcast_cache: Arc<
            RwLock<HashMap<Arc<AuthorizedRequestContext>, Arc<StreamingChannel>>>,
        >,
        rc_cache: Arc<AuthorizedRequestContextCache>,
    ) -> Self {
        StatsigForwardProxyServerImpl {
            config_spec_store,
            dcs_observer,
            update_broadcast_cache,
            rc_cache,
        }
    }

    fn get_api_version_from_request(request: &Request<ConfigSpecRequest>) -> i32 {
        let mut version_number = request.get_ref().version.unwrap_or(1);
        // Set default version for backwards compatability to v1 if not defined
        if version_number == 0 {
            version_number = 1;
        }

        version_number
    }

    fn add_common_metadata<T>(&self, response: &mut Response<T>) {
        response.metadata_mut().insert(
            "x-sfp-hostname",
            MetadataValue::from_str(&env::var("HOSTNAME").unwrap_or("no_hostname_set".to_string()))
                .unwrap(),
        );
    }
}

#[tonic::async_trait]
impl StatsigForwardProxy for StatsigForwardProxyServerImpl {
    async fn get_config_spec(
        &self,
        request: Request<ConfigSpecRequest>,
    ) -> Result<Response<ConfigSpecResponse>, Status> {
        let data = match self
            .config_spec_store
            .get_config_spec(
                &self.rc_cache.get_or_insert(
                    request.get_ref().sdk_key.to_string(),
                    format!(
                        "/v{}/download_config_specs/",
                        StatsigForwardProxyServerImpl::get_api_version_from_request(&request)
                    ),
                    false, /* use_gzip */
                ),
                request.get_ref().since_time.unwrap_or(0),
            )
            .await
        {
            Some(data) => data,
            None => {
                return Err(Status::permission_denied("Unauthorized"));
            }
        };
        let reply = ConfigSpecResponse {
            spec: unsafe { String::from_utf8_unchecked(data.config.data.to_vec()) },
            last_updated: data.lcut,
        };

        let mut response = Response::new(reply);
        self.add_common_metadata(&mut response);
        Ok(response)
    }

    type StreamConfigSpecStream = ReceiverStream<Result<ConfigSpecResponse, Status>>;
    async fn stream_config_spec(
        &self,
        request: Request<ConfigSpecRequest>,
    ) -> Result<tonic::Response<Self::StreamConfigSpecStream>, tonic::Status> {
        let sdk_key = request.get_ref().sdk_key.to_string();
        let api_version = StatsigForwardProxyServerImpl::get_api_version_from_request(&request);
        let request_context = self.rc_cache.get_or_insert(
            sdk_key,
            format!("/v{}/download_config_specs/", api_version,),
            false, /* use_gzip */
        );

        let (tx, rx) = mpsc::channel(1);
        // Get initial DCS on first request
        // If this fails, we just return error instead
        // of initializing stream which also deals with
        // unauthorized requests
        let init_value = match self.get_config_spec(request).await {
            Ok(data) => data.into_inner(),
            Err(e) => {
                return Err(e);
            }
        };

        // Re-use broadcast channel if its already been created for
        // a given sdk key
        let mut wlock = self.update_broadcast_cache.write().await;
        let mut rc = match wlock.contains_key(&request_context) {
            true => wlock
                .get(&request_context)
                .expect("We did a key check")
                .sender
                .read()
                .await
                .subscribe(),
            false => {
                let sc = Arc::new(StreamingChannel::new(Arc::clone(&request_context)));
                let streaming_channel_trait: Arc<dyn HttpDataProviderObserverTrait + Send + Sync> =
                    sc.clone();
                self.dcs_observer
                    .add_observer(streaming_channel_trait)
                    .await;
                let rv = sc.sender.read().await.subscribe();
                wlock.insert(Arc::clone(&request_context), sc);
                rv
            }
        };
        drop(wlock);

        // After initial response, then start listening for updates
        let ubc_ref = Arc::clone(&self.update_broadcast_cache);
        rocket::tokio::spawn(async move {
            tx.send(Ok(init_value)).await.ok();
            ProxyEventObserver::publish_event(
                ProxyEvent::new_with_rc(
                    ProxyEventType::GrpcStreamingStreamedInitialized,
                    &request_context,
                )
                .with_stat(EventStat {
                    operation_type: OperationType::IncrByValue,
                    value: 1,
                }),
            );

            loop {
                match rc.recv().await {
                    Ok(maybe_csr) => match maybe_csr {
                        Some((data, lcut)) => match tx
                            .send(Ok(ConfigSpecResponse {
                                spec: unsafe { String::from_utf8_unchecked(data.data.to_vec()) },
                                last_updated: lcut,
                            }))
                            .await
                        {
                            Ok(_) => {
                                ProxyEventObserver::publish_event(
                                    ProxyEvent::new_with_rc(
                                        ProxyEventType::GrpcStreamingStreamedResponse,
                                        &request_context,
                                    )
                                    .with_stat(EventStat {
                                        operation_type: OperationType::IncrByValue,
                                        value: 1,
                                    }),
                                );
                            }
                            Err(_e) => {
                                break;
                            }
                        },
                        None => {
                            break;
                        }
                    },
                    Err(e) => {
                        eprintln!("Error on broadcast receiver: {:?}", e);
                        rc = ubc_ref
                            .read()
                            .await
                            .get(&request_context)
                            .expect("Already created")
                            .sender
                            .read()
                            .await
                            .subscribe();
                    }
                }
            }

            ProxyEventObserver::publish_event(
                ProxyEvent::new_with_rc(
                    ProxyEventType::GrpcStreamingStreamDisconnected,
                    &request_context,
                )
                .with_stat(EventStat {
                    operation_type: OperationType::IncrByValue,
                    value: 1,
                }),
            );
        });

        let mut response = Response::new(ReceiverStream::new(rx));
        self.add_common_metadata(&mut response);
        Ok(response)
    }
}

pub struct GrpcServer {}

impl GrpcServer {
    async fn shutdown_signal(
        update_broadcast_cache: Arc<
            RwLock<HashMap<Arc<AuthorizedRequestContext>, Arc<StreamingChannel>>>,
        >,
    ) {
        let mut int_stream =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                .expect("Failed to install SIGINT handler");
        let mut term_stream =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("Failed to install SIGTERM handler");
        tokio::select! {
            _ = int_stream.recv() => {
                println!("Received SIGINT, terminating...");
            }
            _ = term_stream.recv() => {
                println!("Received SIGTERM, terminating...");
            }
        }

        let wlock = update_broadcast_cache.write().await;

        for (_, sc) in wlock.iter() {
            sc.sender.write().await.send(None).ok();
        }
        println!("All grpc streams terminated, shutting down...");
    }

    pub async fn start_server(
        cli: &Cli,
        config_spec_store: Arc<ConfigSpecStore>,
        shared_dcs_observer: Arc<HttpDataProviderObserver>,
        rc_cache: Arc<AuthorizedRequestContextCache>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let http_addr = "0.0.0.0:50051".parse().unwrap();
        let https_addr = "0.0.0.0:50052".parse().unwrap();
        let update_broadcast_cache = Arc::new(RwLock::new(HashMap::new()));
        let sfp_server = StatsigForwardProxyServerImpl::new(
            config_spec_store,
            shared_dcs_observer,
            update_broadcast_cache.clone(),
            rc_cache,
        );

        match (
            cli.x509_server_cert_path.is_some() && cli.x509_server_key_path.is_some(),
            cli.enforce_tls,
        ) {
            (false, _) => {
                println!("GrpcServer listening on {} (HTTP)", http_addr);
                Self::create_http_server(sfp_server, http_addr, update_broadcast_cache.clone())
                    .await?
            }
            (true, true) => {
                println!("GrpcServer listening on {} (HTTPS)", http_addr);
                Self::create_https_server(
                    cli,
                    sfp_server.clone(),
                    https_addr,
                    update_broadcast_cache.clone(),
                )
                .await?
            }
            (true, false) => {
                let https_server = Self::create_https_server(
                    cli,
                    sfp_server.clone(),
                    https_addr,
                    update_broadcast_cache.clone(),
                );
                let http_server =
                    Self::create_http_server(sfp_server, http_addr, update_broadcast_cache.clone());
                println!(
                    "GrpcServer listening on {} (HTTPS) and {} (HTTP)",
                    https_addr, http_addr
                );

                // Use select! to run both servers concurrently
                tokio::select! {
                    https_result = https_server => {
                        if let Err(e) = https_result {
                            eprintln!("HTTPS server error: {:?}", e);
                        }
                    }
                    http_result = http_server => {
                        if let Err(e) = http_result {
                            eprintln!("HTTP server error: {:?}", e);
                        }
                    }
                }
            }
        }

        Ok(())
    }

    async fn create_https_server(
        cli: &Cli,
        sfp_server: StatsigForwardProxyServerImpl, // Remove & here
        addr: std::net::SocketAddr,
        update_broadcast_cache: Arc<
            RwLock<HashMap<Arc<AuthorizedRequestContext>, Arc<StreamingChannel>>>,
        >,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut builder = Server::builder();
        if let (Some(cert_path), Some(key_path)) = (
            cli.x509_server_cert_path.clone(),
            cli.x509_server_key_path.clone(),
        ) {
            let cert = fs::read_to_string(cert_path)?;
            let key = fs::read_to_string(key_path)?;
            let server_tls_id =
                tonic::transport::Identity::from_pem(cert.as_bytes(), key.as_bytes());
            let mut tls = tonic::transport::ServerTlsConfig::new().identity(server_tls_id);
            tls = tls.client_auth_optional(!cli.enforce_mtls);

            if let Some(client_cert_path) = cli.x509_client_cert_path.clone() {
                let encoded_client_cert = fs::read_to_string(client_cert_path)?;
                let client_cert =
                    tonic::transport::Certificate::from_pem(encoded_client_cert.as_bytes());
                tls = tls.client_ca_root(client_cert);
            }

            builder = builder.tls_config(tls)?;
        }
        builder
            .max_concurrent_streams(cli.grpc_max_concurrent_streams)
            .add_service(StatsigForwardProxyServer::new(sfp_server)) // Remove .clone() here
            .serve_with_shutdown(addr, GrpcServer::shutdown_signal(update_broadcast_cache))
            .await?;
        Ok(())
    }

    async fn create_http_server(
        sfp_server: StatsigForwardProxyServerImpl, // Remove & here
        addr: std::net::SocketAddr,
        update_broadcast_cache: Arc<
            RwLock<HashMap<Arc<AuthorizedRequestContext>, Arc<StreamingChannel>>>,
        >,
    ) -> Result<(), Box<dyn std::error::Error>> {
        Server::builder()
            .add_service(StatsigForwardProxyServer::new(sfp_server)) // Remove .clone() here
            .serve_with_shutdown(addr, GrpcServer::shutdown_signal(update_broadcast_cache))
            .await?;
        Ok(())
    }
}
