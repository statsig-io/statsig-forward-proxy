# Monitoring

The forward proxy comes pre-built with a few different options for monitoring:
1. **--debug-logging**: With no additional configuration, we will output event information to stdout. This is best used for debugging and assisting while deployment for quick understanding of functionality. This is not recommended for prod.
1. **--statsig-logging**: With configuration, send your metrics to Statsig and monitor through our dashboards and metric explorer capabilities.
1. **--statsd-logging**: With configuration, utilize the [open standard](https://github.com/statsd/statsd/blob/master/docs/metric_types.md) to emit metrics to a UDS or UDP socket.
1. **--datadog-logging**: Same as statsd, except replaces the timing metric with datadog's proprietary distribution metric.

## Configuration

### Debug Logging

To get debug logging working, simply pass in the flag:

```
statsig-forward-proxy http --debug-logging
```

### Statsig Logging

To get Statsig logging working, pass in the flag and set your sdk key using environment variables:

```
STATSIG_SERVER_SDK_KEY="your_key" statsig-forward-proxy http --statsig-logging
```

### Statsd and Datadog Logging

By default, both of these logging modes will use UDP to 127.0.0.1:8125. To enable either statsd or datadog logging, you can pass in these arguments:

```
statsig-forward-proxy http --datadog-logging
statsig-forward-proxy http --statsd-logging
```

If you would like to override the host/port for UDP, then you can do this by passing in the environment variables:

```
STATSD_HOST_OVERRIDE=192.168.0.1  STATSD_PORT_OVERRIDE=1337 statsig-forward-proxy http --statsd-logging
```

If you want to use UDS, then you can set statsd_socket and the host/port options will be ignored:

```
STATSD_SOCKET=/tmp/statsig-uds-socket statsig-forward-proxy http --statsd-logging
```

Along with this, we offer more advance configuration, feel free to send us a message on slack if you think its applicable to you, but unsure (Note: We will be swapping the datadog specific terminology in our v2 release):
- datadog_max_batch_time_ms: This sets the maximum time in milliseconds to wait before sending a batch of metrics.
- datadog_max_batch_event_count: This defines the maximum number of events to accumulate before sending a batch of metrics.
- dogstatsd_max_buffer_size: This specifies the maximum size of the buffer for raw bytes from the statsd metrics.
- dogstatsd_max_time_ms: This sets the maximum time in milliseconds before flushing the raw bytes from the statsd metrics.
- dogstatsd_max_retry_attempt: This determines the maximum number of retry attempts for sending statsd metrics.
- dogstatsd_initial_retry_delay: This sets the initial delay in milliseconds before the first retry attempt, allowing for temporary issues to resolve before retrying.
- datadog_sender_buffer_size: This sets the size of the buffer for the channel sending metrics to the statsd client.

## Types of Events Emitted

We emit a number of events that allow you to monitor and ensure that the forward proxy is working correctly. In the ideal world, these are for understanding as our goal is to make the service 100% no touch.

> Note: Everything will be prefixed with ```statsig.forward_proxy.``` in statsig, statsd, and datadog.

### HttpServerRequestSuccess

- **Description**: Indicates a successful HTTP server request from SDKs -> Proxy
- **Event Unit Type**: Latency in ms
- **Useful Dimensions**: path, sdk_key
- **How to Interpret**:
  - Count total data points to get volume
  - Take percentile/average to understand request latency
- **Is it working?**: When the volume of success drops significantly it means that the proxy is either not receiving traffic or is in a bad state.

### HttpServerRequestFailed

- **Description**: Indicates a failed HTTP server request from SDKs -> Proxy
- **Event Unit Type**: Latency in ms
- **Useful Dimensions**: path, sdk_key
- **How to Interpret**:
  - Count total data points to get volume
  - Take percentile/average to understand request latency
- **Is it working?**: When the volume of failure increases signficantly, it means the proxy is unable to get payloads for a given sdk_key.

### HttpDataProviderGotData

- **Description**: Indicates a successful HTTP server request from Proxy -> Origin/CDN where it also received a new configuration payload compared to what is cached internally.
- **Event Unit Type**: Latency in ms
- **Useful Dimensions**: path, sdk_key, lcut
- **How to Interpret**:
  - Count total data points to get volume
  - Take percentile/average to understand request latency
  - lcut, also known as last config update time, lets you know the timestamp of when the served configuration was generated
- **Is it working?**: This should match one-to-one with your project configuration updates.

### HttpDataProviderNoData

- **Description**: Indicates a successful HTTP server request from Proxy -> Origin/CDN, but no new configuration was received.
- **Event Unit Type**: Latency in ms
- **Useful Dimensions**: path, sdk_key, lcut
- **How to Interpret**:
  - Count total data points to get volume
  - Take percentile/average to understand request latency
  - lcut, also known as last config update time, lets you know the timestamp of when the served configuration was generated
- **Is it working?**: This should be the inverse of HttpDataProviderGotData, so the combination of two metrics should show consistant data points.

### HttpDataProviderNoDataDueToBadLcut

- **Description**: This should never show up, but if it does, it means that we sent an lcut value down from our origin which is invalid and can't be parsed as a number
- **Event Unit Type**: Latency in ms
- **Useful Dimensions**: path, sdk_key, lcut
- **How to Interpret**:
  - Count total data points to get volume
  - Take percentile/average to understand request latency
  - lcut, also known as last config update time, lets you know the timestamp of when the served configuration was generated
- **Is it working?**: If this shows up at all, please notify statsig over slack as it should be impossible.

### HttpDataProviderError

- **Description**: Indicates a failed HTTP server request from Proxy -> Origin/CDN.
- **Event Unit Type**: Latency in ms
- **Useful Dimensions**: path, sdk_key
- **How to Interpret**:
  - Count total data points to get volume
  - Take percentile/average to understand request latency
- **Is it working?**: This can occur time to time, but does not mean that SDK -> proxy requests are failing. If this is happening consistantly, this will prevent configurations from propagating. Please check https://status.statsig.com, and if there are no updates, message us on slack.

### RedisCacheWriteSucceed

- **Description**: If using redis caching, indicates a successful write of the latest configuration to redis.
- **Event Unit Type**: Count
- **Useful Dimensions**: path, sdk_key
- **How to Interpret**:
  - Sum total data points to get volume
- **Is it working?**: This should be one-to-one with HttpDataProviderGotData.

### RedisCacheWriteFailed

- **Description**: If using redis caching, indicates a failed write of the latest configuration to redis.
- **Event Unit Type**: Count
- **Useful Dimensions**: path, sdk_key
- **How to Interpret**:
  - Sum total data points to get volume
- **Is it working?**: If this is happening consistantly, please validate that your redis instance/cluster is operating normally.

### RedisCacheReadSucceed

- **Description**: If using redis caching, if a request to our CDN/Origin fails, it will check redis to see if there is a newer payload.
- **Event Unit Type**: Count
- **Useful Dimensions**: path, sdk_key
- **How to Interpret**:
  - Sum total data points to get volume
- **Is it working?**: If this is happening consistantly, please validate that your redis instance/cluster is operating normally.

### RedisCacheReadMiss

- **Description**: If using redis caching, if a request to our CDN/Origin fails, it will check redis to see if there is a newer payload, however, when we checked redis there was no entry.
- **Event Unit Type**: Count
- **Useful Dimensions**: path, sdk_key
- **How to Interpret**:
  - Sum total data points to get volume
- **Is it working?**: This should only happen on a cold cache where your entire system has not seen an sdk_key. If this occurs consistantly it is possible something is causing redis to be flushed.

### RedisCacheReadFailed

- **Description**: If using redis caching, if a request to our CDN/Origin fails, it will check redis to see if there is a newer payload. This metric means that this backup read failed.
- **Event Unit Type**: Count
- **Useful Dimensions**: path, sdk_key
- **How to Interpret**:
  - Sum total data points to get volume
- **Is it working?**: If this is happening consistantly, please validate that your redis instance/cluster is operating normally.

### RedisCacheWriteSkipped

- **Description**: If using redis caching, we ensure only one proxy instance is writing at any given time. This allows to you check which proxy instances are not doing RedisCacheWriteSucceed.
- **Event Unit Type**: Count
- **Useful Dimensions**: path, sdk_key
- **How to Interpret**:
  - Sum total data points to get volume
- **Is it working?**: This should be the inverse of RedisCacheWriteSucceed, such that the sum of the two metrics is the total number of pods when grouping by path + sdk_key.


### RedisCacheDeleteSucceed

- **Description**: On a 403 or 401, we have an option that allows you to clear the data from redis as well. If that feature is enabled, once a key is invalidated, this counter should be bumped.
- **Event Unit Type**: Count
- **Useful Dimensions**: path, sdk_key
- **How to Interpret**:
  - Sum total data points to get volume
- **Is it working?**: This metric should only fire whenever an sdk_key is deleted from Statsig.

### RedisCacheDeleteFailed

- **Description**: On a 403 or 401, we have an option that allows you to clear the data from redis as well. If that feature is enabled, once a key is invalidated, this counter should be bumped. However, when we attempted, it failed.
- **Event Unit Type**: Count
- **Useful Dimensions**: path, sdk_key
- **How to Interpret**:
  - Sum total data points to get volume
- **Is it working?**: If this counter is bumped, all configuration payloads are loaded into redis with a TTL. This means it will still be removed from the redis datastore automatically after some time.

### ConfigSpecStoreGotData

- **Description**: Our internal in memory cache has received a new payload for a given sdk_key.
- **Event Unit Type**: Count
- **Useful Dimensions**: path, sdk_key, lcut
- **How to Interpret**:
  - Sum total data points to get volume
  - lcut, also known as last config update time, lets you know the timestamp of when the served configuration was generated
- **Is it working?**: This should be 1:1 with HttpDataProviderGotData.

### GrpcStreamingStreamedInitialized

- **Description**: Signifies that a gRPC streaming connection was successfully initialized.
- **Event Unit Type**: Count
- **Useful Dimensions**: path, sdk_key
- **How to Interpret**:
  - Sum total data points to get volume
- **Is it working?**: This should be 1:1 with new pod initialization.

### GrpcStreamingStreamedResponse

- **Description**: Indicates that a response was streamed over the gRPC connection.
- **Event Unit Type**: Count
- **Useful Dimensions**: path, sdk_key
- **How to Interpret**:
  - Sum total data points to get volume
- **Is it working?**: There should be one data point for every new configuration we receive (HttpDataProviderGotData) multiplied by the total active GRPC connections.


### GrpcStreamingStreamDisconnected

- **Description**: Signifies that a gRPC streaming connection was disconnected.
- **Event Unit Type**: Count
- **Useful Dimensions**: path, sdk_key
- **How to Interpret**:
  - Sum total data points to get volume
- **Is it working?**: This should roughly match with pod shutdown, however, transient issues can occur on the client application level/networking stack that can cause slightly higher volume.

### StreamingChannelGotNewData

- **Description**: Indicates that new data was received on a streaming channel which is about to be distributed to all GRPC connections.
- **Event Unit Type**: Count
- **Useful Dimensions**: path, sdk_key, lcut
- **How to Interpret**:
  - Sum total data points to get volume
  - lcut, also known as last config update time, lets you know the timestamp of when the served configuration was generated
- **Is it working?**: This should be 1:1 with HttpDataProviderGotData.

### UpdateConfigSpecStorePropagationDelayMs

- **Description**: Represents the difference in ms between current time on the proxy and the latest configuration update that was received
- **Event Unit Type**: Latency in ms
- **Useful Dimensions**: path, sdk_key, lcut
- **How to Interpret**:
  - Count total data points to get volume
  - Take percentile/average to understand propagation latency
  - lcut, also known as last config update time, lets you know the timestamp of when the served configuration was generated
- **Is it working?**: This should match one-to-one with your project configuration updates. P99 is expected to be around 30s after taking into account all our caching layers.

## Common Issues and Questions

### What are the most important metrics to monitor?

HttpServerRequestSuccess, HttpServerRequestFailed, HttpDataProviderGotData, and UpdateConfigSpecStorePropagationDelayMs.

These allow you to ensure:
- the proxy is servicing requests
- the proxy is getting updates
- the proxy is receiving timely updates

### What does this mean? [sfp] Dropping event... Buffer limit hit...

This log is emitted due to the high volume of events being generated. This is directly proportional to QPS your service is receiving.

To address this, increase the buffer size by setting a larger EVENT_CHANNEL_SIZE value (default is 100000). Note will cause service memory to increase:

```
EVENT_CHANNEL_SIZE=1000000000 statsig-forward-proxy http --debug-logging
```

If this is still happening, it can occur for a number of reasons such as back pressure from the UDP/UDS socket preventing us from flushing our buffer fast enough.

Feel free to reach out to us on slack and we'll do our best to help.