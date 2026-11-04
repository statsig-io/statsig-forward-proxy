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

## Using Helm Chart

For Kubernetes deployments, we provide a Helm chart that simplifies the installation and configuration process:

```bash
# Add the Statsig Helm repository
helm repo add statsig https://statsig-helm.storage.googleapis.com
helm repo update

# Install the chart
helm install statsig-forward-proxy statsig/statsig-forward-proxy
```

For detailed configuration options and more advanced installation scenarios, please refer to the [Helm chart README](./chart/README.md).

## Manual Deployment

You could optionally build your own binary, however, we have provided a pre-built amd64/linux build. When starting up the binary, either standalone or through docker. You have various options to configure:

```
Usage: server [OPTIONS] <MODE> <CACHE>

Arguments:
  <MODE>   [possible values: grpc-and-http, grpc, http]
  <CACHE>  [possible values: disabled, redis]

Options:
      --double-write-cache-for-legacy-key

      --datadog-logging

      --statsd-logging

      --statsig-logging

      --debug-logging

  -m, --maximum-concurrent-sdk-keys <MAXIMUM_CONCURRENT_SDK_KEYS>
          [default: 1000]
  -p, --polling-interval-in-s <POLLING_INTERVAL_IN_S>
          [default: 10]
  -u, --max-in-flight <MAX_IN_FLIGHT>
          [default: 64]
      --background-poll-item-spacing-ms <BACKGROUND_POLL_ITEM_SPACING_MS>
          [default: 0]
  -r, --redis-leader-key-ttl <REDIS_LEADER_KEY_TTL>
          [default: 70]
      --redis-cache-ttl-in-s <REDIS_CACHE_TTL_IN_S>
          [default: 86400]
      --log-event-process-queue-size <LOG_EVENT_PROCESS_QUEUE_SIZE>
          [default: 20000]
      --force-gcp-profiling-enabled

  -g, --grpc-max-concurrent-streams <GRPC_MAX_CONCURRENT_STREAMS>
          [default: 500]
      --clear-datastore-on-unauthorized

      --x509-server-cert-path <X509_SERVER_CERT_PATH>

      --x509-server-key-path <X509_SERVER_KEY_PATH>

      --x509-client-cert-path <X509_CLIENT_CERT_PATH>

      --enforce-tls
          [default: false]
      --enforce-mtls

      --http-connection-pool-max-idle-per-host <HTTP_CONNECTION_POOL_MAX_IDLE_PER_HOST>
          [default: 10]
      --http-request-timeout-in-s <HTTP_REQUEST_TIMEOUT_IN_S>
          [default: 30]
      --http-read-timeout-in-s <HTTP_READ_TIMEOUT_IN_S>
          [default: 10]
      --http-connect-timeout-in-s <HTTP_CONNECT_TIMEOUT_IN_S>
          [default: 10]
      --id-list-file-refresh-observer-enabled

  -h, --help
          Print help
  -V, --version
          Print version
```
1. MODE:  This can be configured as grpc or http or grpc-and-http to allow for easy migrations.
2. CACHE: This is for the backup cache. Local uses in process memory to cache backup values, while redis,
          will use redis. Redis will store the config in a single key entry that ends up being "statsig::{sha256(sdk_key)}".
          This allows you read the value back from the SDK data adapter if needed. We recommend using
          redis or disabled. Local was primarily designed for testing and not to be used in production
          as it is an exact duplicate of the datastores that are also stored in memory.

Additional logging parameters we support:
```
--debug-logging: This enables state tracking logging that is emitted for various events within the proxy.
                 It will also emit useful data such as latency for some events.
--double-write-cache-for-legacy-key: Starting in version 2.x.x, we begin to use a new key schema in preparation for SDKs to manage all key creation in external caches. This allows you to gracefully migrate by double writing to the old key as well.
--statsig-logging: Send events to Statsig to monitor performance and behaviour of proxy.
--statsd-logging: This will emit the same metrics and events using statsd.
--datadog-logging: Same as statsd logging, but uses distribution metrics instead of timing
--force_gcp_profiling_enabled: Enable gcp cloud profiler by force.
--clear-datastore-on-unauthorized: When a 401/403 is received, clear external caches (such as redis), as well as, internal caches. Noting that this is a potential reliability trade off.
```

### Background Polling And Request Backoff

These flags control how the proxy paces background refresh traffic and how it backs off per key after upstream failures:

```
--polling-interval-in-s: Primary background poll cadence. Each cycle adds a small per-process jitter so pods do not wake up in lockstep. Also the initial per-key retry delay after an upstream fetch error. Consecutive failures double from this value and cap at 300s.
--max-in-flight: Maximum number of background refresh requests allowed in flight at once. This concurrency limit also applies to startup warmup fetches.
--background-poll-item-spacing-ms: Minimum delay between background request launches within a poll cycle. Use this to stagger bursts across pods. 0 disables pacing.
--clear-datastore-on-unauthorized: On 401/403, clear caches instead of serving stale data. Unauthorized responses also clear request-backoff state so the next poll can retry immediately after credentials are fixed.
--id-list-file-refresh-observer-enabled: Enables the observer that reads `/v1/get_id_lists` manifests and asynchronously refreshes referenced `/v1/download_id_list_file` payloads. Disabled by default.
```

Related environment variable:
`TOKIO_WORKER_THREADS_BACKGROUND`: Worker thread count for the dedicated Tokio runtime used by background polling loops. Must be a positive integer. Defaults to host CPU count.
`SFP_IDLIST_SYNC_KEYS_JSON`: JSON array of server keys. When set, enables the reverse-index sync producer, one worker per pod, and `POST /v1/user_enrichment`. Requires Redis cache mode. Unset leaves all three off.

`IDLIST_ENABLE_REVERSE_INDEX`: Boolean (`true` or `false`), default `true`.
With `true`, synchronization and membership lookups use the existing reverse index.
With `false`, synchronization maintains versioned forward sets and checkpoints but
skips reverse-index writes, rotation diff fan-out, and reverse-index removal scans.
Membership lookups check each list visible in the authenticated server key's manifest,
using pipelined `SMISMEMBER` lookups against committed forward sets. If a list has
no committed generation yet, lookups use its loading generation. They do not
query other lists in the company's manifest union. This requires Redis 6.2 or newer.
Each request performs work proportional to distinct lookup IDs multiplied by visible
lists, so large manifests increase Redis CPU usage and response size. Forward-only
reads use two pipelined round trips: version metadata, then membership checks.
The existing fetch timeout covers both and pool acquisition. Forward-only readers
do not load or refresh token dictionaries. Forward-only membership requests return
HTTP 200 with available matches, including during recovery. Missing state or sets
contribute no matches; results can be partial or stale while synchronization runs.
Actual Redis command failures and timeouts still return HTTP 503.

All producers, workers, and readers sharing a Redis key prefix must use the same
setting. Stop the old fleet before changing modes; mixed-mode rolling deployments
are unsupported. Disabling does not delete existing reverse keys or reclaim their
memory automatically. Lists changed while disabled are marked for full reconciliation
when re-enabled. Existing forward sets and checkpoints are reused when disabling.

If the manifest still advertises an older file when reverse indexing is re-enabled,
repair uses the committed file's metadata and never rolls back to that older file.
A complete committed forward set is reused to rebuild reverse mappings without a
download. Missing or unverified forward sets use normal download recovery for the
committed file. Repair markers remain until the reverse rebuild succeeds.

ID-list storage is shared by company. The proxy discovers each configured key's
`company_id` from a full v2 JSON config-spec response at startup, retrying missing
identities every 30 seconds through the existing HTTP provider. No-update
responses preserve the mapping. Redis namespace identifiers are
`base64(SHA256(company_id))[0:4]`; the configured keys remain a JSON string array.
Reverse membership keys use `i:<C><M>`, where `C` is this four-character namespace
and `M` is the eight-character member hash. Without a configured Redis key prefix,
each reverse key is 14 bytes. Short namespaces have 24 bits of collision space;
distinct companies sharing Redis must have different namespace hashes.
Before generating tasks, the elected producer checks all discovered configured
company IDs for namespace collisions. If any collide, it logs each conflicting
namespace and its company IDs and fails the entire generation tick before writing
tasks or readiness. This detects collisions among identities known to that producer;
it does not resolve collisions or change existing queued tasks or membership reads.

The producer synchronizes the union of the configured keys' manifests once per
company. Shared list names produce one task, using the largest offset for the
same file or the newest generation across snapshots. Missing manifests suppress
company removals and readiness publication; an unresolved key's company suppresses
both for all companies until its identity is known. All producer pods sharing a
Redis key prefix must use the same configured key set, so they plan the same union.

For an already synchronized list, a different `fileID` triggers a full load only
when its `creationTime` is strictly newer than the committed file's. Equal or older
rotations are ignored, even if their size grew. For the same `fileID`, creation time
is ignored: size growth downloads the appended bytes; equal or smaller sizes do
not schedule work. First loads and recovery of interrupted or missing current-file
Redis state still use the full-load recovery path.

Enrichment resolves the authenticated key to its company namespace, then filters
membership names against that key's latest successful manifest. A missing company
mapping or manifest returns HTTP 200 with empty membership results. An empty
manifest also returns empty membership results. Dictionary caches are shared within a
pod by company; authentication and rate limits remain per key.

Changing the namespace hash length or reverse key format builds a fresh index,
as does changing from server-key namespaces to company namespaces;
old Redis namespaces are not migrated or deleted. During rollout, lookups can
return empty or partial memberships while the new index builds. A known empty
manifest does not require a Redis index. Avoid mixing old and new producer versions
or workers: the task queue is shared across formats, so an old worker can consume a
new-format task and publish readiness for an incompatible reverse index. Stop old
producers, finish or discard their pending work, and stop all old workers before
starting the new version. Discarded tasks also require clearing their task metadata.
Rebuild the new index with
enough memory for any retained old namespaces, or clean up the old index while
serving is stopped. Do not use a rolling upgrade across this format change.

Idlist sync worker tuning:

Normal version changes use partitioned streaming diff for every list size. Lists
below the partition target use one partition. First loads and transitions from an
empty list directly fan out the new members without building a difference set.

- `IDLIST_QUEUE_POLL_INTERVAL_MS` (default `2000`)
- `IDLIST_LOCK_TTL_IN_S` (default `60`; renewed every third of the TTL)
- `IDLIST_PIPELINE_BATCH_SIZE` (default `5000`)
- `IDLIST_SCAN_COUNT` (default `1000`)
- `IDLIST_DIFF_PARTITION_MAX_MEMBERS` (default `2000000`)
- `IDLIST_OLD_VERSION_TTL_IN_S` (default `86400`)

Idlist membership request-path tuning:
- `IDLIST_REDIS_POOL_SIZE` (default `32`; dedicated read pool)
- `IDLIST_REDIS_READ_URI` (optional `redis://` or `rediss://` read-replica address; defaults to the primary Redis address)
- `IDLIST_FETCH_TIMEOUT_MS` (default `50`)
- `IDLIST_DICT_REFRESH_IN_S` (default `60`)

Membership requests return HTTP 200 with the memberships currently available,
without checking namespace or list readiness, cardinality, or recovery flags.
Missing member keys and unknown dictionary tokens contribute no memberships.
Missing dictionary entries leave those memberships unresolved. During startup,
rebuilding, or mode changes, results can be partial or stale; an empty result does not guarantee that source data excludes the ID.
Actual Redis command failures and timeouts still return HTTP 503.

Example direct image invocation with burst smoothing:

```bash
docker run --rm -p 8000:8000 -p 50051:50051 \
  -e STATSIG_SERVER_SDK_KEY=secret-server-key \
  statsig/statsig-forward-proxy:latest \
  http disabled \
  --polling-interval-in-s 15 \
  --max-in-flight 16 \
  --background-poll-item-spacing-ms 25 \
  --clear-datastore-on-unauthorized \
  --id-list-file-refresh-observer-enabled
```

### Outbound Fetch HTTP Client

These flags control the outbound HTTP client used for config spec, id list, and id list file fetches:

```
--http-connection-pool-max-idle-per-host: Maximum idle connections to keep per upstream host. Default 10.
--http-request-timeout-in-s: Total request timeout for outbound fetches. Default 30.
--http-read-timeout-in-s: Read timeout for outbound fetches. Default 10.
--http-connect-timeout-in-s: Connect timeout for outbound fetches. Default 10.
```

These settings apply to the background and foreground data-fetch paths. They do not change Redis, nginx, gRPC, or log-event client timeouts.

Example direct image invocation:

```bash
docker run --rm -p 8000:8000 -p 50051:50051 \
  -e STATSIG_SERVER_SDK_KEY=secret-server-key \
  statsig/statsig-forward-proxy:latest \
  http disabled \
  --http-request-timeout-in-s 45 \
  --http-read-timeout-in-s 20 \
  --http-connect-timeout-in-s 5 \
  --http-connection-pool-max-idle-per-host 25
```

### Startup Warm-up Keys

On startup, the proxy can prefetch a fixed set of SDK/path combinations before the normal background polling loop begins. Configure this with `SFP_STARTUP_WARMUP_KEYS_JSON`.

The value must be a JSON array. Each item requires:
- `sdk_key`: the Statsig SDK key to warm. It must start with `client-`, `server-`, or `secret-`.
- `path`: one of `/v1/download_config_specs`, `/v2/download_config_specs`, or `/v1/get_id_lists`
- `encodings`: optional list of request-context encodings to warm

Example:

```json
[
  {
    "sdk_key": "secret-client-key",
    "path": "/v1/download_config_specs",
    "encodings": ["gzip", "statsig-br"]
  },
  {
    "sdk_key": "secret-client-key",
    "path": "/v2/download_config_specs",
    "encodings": ["gzip", "statsig-br"]
  },
  {
    "sdk_key": "secret-client-key",
    "path": "/v1/get_id_lists",
    "encodings": ["gzip"]
  }
]
```

Notes:
- Invalid entries are skipped individually. Startup logs include a configured/valid/invalid summary so you can spot bad payloads without losing the valid entries.
- In current proxy versions, omitted or unsupported encodings fall back to the plain-text request-context behavior. If you want a compressed warm-up entry, specify `gzip` explicitly.
- `statsig-br` is only useful for config-spec warm-up entries that should target that distinct cache key.
- This environment variable contains SDK keys. In Kubernetes, prefer injecting it from a Secret rather than placing it directly in a ConfigMap-backed values block.

# Configuration

If you are using additionaly dependencies such as, redis or datadog, take a look at the implementation files
in order to get an idea of what environment variables you may need to set to ensure those dependencies are
configured correctly.

For the redis cache specifically, see [Redis Authentication](docs/redis_auth.md), which documents the
`REDIS_*` environment variables and the supported auth modes (static password/ACL, and optional
OAuth via Microsoft Entra ID).

# Nginx Caching

We leverage nginx to leverage it as a passthrough proxy with request queueing, per recommendation of the [rocket framework](https://rocket.rs/guide/v0.5/deploying/#overview).

In addition to this, we leverage it as a cache. To configure this cache/fronting layer, we expose these environment variables:
- PROXY_CACHE_PATH_CONFIGURATION: The root directory at which to store cached data, by default, is /dev/shm/nginx
- PROXY_CACHE_DOWNLOAD_PATH_CONFIGURATION: The directory for download_config_specs cache data, by default, is ${PROXY_CACHE_PATH_CONFIGURATION}/download_cache
- PROXY_CACHE_DOWNLOAD_ID_LIST_PATH_CONFIGURATION: The directory for download_id_list cache data, by default, is ${PROXY_CACHE_PATH_CONFIGURATION}/download_id_list
- PROXY_CACHE_MAX_SIZE_IN_MB: The size of each nginx cache, which by default is 1024mb
- PROXY_CACHE_TTL: TTL for cached 200 responses in nginx (first layer cache), default 5s (must include a unit, e.g. 500ms, 5s, 1m)
- NGINX_WORKER_PROCESSES: nginx worker process count (`auto` or a positive integer), default `auto`

Note: In most cases, the default size limit for /dev/shm is 64mb, in our next major version release, we plan to align the default value for PROXY_CACHE_MAX_SIZE_IN_MB to this. In most scenarios, this should not matter, however, if your config spec payload is multiple MB, this is something to be aware of.

## Nginx Access Logs

The official image supports three nginx access-log modes for HTTP, HTTPS, and
combined HTTP/HTTPS deployments. Set the `NGINX_ACCESS_LOG` environment variable:

| Value | Behavior |
| --- | --- |
| `errors` (default) | Log requests with a status outside 2xx/3xx to stdout. |
| `all` | Log every request to stdout, including successful requests and redirects. |
| `off` | Disable nginx access logs entirely. |

Unset or empty values use `errors`; unsupported values fail startup. The default
preserves errors-only logging: successful requests are dominated by SDK polling
and are already covered by the `HttpServerRequestSuccess`/`HttpServerRequestFailed`
metrics, while nginx-served failures (for example 502/504 when the Rust proxy is
unavailable) are only visible here. Use `off` to avoid access-log ingest costs, or
`all` when troubleshooting requires a record of every request. Set the variable
when starting the container (for example, `docker run -e NGINX_ACCESS_LOG=off ...`),
or through [`sfp.environment` in Helm values](chart/README.md#example-configure-nginx-access-logs).
Restart the container after changing it. This setting does not change Rust
application logging, metrics, or nginx error-log handling.

The entrypoint runs nginx with
`daemon off` while the Rust proxy remains the main process. This works with the
image's non-root user without a log-file mount, sidecar, or `/proc/1/fd` override;
standard container log collectors can read the access lines.

The format uses the combined-log layout with a sanitized request path and `-` for
user, referer, and user-agent fields. Known route names are retained, while dynamic
path suffixes (including SDK keys in `/v1|v2/download_config_specs/<key>`) are replaced
with `[REDACTED]`. Unknown paths are logged as `[REDACTED]`. All query strings are
omitted, including repeated or encoded `k=` parameters. Request headers and bodies
are never included. Redaction affects logging only, not the forwarded request or
cache key.

For example:

```text
127.0.0.1 - - [17/Sep/2026:12:00:00 +0000] "GET /v2/download_config_specs/[REDACTED] HTTP/1.1" 502 157 "-" "-"
```

Runtime nginx stderr remains suppressed: native nginx error messages can include
raw request URLs and do not support this access-log redaction format. Configuration
validation failures are still printed at startup. This redaction guarantee applies
to the access-log format; custom nginx logging overrides must handle credentials
separately.

Run the nginx logging regression checks (requires Python 3, nginx, and OpenSSL):

```bash
python3 scripts/test_nginx_access_logging.py
```

## Benchmarking Nginx Hot Path

To benchmark and validate nginx caching behavior for `/v1|v2/download_config_specs`, run:

```bash
./scripts/benchmark_nginx_hot_path.py
```

This script starts a mock upstream, renders `nginx-http-only.conf.template`, runs load against the hot path, and reports throughput, latency percentiles, and an estimated cache hit ratio from upstream hit counts. By default, the mock endpoint returns a 5MB JSON payload per response.

When available, nginx cache data is written to `/dev/shm` (RAM disk). Use `--cache-dir` to override.
On macOS, `/dev/shm` is typically unavailable, so use a writable path (for example `/tmp/sfp-nginx-bench`) or a mounted RAM disk path under `/Volumes`.

Use `--payload-size-mb` to override the payload size.
For large payload stress tests, use a higher timeout (default is 15s).

## Load Testing /v1/log_event Throughput

To generate synthetic traffic for a fixed period and measure peak throughput, run:

```bash
scripts/load_test_log_event.py \
  --target-url http://127.0.0.1:8000/v1/log_event \
  --concurrency 128 \
  --warmup-seconds 10 \
  --duration-seconds 60
```

This benchmark is closed-loop (workers continuously issue requests), reports:
- peak 1-second throughput
- peak moving-average throughput
- overall req/s
- non-2xx/transport error counts
- latency percentiles from sampled requests

By default, it posts synthetic `/v1/log_event` payloads and can be tuned with:
- `--events-per-request`
- `--duplicate-ratio`
- `--body-pool-size`

You can also supply your own POST body via `--body-file` or target a GET endpoint with `--method GET`.

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
