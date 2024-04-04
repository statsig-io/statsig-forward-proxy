# Statsig Forward Proxy

This proxy was developed with the intention of being deployed in customer clusters. The purpose is to improve reliability by reducing the dependence of every server in your cluster requesting for download_config_specs independently. When deployed, you should be able to get the following benefits:
1. Reduced dependency on Statsig Infrastructure Availability
2. Improved performance through localization of download_config_spec to your cluster
3. Improved cost savings and reduced network overhead through minimizing requests to Statsig Infrastructure

# Recommended Methods of Deployment

We recommend two main modes of deploying the statsig forward proxy:
1. Active Proxy: Directly request for config spec from forward proxy to relieve read
   qps pressure from your caching layer. Downside is that if the forward proxy goes down
   it should be prepared to handle an increase in traffic.
2. Passive Proxy: Directly read config spec from caching layer. Downside is if the forward
   proxy becomes unavailable, then you will need to make a lot network requests to our CDN
   which will have increased latency.

<img width="636" alt="Screenshot 2024-04-03 at 7 26 56 PM" src="https://github.com/statsig-io/private-statsig-forward-proxy/assets/111610731/2c4c66e0-7c0d-454f-8a23-d0945c6a150d">

# Architecture

The software architecture is designed as follows:

<img width="1088" alt="Screenshot 2024-04-03 at 3 58 19 PM" src="https://github.com/statsig-io/private-statsig-forward-proxy/assets/111610731/9d0b1e2a-2282-4cb3-81be-63e93043cc2f">

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
