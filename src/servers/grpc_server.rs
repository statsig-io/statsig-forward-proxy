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
use statsig_forward_proxy::statsig_forward_proxy_server::{
    StatsigForwardProxy, StatsigForwardProxyServer,
};
use statsig_forward_proxy::{ConfigSpecRequest, ConfigSpecResponse};
use std::env;
use std::str::FromStr;
use tonic::metadata::MetadataValue;

use std::collections::HashMap;
use std::sync::Arc;

use super::http_server::AuthorizedRequestContext;

pub mod statsig_forward_proxy {
    tonic::include_proto!("statsig_forward_proxy");
}

pub struct StatsigForwardProxyServerImpl {
    config_spec_store: Arc<datastore::config_spec_store::ConfigSpecStore>,
    dcs_observer: Arc<HttpDataProviderObserver>,
    update_broadcast_cache: Arc<RwLock<HashMap<AuthorizedRequestContext, Arc<StreamingChannel>>>>,
}

impl StatsigForwardProxyServerImpl {
    fn new(
        config_spec_store: Arc<ConfigSpecStore>,
        dcs_observer: Arc<HttpDataProviderObserver>,
        update_broadcast_cache: Arc<
            RwLock<HashMap<AuthorizedRequestContext, Arc<StreamingChannel>>>,
        >,
    ) -> Self {
        StatsigForwardProxyServerImpl {
            config_spec_store,
            dcs_observer,
            update_broadcast_cache,
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
                &AuthorizedRequestContext::new(
                    request.get_ref().sdk_key.clone(),
                    format!(
                        "/v{}/download_config_specs/",
                        StatsigForwardProxyServerImpl::get_api_version_from_request(&request)
                    ),
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
            spec: data.read().await.config.to_string(),
            last_updated: data.read().await.lcut,
        };
        Ok(Response::new(reply))
    }

    type StreamConfigSpecStream = ReceiverStream<Result<ConfigSpecResponse, Status>>;
    async fn stream_config_spec(
        &self,
        request: Request<ConfigSpecRequest>,
    ) -> Result<tonic::Response<Self::StreamConfigSpecStream>, tonic::Status> {
        let sdk_key = request.get_ref().sdk_key.to_string();
        let api_version = StatsigForwardProxyServerImpl::get_api_version_from_request(&request);
        let request_context = AuthorizedRequestContext::new(
            sdk_key.clone(),
            format!("/v{}/download_config_specs/", api_version),
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
                let sc = Arc::new(StreamingChannel::new(&request_context));
                let streaming_channel_trait: Arc<dyn HttpDataProviderObserverTrait + Send + Sync> =
                    sc.clone();
                self.dcs_observer
                    .add_observer(streaming_channel_trait)
                    .await;
                let rv = sc.sender.read().await.subscribe();
                wlock.insert(request_context.clone(), sc);
                rv
            }
        };
        drop(wlock);

        // After initial response, then start listening for updates
        let ubc_ref = Arc::clone(&self.update_broadcast_cache);
        tokio::spawn(async move {
            tx.send(Ok(init_value)).await.ok();
            ProxyEventObserver::publish_event(
                ProxyEvent::new(
                    ProxyEventType::GrpcStreamingStreamedInitialized,
                    &request_context,
                )
                .with_stat(EventStat {
                    operation_type: OperationType::IncrByValue,
                    value: 1,
                }),
            )
            .await;

            loop {
                match rc.recv().await {
                    Ok(maybe_csr) => match maybe_csr {
                        Some(csr) => match tx.send(Ok(csr)).await {
                            Ok(_) => {
                                ProxyEventObserver::publish_event(
                                    ProxyEvent::new(
                                        ProxyEventType::GrpcStreamingStreamedResponse,
                                        &request_context,
                                    )
                                    .with_stat(EventStat {
                                        operation_type: OperationType::IncrByValue,
                                        value: 1,
                                    }),
                                )
                                .await;
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
                ProxyEvent::new(
                    ProxyEventType::GrpcStreamingStreamDisconnected,
                    &request_context,
                )
                .with_stat(EventStat {
                    operation_type: OperationType::IncrByValue,
                    value: 1,
                }),
            )
            .await;
        });

        let mut response = Response::new(ReceiverStream::new(rx));
        response.metadata_mut().insert(
            "x-sfp-hostname",
            MetadataValue::from_str(&env::var("HOSTNAME").unwrap_or("no_hostname_set".to_string()))
                .unwrap(),
        );
        Ok(response)
    }
}

pub struct GrpcServer {}

impl GrpcServer {
    async fn shutdown_signal(
        update_broadcast_cache: Arc<
            RwLock<HashMap<AuthorizedRequestContext, Arc<StreamingChannel>>>,
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
        max_concurrent_streams: u32,
        config_spec_store: Arc<ConfigSpecStore>,
        shared_dcs_observer: Arc<HttpDataProviderObserver>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let addr = "0.0.0.0:50051".parse().unwrap();
        let update_broadcast_cache = Arc::new(RwLock::new(HashMap::new()));
        let sfp_server = StatsigForwardProxyServerImpl::new(
            config_spec_store,
            shared_dcs_observer,
            update_broadcast_cache.clone(),
        );

        println!("GrpcServer listening on {}", addr);
        Server::builder()
            .max_concurrent_streams(max_concurrent_streams)
            .add_service(StatsigForwardProxyServer::new(sfp_server))
            .serve_with_shutdown(addr, GrpcServer::shutdown_signal(update_broadcast_cache))
            .await?;

        Ok(())
    }
}
