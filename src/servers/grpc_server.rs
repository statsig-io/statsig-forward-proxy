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
use crate::utils::compress_encoder::CompressionEncoder;
use crate::Cli;
use grpc_health::health_server::HealthServer;
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
use super::normalized_path::NormalizedPath;
use crate::servers::authorized_request_context::AuthorizedRequestContext;
use crate::GRACEFUL_SHUTDOWN_TOKEN;
use std::sync::atomic::{AtomicUsize, Ordering};

const NO_UPDATE_JSON: &str = "{\"has_updates\":false}";

pub mod statsig_forward_proxy {
    tonic::include_proto!("statsig_forward_proxy");
}
pub mod grpc_health {
    tonic::include_proto!("grpc.health.v1");
}

static CONNECTION_COUNTER: AtomicUsize = AtomicUsize::new(0);
#[derive(Clone)]
pub struct StatsigForwardProxyServerImpl {
    config_spec_store: Arc<datastore::config_spec_store::ConfigSpecStore>,
    dcs_observer: Arc<HttpDataProviderObserver>,
    update_broadcast_cache:
        Arc<RwLock<HashMap<Arc<AuthorizedRequestContext>, Arc<StreamingChannel>>>>,
    rc_cache: Arc<AuthorizedRequestContextCache>,
    max_concurrent_streams: usize,
}

impl StatsigForwardProxyServerImpl {
    fn new(
        config_spec_store: Arc<ConfigSpecStore>,
        dcs_observer: Arc<HttpDataProviderObserver>,
        update_broadcast_cache: Arc<
            RwLock<HashMap<Arc<AuthorizedRequestContext>, Arc<StreamingChannel>>>,
        >,
        rc_cache: Arc<AuthorizedRequestContextCache>,
        max_concurrent_streams: usize,
    ) -> Self {
        StatsigForwardProxyServerImpl {
            config_spec_store,
            dcs_observer,
            update_broadcast_cache,
            rc_cache,
            max_concurrent_streams,
        }
    }

    fn get_normalized_path_from_request(request: &Request<ConfigSpecRequest>) -> NormalizedPath {
        fn get_api_version_from_request(request: &Request<ConfigSpecRequest>) -> i32 {
            let mut version_number = request.get_ref().version.unwrap_or(1);
            // Set default version for backwards compatability to v1 if not defined
            if version_number == 0 {
                version_number = 1;
            }

            version_number
        }
        match get_api_version_from_request(request) {
            1 => NormalizedPath::V1DownloadConfigSpecs,
            2 => NormalizedPath::V2DownloadConfigSpecs,
            _ => NormalizedPath::RawPath(format!(
                "/v{}/download_config_specs/",
                get_api_version_from_request(request)
            )),
        }
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
        let authorized_request_context = self.rc_cache.get_or_insert(
            request.get_ref().sdk_key.to_string(),
            StatsigForwardProxyServerImpl::get_normalized_path_from_request(&request),
            vec![CompressionEncoder::PlainText],
        );
        let result = self
            .config_spec_store
            .get_config_spec(
                &authorized_request_context,
                request.get_ref().since_time.unwrap_or(0),
            )
            .await;
        let data = match result {
            Some(data) => data,
            None => {
                return Err(Status::permission_denied("Unauthorized"));
            }
        };
        let reply = ConfigSpecResponse {
            spec: if Arc::ptr_eq(&data.config, &self.config_spec_store.no_update_config) {
                NO_UPDATE_JSON.to_string()
            } else {
                unsafe { String::from_utf8_unchecked(data.config.data.to_vec()) }
            },
            last_updated: data.lcut,
            zstd_dict_id: None,
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
        if CONNECTION_COUNTER.load(Ordering::SeqCst) >= self.max_concurrent_streams {
            return Err(Status::resource_exhausted("Max concurrent streams reached"));
        }

        let sdk_key = request.get_ref().sdk_key.to_string();
        let normalized_path =
            StatsigForwardProxyServerImpl::get_normalized_path_from_request(&request);
        let request_context = self.rc_cache.get_or_insert(
            sdk_key,
            normalized_path,
            vec![CompressionEncoder::PlainText],
        );

        let (tx, rx) = mpsc::channel(1);
        // Get initial DCS on first request
        // If this fails, we just return error instead
        // of initializing stream which also deals with
        // unauthorized requests
        let init_value = match self.get_config_spec(request).await {
            Ok(data) => data.into_inner(),
            Err(e) => {
                ProxyEventObserver::publish_event(
                    ProxyEvent::new_with_rc(
                        ProxyEventType::GrpcStreamingStreamUnauthorized,
                        &request_context,
                    )
                    .with_stat(EventStat {
                        operation_type: OperationType::IncrByValue,
                        value: 1,
                    }),
                );
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
        let mut latest_lcut = init_value.last_updated;
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
            CONNECTION_COUNTER.fetch_add(1, Ordering::SeqCst);

            loop {
                match tokio::time::timeout(tokio::time::Duration::from_secs(30), rc.recv()).await {
                    Ok(Ok(maybe_csr)) => {
                        match maybe_csr {
                            Some((data, lcut)) => {
                                latest_lcut = lcut;
                                match tx
                                    .send(Ok(ConfigSpecResponse {
                                        spec: unsafe {
                                            String::from_utf8_unchecked(data.data.to_vec())
                                        },
                                        last_updated: lcut,
                                        zstd_dict_id: None,
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
                                }
                            }
                            None => {
                                break;
                            }
                        }
                    }
                    Ok(Err(e)) => {
                        eprintln!("Error on broadcast receiver: {e:?}");
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
                    Err(_) => {
                        match tx
                            .send(Ok(ConfigSpecResponse {
                                spec: NO_UPDATE_JSON.to_string(),
                                last_updated: latest_lcut,
                                zstd_dict_id: None,
                            }))
                            .await
                        {
                            Ok(_) => {
                                ProxyEventObserver::publish_event(
                                    ProxyEvent::new_with_rc(
                                        ProxyEventType::GrpcStreamingHealthcheckSent,
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
                        }
                    }
                }
            }

            CONNECTION_COUNTER.fetch_sub(1, Ordering::SeqCst);
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
pub struct HealthService {
    max_concurrent_streams: usize,
    grpc_ready_endpoint_percentage_of_total_streams_threshold: usize,
}

impl HealthService {
    fn new(
        max_concurrent_streams: u32,
        grpc_ready_endpoint_percentage_of_total_streams_threshold: usize,
    ) -> Self {
        HealthService {
            max_concurrent_streams: max_concurrent_streams as usize,
            grpc_ready_endpoint_percentage_of_total_streams_threshold,
        }
    }
}

#[tonic::async_trait]
impl grpc_health::health_server::Health for HealthService {
    async fn check(
        &self,
        _request: Request<grpc_health::HealthCheckRequest>,
    ) -> Result<Response<grpc_health::HealthCheckResponse>, Status> {
        let active_connections = CONNECTION_COUNTER.load(Ordering::SeqCst);
        Ok(Response::new(grpc_health::HealthCheckResponse {
            status: match ((active_connections * 100) / self.max_concurrent_streams)
                <= self.grpc_ready_endpoint_percentage_of_total_streams_threshold
            {
                true => grpc_health::health_check_response::ServingStatus::Serving.into(),
                false => grpc_health::health_check_response::ServingStatus::NotServing.into(),
            },
        }))
    }

    type WatchStream =
        tokio_stream::wrappers::ReceiverStream<Result<grpc_health::HealthCheckResponse, Status>>;
    async fn watch(
        &self,
        _request: Request<grpc_health::HealthCheckRequest>,
    ) -> Result<Response<Self::WatchStream>, Status> {
        Err(Status::unimplemented("not supported"))
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
            cli.grpc_max_concurrent_streams as usize,
        );

        // Spawn a background task to log number of active grpc streams
        tokio::spawn(async {
            loop {
                if tokio::select! {
                    _ = tokio::time::sleep(tokio::time::Duration::from_secs(60)) => { false },
                    _ = GRACEFUL_SHUTDOWN_TOKEN.cancelled() => {
                        true
                    },
                } {
                    break;
                }

                ProxyEventObserver::publish_event(
                    ProxyEvent::new(ProxyEventType::GrpcEstimatedActiveStreams).with_stat(
                        EventStat {
                            operation_type: OperationType::Gauge,
                            value: CONNECTION_COUNTER.load(Ordering::SeqCst) as i64,
                        },
                    ),
                );
            }
        });

        match (
            cli.x509_server_cert_path.is_some() && cli.x509_server_key_path.is_some(),
            cli.enforce_tls,
        ) {
            (false, _) => {
                println!("GrpcServer listening on {http_addr} (HTTP)");
                Self::create_http_server(cli, sfp_server, http_addr, update_broadcast_cache.clone())
                    .await?
            }
            (true, true) => {
                println!("GrpcServer listening on {http_addr} (HTTPS)");
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
                let http_server = Self::create_http_server(
                    cli,
                    sfp_server,
                    http_addr,
                    update_broadcast_cache.clone(),
                );
                println!(
                    "[SFP] GrpcServer listening on {https_addr} (HTTPS) and {http_addr} (HTTP)"
                );

                // Use select! to run both servers concurrently
                tokio::select! {
                    https_result = https_server => {
                        if let Err(e) = https_result {
                            eprintln!("HTTPS server error: {e:?}");
                        }
                    }
                    http_result = http_server => {
                        if let Err(e) = http_result {
                            eprintln!("HTTP server error: {e:?}");
                        }
                    }
                }
            }
        }

        Ok(())
    }

    async fn create_https_server(
        cli: &Cli,
        sfp_server: StatsigForwardProxyServerImpl,
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
            .add_service(HealthServer::new(HealthService::new(
                cli.grpc_max_concurrent_streams,
                cli.grpc_ready_endpoint_percentage_of_total_streams_threshold,
            )))
            .add_service(
                StatsigForwardProxyServer::new(sfp_server)
                    .max_decoding_message_size(20 * 1024 * 1024),
            )
            .serve_with_shutdown(addr, GrpcServer::shutdown_signal(update_broadcast_cache))
            .await?;
        Ok(())
    }

    async fn create_http_server(
        cli: &Cli,
        sfp_server: StatsigForwardProxyServerImpl,
        addr: std::net::SocketAddr,
        update_broadcast_cache: Arc<
            RwLock<HashMap<Arc<AuthorizedRequestContext>, Arc<StreamingChannel>>>,
        >,
    ) -> Result<(), Box<dyn std::error::Error>> {
        Server::builder()
            .max_concurrent_streams(cli.grpc_max_concurrent_streams)
            .add_service(HealthServer::new(HealthService::new(
                cli.grpc_max_concurrent_streams,
                cli.grpc_ready_endpoint_percentage_of_total_streams_threshold,
            )))
            .add_service(StatsigForwardProxyServer::new(sfp_server))
            .serve_with_shutdown(addr, GrpcServer::shutdown_signal(update_broadcast_cache))
            .await?;
        Ok(())
    }
}
