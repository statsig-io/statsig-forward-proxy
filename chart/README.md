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
| sfp.environment                       | object  | `{}`                               | Environment variables for Statsig Forward Proxy (via ConfigMap)                         |
| sfp.args                              | array   | `["http", "disabled"]`             | Container command-line arguments for Statsig Forward Proxy                              |
| sfp.environmentVariables              | array   | `[]`                               | Complex `valueFrom` style variable configurations for the deployment                     |
| sfp.secrets                           | array   | `[]`                               | Environment variables set from Kubernetes secrets                                        |
| sfp.envFromSecret                     | string  | `nil`                              | Name of a Kubernetes secret to set all environment variables from its key-value pairs    |
| sfp.livenessProbe                     | object  | HTTP check on `/v1/health` endpoint | Container liveness probe configuration                                                  |
| sfp.readinessProbe                    | object  | HTTP check on `/v1/ready` endpoint| Container readiness probe configuration                                                 |
| sfp.startupProbe                      | object  | HTTP check on `/v1/startup` endpoint  | Container startup probe configuration                                                   |
| sfp.lifecycle                         | array   | `[]`                               | Container lifecycle hooks                                                               |
