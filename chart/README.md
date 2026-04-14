# Statsig Forward Proxy Helm Chart

A Helm chart for Statsig Forward Proxy, a self-managed proxy service that works with Statsig APIs.

Detailed documentation of Statsig Forward Proxy can be found:
- [Statsig documentation](https://docs.statsig.com/server/concepts/forward_proxy)
- [Statsig Forward Proxy repo](https://github.com/statsig-io/statsig-forward-proxy)

## Installation

### Add the Statsig Helm repository

```bash
helm repo add statsig https://statsig-helm.storage.googleapis.com
helm repo update
```

### Install the chart

```bash
# Install the chart with the release name "statsig-forward-proxy"
helm install statsig-forward-proxy statsig/statsig-forward-proxy

# To install with custom values
helm install statsig-forward-proxy statsig/statsig-forward-proxy -f values.yaml
```

### Required Configuration

For Statsig Forward Proxy to function correctly, you must provide your Statsig Server SDK key. You can do this in several ways:

```bash
helm install statsig-forward-proxy statsig/statsig-forward-proxy
```

### Beta Endpoint Support

The `/v1/download_id_list_file` and `/v2/download_config_specs_deltas` endpoints are currently in beta. Their proxy behavior is available for validation, but customers should treat these paths as beta until they are promoted to stable support.

### Example: Tune Outbound Fetch HTTP Client

The chart already forwards raw container arguments through `sfp.args`. To customize the outbound fetch HTTP client used for config and id-list fetches, set the CLI flags there:

```yaml
sfp:
  args:
    - http
    - disabled
    - --http-request-timeout-in-s
    - "45"
    - --http-read-timeout-in-s
    - "20"
    - --http-connect-timeout-in-s
    - "5"
    - --http-connection-pool-max-idle-per-host
    - "25"
```

These flags affect the proxy's outbound data-fetch paths only. They do not change Redis, nginx, gRPC, or log-event client timeouts.

### Example: Tune Background Polling and Request Backoff

Background dispatch and backoff settings are also passed through `sfp.args`:

```yaml
sfp:
  args:
    - http
    - disabled
    - --polling-interval-in-s
    - "15"
    - --max-in-flight
    - "16"
    - --background-poll-item-spacing-ms
    - "25"
    - --clear-datastore-on-unauthorized
    - --id-list-file-refresh-observer-enabled
```

What these flags do:
- `--polling-interval-in-s` sets the primary background poll cadence and the initial per-key retry delay after upstream fetch errors.
- `--max-in-flight` caps background refresh concurrency and also applies to startup warmup fetches.
- `--background-poll-item-spacing-ms` adds delay between launches inside each polling cycle. `0` disables pacing.
- `--clear-datastore-on-unauthorized` clears cached data on 401/403 responses instead of serving stale results.
- `--id-list-file-refresh-observer-enabled` enables asynchronous refreshes for `/v1/download_id_list_file` payloads referenced by `/v1/get_id_lists` manifests. It is disabled by default.

### Example: Configure Startup Warm-up Keys

The proxy can prefetch a fixed set of SDK/path combinations before the steady-state background polling loop starts by setting `SFP_STARTUP_WARMUP_KEYS_JSON`.

The value is a JSON array. Each entry must contain:
- `sdk_key` (must start with `client-`, `server-`, or `secret-`)
- `path`: `/v1/download_config_specs`, `/v2/download_config_specs`, or `/v1/get_id_lists`
- `encodings` (optional): use `["gzip"]` for the common compressed variant; `statsig-br` is only relevant for config-spec warm-up entries

Because this value contains SDK keys, it should usually come from a Kubernetes Secret, not from the chart's ConfigMap-backed `sfp.environment`.

```bash
kubectl create secret generic sfp-startup-warmup \
  --from-literal=SFP_STARTUP_WARMUP_KEYS_JSON='[
    {"sdk_key":"secret-client-key","path":"/v1/download_config_specs","encodings":["gzip","statsig-br"]},
    {"sdk_key":"secret-client-key","path":"/v2/download_config_specs","encodings":["gzip","statsig-br"]},
    {"sdk_key":"secret-client-key","path":"/v1/get_id_lists","encodings":["gzip"]}
  ]'
```

```yaml
sfp:
  secrets:
    - envName: SFP_STARTUP_WARMUP_KEYS_JSON
      secretName: sfp-startup-warmup
      secretKey: SFP_STARTUP_WARMUP_KEYS_JSON
```

At startup, invalid entries are skipped individually and the proxy logs a configured/valid/invalid summary. Omitted or unsupported encodings fall back to the plain-text request-context behavior used by current proxy versions.

## Configurations

The default values file is only intended to be a starting point of configuring Statsig Forward Proxy that works with your environment and setup. Please read the following reference and [deployment options](https://github.com/statsig-io/statsig-forward-proxy?tab=readme-ov-file#deploying) to properly configure the deployment to meet your need.

### Configuration Reference

| Key                                   | Type    | Default                            | Description                                                                              |
| ------------------------------------- | ------- | ---------------------------------- | ---------------------------------------------------------------------------------------- |
| replicaCount                          | int     | `1`                                | Number of Statsig Forward Proxy replica pods to run                                      |
| image.repository                      | string  | `statsig/statsig-forward-proxy`    | Container image repository                                                              |
| image.pullPolicy                      | string  | `IfNotPresent`                     | Container image pull policy                                                             |
| image.tag                             | string  | `""`                               | Overrides the image tag. Defaults to chart appVersion if empty                           |
| imagePullSecrets                      | array   | `[]`                               | Image pull secrets for private image repositories                                        |
| nameOverride                          | string  | `""`                               | Overrides the name of the chart                                                         |
| fullnameOverride                      | string  | `""`                               | Overrides the full name of the resources                                                |
| serviceAccount.create                 | bool    | `true`                             | Specifies whether a service account should be created                                    |
| serviceAccount.automount              | bool    | `true`                             | Automount service account token                                                         |
| serviceAccount.annotations            | object  | `{}`                               | Annotations to add to the service account                                               |
| serviceAccount.name                   | string  | `""`                               | The name of the service account to use                                                  |
| pod.annotations                       | object  | `{}`                               | Annotations to add to the pod                                                           |
| pod.labels                            | object  | `{}`                               | Labels to add to the pod                                                                |
| pod.securityContext                   | object  | `{}`                               | Security context applied to the pod                                                     |
| pod.priorityClassName                 | string  | `""`                               | PriorityClassName to apply to the pod                                                   |
| pod.pdb.enabled                       | bool    | `false`                            | Deploy a PodDisruptionBudget for Statsig Forward Proxy                                  |
| pod.pdb.labels                        | object  | `{}`                               | Labels to be added to PodDisruptionBudget                                               |
| pod.pdb.annotations                   | object  | `{}`                               | Annotations to be added to PodDisruptionBudget                                          |
| pod.pdb.minAvailable                  | string  | `""`                               | Number of pods that are available after eviction (number or percentage)                  |
| pod.pdb.maxUnavailable                | string  | `""`                               | Number of pods that are unavailable after eviction (number or percentage)                |
| pod.topologySpreadConstraints         | array   | `[]`                               | Pod topology spread constraints                                                         |
| terminationGracePeriodSeconds         | int     | `60`                               | Grace period for pod termination in seconds                                             |
| securityContext                       | object  | `{}`                               | Container security context                                                              |
| service.type                          | string  | `ClusterIP`                        | Kubernetes service type                                                                 |
| service.annotations                   | object  | `{}`                               | Service annotations                                                                     |
| service.ports                         | array   | HTTP & gRPC ports                  | Service ports configuration                                                             |
| ingress.enabled                       | bool    | `false`                            | Enable ingress controller resource                                                      |
| ingress.className                     | string  | `""`                               | IngressClass that will be used to implement the Ingress                                 |
| ingress.annotations                   | object  | `{}`                               | Ingress annotations                                                                     |
| ingress.hosts                         | array   | `[{host: chart-example.local...}]` | Ingress accepted hostnames with paths                                                   |
| ingress.tls                           | array   | `[]`                               | Ingress TLS configuration                                                               |
| resources                             | object  | `{}`                               | CPU/Memory resource requests/limits                                                     |
| autoscaling.enabled                   | bool    | `false`                            | Enable Horizontal Pod Autoscaler                                                        |
| autoscaling.minReplicas               | int     | `1`                                | Minimum number of replicas                                                              |
| autoscaling.maxReplicas               | int     | `100`                              | Maximum number of replicas                                                              |
| autoscaling.targetCPUUtilizationPercentage | int | `80`                             | Target CPU utilization percentage                                                       |
| autoscaling.targetMemoryUtilizationPercentage | int | `80`                          | Target Memory utilization percentage                                                    |
| volumes                               | array   | `[]`                               | Additional volumes on the output deployment definition                                   |
| volumeMounts                          | array   | `[]`                               | Additional volumeMounts on the output deployment definition                              |
| nodeSelector                          | object  | `{}`                               | Node labels for pod assignment                                                          |
| tolerations                           | array   | `[]`                               | Tolerations for pod assignment                                                          |
| affinity                              | object  | `{}`                               | Affinity for pod assignment                                                             |
| sfp.environment                       | object  | `{}`                               | Environment variables for Statsig Forward Proxy (via ConfigMap). Avoid for secret values such as SDK keys or warm-up JSON |
| sfp.args                              | array   | `["http", "disabled"]`             | Container command-line arguments for Statsig Forward Proxy, including background polling/backoff knobs such as `--polling-interval-in-s`, `--max-in-flight`, `--background-poll-item-spacing-ms`, and `--id-list-file-refresh-observer-enabled` |
| sfp.environmentVariables              | array   | `[]`                               | Complex `valueFrom` style variable configurations for the deployment                     |
| sfp.secrets                           | array   | `[]`                               | Environment variables set from Kubernetes secrets. Preferred for `STATSIG_SERVER_SDK_KEY` and `SFP_STARTUP_WARMUP_KEYS_JSON` |
| sfp.envFromSecret                     | string  | `nil`                              | Name of a Kubernetes secret to set all environment variables from its key-value pairs    |
| sfp.livenessProbe                     | object  | HTTP check on `/v1/health` endpoint | Container liveness probe configuration                                                  |
| sfp.readinessProbe                    | object  | HTTP check on `/v1/ready` endpoint| Container readiness probe configuration                                                 |
| sfp.startupProbe                      | object  | HTTP check on `/v1/startup` endpoint  | Container startup probe configuration                                                   |
| sfp.lifecycle                         | array   | `[]`                               | Container lifecycle hooks                                                               |
