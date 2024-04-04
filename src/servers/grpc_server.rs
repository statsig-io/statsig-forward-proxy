use tonic::{transport::Server, Request, Response, Status};

use crate::datastore::config_spec_store::ConfigSpecStore;

use crate::datastore::{self};
use statsig_forward_proxy::statsig_forward_proxy_server::{
    StatsigForwardProxy, StatsigForwardProxyServer,
};
use statsig_forward_proxy::{ConfigSpecRequest, ConfigSpecResponse};
use std::sync::Arc;

pub mod statsig_forward_proxy {
    tonic::include_proto!("statsig_forward_proxy");
}

pub struct StatsigForwardProxyServerImpl {
    config_spec_store: Arc<datastore::config_spec_store::ConfigSpecStore>,
}

impl StatsigForwardProxyServerImpl {
    fn new(config_spec_store: Arc<ConfigSpecStore>) -> Self {
        StatsigForwardProxyServerImpl { config_spec_store }
    }
}

#[tonic::async_trait]
impl StatsigForwardProxy for StatsigForwardProxyServerImpl {
    async fn get_config_spec(
        &self,
        request: Request<ConfigSpecRequest>,
    ) -> Result<Response<ConfigSpecResponse>, Status> {
        let reply = ConfigSpecResponse {
            spec: match self
                .config_spec_store
                .get_config_spec(
                    &request.get_ref().sdk_key,
                    request.get_ref().since_time.unwrap_or(0),
                )
                .await
            {
                Some(spec) => spec.to_string(),
                None => "Unauthorized".to_string(),
            },
        };
        Ok(Response::new(reply))
    }
}

pub struct GrpcServer {}

impl GrpcServer {
    pub async fn start_server(
        config_spec_store: Arc<ConfigSpecStore>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let addr = "[::1]:50051".parse().unwrap();
        let greeter = StatsigForwardProxyServerImpl::new(config_spec_store);
        println!("GrpcServer listening on {}", addr);

        Server::builder()
            .add_service(StatsigForwardProxyServer::new(greeter))
            .serve(addr)
            .await?;

        Ok(())
    }
}
