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

use std::collections::HashMap;
use std::sync::Arc;

pub mod statsig_forward_proxy {
    tonic::include_proto!("statsig_forward_proxy");
}

pub struct StatsigForwardProxyServerImpl {
    config_spec_store: Arc<datastore::config_spec_store::ConfigSpecStore>,
    dcs_observer: Arc<HttpDataProviderObserver>,
    update_broadcast_cache: Arc<RwLock<HashMap<String, Arc<StreamingChannel>>>>,
}

impl StatsigForwardProxyServerImpl {
    fn new(
        config_spec_store: Arc<ConfigSpecStore>,
        dcs_observer: Arc<HttpDataProviderObserver>,
    ) -> Self {
        StatsigForwardProxyServerImpl {
            config_spec_store,
            dcs_observer,
            update_broadcast_cache: Arc::new(RwLock::new(HashMap::new())),
        }
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
                &request.get_ref().sdk_key,
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
        let (tx, rx) = mpsc::channel(1);
        let sdk_key = request.get_ref().sdk_key.to_string();

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
        let contains_key = self
            .update_broadcast_cache
            .read()
            .await
            .contains_key(&sdk_key);
        let mut rc = match contains_key {
            true => self
                .update_broadcast_cache
                .read()
                .await
                .get(&sdk_key)
                .expect("We did a key check")
                .sender
                .read()
                .await
                .subscribe(),
            false => {
                let sc = Arc::new(StreamingChannel::new(&sdk_key));
                let streaming_channel_trait: Arc<dyn HttpDataProviderObserverTrait + Send + Sync> =
                    sc.clone();
                self.dcs_observer
                    .add_observer(streaming_channel_trait)
                    .await;
                let rv = sc.sender.read().await.subscribe();
                self.update_broadcast_cache
                    .write()
                    .await
                    .insert(sdk_key.to_string(), sc);
                rv
            }
        };

        // After initial response, then start listening for updates
        let ubc_ref = Arc::clone(&self.update_broadcast_cache);
        tokio::spawn(async move {
            match tx.send(Ok(init_value)).await {
                Ok(_) => {
                    ProxyEventObserver::publish_event(
                        ProxyEvent::new(
                            ProxyEventType::GrpcStreamingStreamedInitialized,
                            sdk_key.to_string(),
                        )
                        .with_stat(EventStat {
                            operation_type: OperationType::IncrByValue,
                            value: 1,
                        }),
                    )
                    .await;
                }
                Err(_e) => {
                    ProxyEventObserver::publish_event(
                        ProxyEvent::new(
                            ProxyEventType::GrpcStreamingStreamDisconnected,
                            sdk_key.to_string(),
                        )
                        .with_stat(EventStat {
                            operation_type: OperationType::IncrByValue,
                            value: 1,
                        }),
                    )
                    .await;
                }
            }

            loop {
                match rc.recv().await {
                    Ok(csr) => match tx.send(Ok(csr)).await {
                        Ok(_) => {
                            ProxyEventObserver::publish_event(
                                ProxyEvent::new(
                                    ProxyEventType::GrpcStreamingStreamedResponse,
                                    sdk_key.to_string(),
                                )
                                .with_stat(EventStat {
                                    operation_type: OperationType::IncrByValue,
                                    value: 1,
                                }),
                            )
                            .await;
                        }
                        Err(_e) => {
                            ProxyEventObserver::publish_event(
                                ProxyEvent::new(
                                    ProxyEventType::GrpcStreamingStreamDisconnected,
                                    sdk_key.to_string(),
                                )
                                .with_stat(EventStat {
                                    operation_type: OperationType::IncrByValue,
                                    value: 1,
                                }),
                            )
                            .await;
                            break;
                        }
                    },
                    Err(e) => {
                        eprintln!("Error on broadcast receiver: {:?}", e);
                        rc = ubc_ref
                            .read()
                            .await
                            .get(&sdk_key)
                            .expect("Already created")
                            .sender
                            .read()
                            .await
                            .subscribe();
                    }
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

pub struct GrpcServer {}

impl GrpcServer {
    pub async fn start_server(
        config_spec_store: Arc<ConfigSpecStore>,
        shared_dcs_observer: Arc<HttpDataProviderObserver>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let addr = "0.0.0.0:50051".parse().unwrap();
        let greeter = StatsigForwardProxyServerImpl::new(config_spec_store, shared_dcs_observer);
        println!("GrpcServer listening on {}", addr);

        Server::builder()
            .add_service(StatsigForwardProxyServer::new(greeter))
            .serve(addr)
            .await?;

        Ok(())
    }
}
