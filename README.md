# Statsig Forward Proxy

This proxy was developed with the intention of being deployed in customer clusters. The purpose is to improve reliability by reducing the dependence of every server in your cluster requesting for download_config_specs independently. When deployed, you should be able to get the following benefits:
1. Reduced dependency on Statsig Infrastructure Availability
2. Improved performance through localization of download_config_spec to your cluster
3. Improved cost savings and reduced network overhead through minimizing requests to Statsig Infrastructure

# Releases

Our release can be found here: https://hub.docker.com/r/statsig/statsig-forward-proxy
- All stable builds have tags that are prefixed with "stable", for example: [stable-amd64-*](https://hub.docker.com/r/statsig/statsig-forward-proxy/tags?page=&page_size=&ordering=&name=stable)
- If you want to work off bleeding edge, we generate the "[latest](https://hub.docker.com/r/statsig/statsig-forward-proxy/tags?page=&page_size=&ordering=&name=latest)" tag as code lands (give or take 10 minutes). If there's a particular datetime latest you are looking for, we also tag each one with the "[YYMMDDHHmmss](https://hub.docker.com/layers/statsig/statsig-forward-proxy/240404230603-bff2e9385632/images/sha256-0a2a2cc7a338510429ae0b611248be9f3c3371c1e3e046f070b4bbbd5bf62104?context=explore)" prefix.
- If you want "nightly", we generate daily branch cuts with the tag prefix "DDMMYY", for example, the April 4th nightly build is "[240404-1ffdf554b54b](https://hub.docker.com/layers/statsig/statsig-forward-proxy/240404-1ffdf554b54b/images/sha256-cf50a0584a58395c40e97be2053f78e27e718047243d814708100887fafa08fb?context=explore)". This is a daily branch cut that occurs at midnight.

# Deploying

You could optionally build your own binary, however, we have provided a pre-built amd64/linux build. When starting up the binary, either standalone or through docker. You have various options to configure:

```
Usage: server [OPTIONS] <MODE> <CACHE>

Arguments:
  <MODE>   [possible values: grpc-and-http, grpc, http]
  <CACHE>  [possible values: local, redis]

Options:
      --statsd-logging
      --datadog-logging
      --debug-logging
  -m, --maximum-concurrent-sdk-keys <MAXIMUM_CONCURRENT_SDK_KEYS>  [default: 1000]
  -p, --polling-interval-in-s <POLLING_INTERVAL_IN_S>              [default: 10]
  -u, --update-batch-size <UPDATE_BATCH_SIZE>                      [default: 64]
  -r, --redis-leader-key-ttl <REDIS_LEADER_KEY_TTL>                [default: 70]
      --redis-cache-ttl-in-s <REDIS_CACHE_TTL_IN_S>                [default: 86400]
      --force-gcp-profiling-enabled
  -g, --grpc-max-concurrent-streams <GRPC_MAX_CONCURRENT_STREAMS>  [default: 500]
      --clear-external-datastore-on-unauthorized
  -h, --help                                                       Print help
  -V, --version                                                    Print version
```
1. MODE:  This can be configured as grpc or http or grpc-and-http to allow for easy migrations.
2. CACHE: local uses in process memory to cache backup values, while redis, will use redis. Redis
          will store the config in a single key entry that ends up being "statsig::{sha256(sdk_key)}".
          This allows you read the value back from the SDK data adapter if needed.

Additional logging parameters we support:
```
--debug-logging: This enables state tracking logging that is emitted for various events within the proxy.
                 It will also emit useful data such as latency for some events.
--statsd-logging: This will emit the same metrics and events using statsd.
--datadog-logging: Same as statsd logging, but uses distribution metrics instead of timing
--force_gcp_profiling_enabled: Enable gcp cloud profiler by force.
--clear-external-datastore-on-unauthorized: When a 401/403 is received, clear external caches (such as redis). Noting that this is a potential reliability trade off.
```

# Configuration

If you are using additionaly dependencies such as, redis or datadog, take a look at the implementation files
in order to get an idea of what environment variables you may need to set to ensure those dependencies are
configured correctly.

# Recommended Methods of Deployment

We recommend two main modes of deploying the statsig forward proxy:
1. Active Proxy: Directly request for config spec from forward proxy to relieve read
   qps pressure from your caching layer. Downside is that if the forward proxy goes down
   it should be prepared to handle an increase in traffic.
2. Passive Proxy: Directly read config spec from caching layer. Downside is if the forward
   proxy becomes unavailable, then you will need to make a lot network requests to our CDN
   which will have increased latency.

<img width="636" alt="Screenshot 2024-04-03 at 7 26 56 PM" src="https://github.com/statsig-io/statsig-forward-proxy/assets/111610731/6f24c712-1423-4cb7-af07-aaeaf0f0d91f">

# Architecture

The software architecture is designed as follows:

<img width="1075" alt="Screenshot 2024-04-03 at 7 48 00 PM" src="https://github.com/statsig-io/statsig-forward-proxy/assets/111610731/4b98fd95-55d0-405a-8800-b7cc1c2bb6b2">

# Future Development

We are actively working on enabling the forward proxy to support /v1/log_event calls as well. This will
provide similar improvements in reliability, bandwidth, and performance for your local services.

# Local Run

Run http server with redis cache (you may need to modify for your local redis environment):

./run_locally.zsh

Or to test in memory standalone run you can do:

cargo run --bin server http local --debug-logging

---

curl -X GET 'http://0.0.0.0:8000/v1/download_config_specs/<INSERT_SDK_KEY>.json

# GCP Cloud Profiling

Besides using the flag force_gcp_profiling_enabled, if you provide your statsig SDK key as an environment
variable (STATSIG_SERVER_SDK_KEY), you can also configure a feature gate called enable_gcp_profiler_for_sfp
to dynamically enable and disable gcp profiling.
