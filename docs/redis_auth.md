# Redis Authentication

## Overview

When running with the `redis` cache, the proxy authenticates to Redis in one of two modes,
selected by `REDIS_AUTH_MODE`:

| Mode | `REDIS_AUTH_MODE` | Credential |
| --- | --- | --- |
| Static password / ACL | `password` (default) | `REDIS_ENTERPRISE_USER` + `REDIS_ENTERPRISE_PASSWORD` |
| OAuth (Microsoft Entra ID) | `oauth` | Short-lived Entra access token used as the Redis password |

Both modes work alongside `REDIS_KEY_PREFIX`, and both connect over TLS when `REDIS_TLS=true`.
OAuth mode **requires** TLS, because in that mode the access token is the Redis password; see
[Transport encryption](#transport-encryption).

> **Note on TLS.** `REDIS_TLS=true` gives the *outbound* Redis connection server-authenticated TLS
> using the system trust store. The proxy does not currently present a client certificate to
> Redis, so it cannot do outbound mTLS to Redis on its own — if your Redis requires client
> certificates, terminate that outside the proxy (for example with a sidecar or service mesh).
> [TLS and mTLS Setup](tls_mtls_setup.md) covers something different: *inbound* client-to-proxy
> TLS/mTLS at nginx.

**OAuth is strictly opt-in.** With `REDIS_AUTH_MODE` unset or set to `password`, no Entra code
runs: no token requests, no background refresh task, and no change to connection pool behavior.

## Common settings

| Variable | Description |
| --- | --- |
| `REDIS_ENTERPRISE_HOST` | Redis host. Required. |
| `REDIS_ENTERPRISE_PORT` | Redis port. Required. |
| `REDIS_TLS` | Connect over TLS. Defaults to `false`. Required in OAuth mode. |
| `REDIS_KEY_PREFIX` | Namespace prepended to every key, e.g. `sfp-np`. A single `:` separator is normalized in. |
| `REDIS_CONNECTION_POOL_MAX_SIZE` | Max pooled connections. Defaults to `10`. |
| `REDIS_CONNECTION_POOL_MIN_SIZE` | Min idle connections. Defaults to `1`. See [the note below](#tuning-the-pool-for-oauth) for OAuth. |

## Static password / ACL mode (default)

```bash
export REDIS_AUTH_MODE=password   # or leave unset
export REDIS_ENTERPRISE_HOST=cache.example.com
export REDIS_ENTERPRISE_PORT=6380
export REDIS_TLS=true
export REDIS_ENTERPRISE_USER=sfp-user
export REDIS_ENTERPRISE_PASSWORD=<password>
```

`REDIS_ENTERPRISE_USER` and `REDIS_ENTERPRISE_PASSWORD` are both optional:

- Both set: sends `AUTH <user> <password>`.
- Password only: sends `AUTH <password>` as the default user.
- **User only:** sends `AUTH <user> ""`. This supports Redis Enterprise ACL users that have an
  empty password because the deployment relies on network-level controls instead. Leaving the
  password unset would otherwise cause the client to skip `AUTH` entirely
  ([redis-rs#1713](https://github.com/redis-rs/redis-rs/issues/1713)).
- Neither set: connects unauthenticated.

## OAuth (Microsoft Entra ID) mode

The proxy runs the Entra **client-credentials** flow and uses the resulting access token as the
Redis password. Only client secrets are supported; certificate and managed-identity credentials
are not.

```bash
export REDIS_AUTH_MODE=oauth
export REDIS_ENTERPRISE_HOST=cache.example.com
export REDIS_ENTERPRISE_PORT=6380
export REDIS_TLS=true
export REDIS_OAUTH_TENANT_ID=<tenant-guid>
export REDIS_OAUTH_CLIENT_ID=<app-registration-client-id>
export REDIS_OAUTH_CLIENT_SECRET=<client-secret>
export REDIS_OAUTH_SCOPE="https://<resource>.onmicrosoft.com/<app-id>/.default"
```

| Variable | Required | Description |
| --- | --- | --- |
| `REDIS_OAUTH_TENANT_ID` | Yes | Entra tenant ID. |
| `REDIS_OAUTH_CLIENT_ID` | Yes | App registration client ID. |
| `REDIS_OAUTH_CLIENT_SECRET` | Yes | Client secret. Inject from a Kubernetes Secret (`sfp.secrets` or `sfp.envFromSecret`), not a ConfigMap. |
| `REDIS_OAUTH_SCOPE` | Yes | The `.../.default` scope for the Redis app registration. |
| `REDIS_OAUTH_USERNAME` | No | Only for setups that authenticate as an object ID. When unset the proxy sends single-argument `AUTH <token>` as the default user. |
| `REDIS_OAUTH_REFRESH_MARGIN_IN_S` | No | How far ahead of expiry to refresh. Defaults to 25% of the token lifetime, i.e. refresh at ~45 minutes for a 1 hour token. Minimum 60; a smaller margin would mint the replacement token only after connections had already started being refused for being too close to expiry, and startup fails. A margin that does not fit the token lifetime (most easily reached by giving the value in milliseconds) is ignored in favor of the default, with a warning — see [How rotation works](#how-rotation-works). Also drives the pooled-connection age cap. |
| `REDIS_OAUTH_AUTHORITY_HOST` | No | Defaults to `https://login.microsoftonline.com`. Override for sovereign clouds, e.g. `https://login.microsoftonline.us`. Must be an `https://` URL — the client secret and the returned token are sent to this host — and startup fails otherwise. |
| `REDIS_OAUTH_ALLOW_PLAINTEXT` | No | Waives the `REDIS_TLS=true` requirement. Only for setups where another layer encrypts the hop. See [Transport encryption](#transport-encryption). |

If any required variable is missing, the process fails at startup and names every missing variable.

### Transport encryption

OAuth mode requires `REDIS_TLS=true` and fails at startup without it. In this mode the access token
is sent to Redis as the `AUTH` password, so an unencrypted connection puts a live Entra credential
on the wire in cleartext on every handshake — worse than the static-password mode, since the token
is a bearer credential for the scope it was issued against rather than for this cache alone. Azure
requires TLS for Entra-authenticated Redis, so this is not a restriction on any supported
deployment.

Set `REDIS_OAUTH_ALLOW_PLAINTEXT=true` only when something else encrypts the hop, such as a
TLS-terminating sidecar the proxy reaches over loopback. It waives the requirement and logs a
warning at startup; it never disables TLS that `REDIS_TLS=true` has already enabled.

The requirement is not applied to `password` mode, whose behavior is unchanged.

Note that the token request to Entra is separately protected: `REDIS_OAUTH_AUTHORITY_HOST` must be
`https://`, and redirects are not followed on that request, so the client secret cannot be replayed
to another host or downgraded to plain HTTP.

### How rotation works

Entra tokens are short-lived, and Azure de-authorizes a connection once the token it authenticated
with expires. The proxy therefore manages a token lifecycle rather than resolving the credential
once:

1. A background task mints a new token ahead of expiry and bumps a generation counter. If a
   refresh fails, the previous (still valid) token stays in use and the refresh retries with
   exponential backoff. A token endpoint reporting an unusable lifetime (under 120 seconds) is
   treated as a failed refresh rather than published, since such a token is too short-lived to
   both keep a refresh margin ahead of the checkout guard and leave the connection usable.
2. Every new connection authenticates with whichever token is current at connect time.
3. A connection whose token has been **superseded but is still valid** is handed to callers
   normally and retired when it is returned to the pool, so its replacement is established in the
   background. Rotation adds no latency to the request path.
4. A connection whose token has **actually expired** is refused at checkout and replaced, so an
   expired connection is never handed to a caller. This is the guarantee; steps 3 and 5 exist to
   keep the resulting reconnect off the request path.
5. Pooled connections are additionally age-capped so that ones sitting idle across a rotation are
   reaped and rebuilt in the background rather than going stale. The cap is derived from the
   refresh margin, because a connection opened just before a refresh inherits a token with only
   one margin of life left. It is a best-effort backstop: a connection that stays completely
   untouched across a rotation may still cost a single reconnect on its next checkout.

### Tuning the pool for OAuth

bb8 replenishes retired connections in the background only up to `REDIS_CONNECTION_POOL_MIN_SIZE`.
With the default of `1`, a rotation under load can push reconnects onto the request path. In OAuth
mode, set `REDIS_CONNECTION_POOL_MIN_SIZE` closer to your steady-state working set (for example,
half of `REDIS_CONNECTION_POOL_MAX_SIZE`). The proxy logs a warning at startup if the min size is
less than half the max size.

All Redis-backed caches in the process share a single token provider, so there is one refresh loop
and one token in flight regardless of how many caches are configured.

Because every refresh retires the pooled connections opened against the previous token, the refresh
cadence is also the pool's churn rate. That is why a `REDIS_OAUTH_REFRESH_MARGIN_IN_S` too large for
the token lifetime is ignored rather than capped: honoring it would refresh every 30 seconds and
rebuild the pool just as often.

### Failure behavior

The initial token fetch retries with exponential backoff for up to `--redis-connection-timeout-in-s`,
so a transient 429 or DNS blip during a rolling deploy does not permanently disable caching for
that pod. This mirrors the Redis pool, which retries for the same window rather than giving up on
the first refused connection. The budget is spent once per process, not once per cache: the proxy
builds two caches and they share a single token provider, so both observe the same startup outcome
and neither can end up with a datastore the other lacks.

If the budget is exhausted, the proxy logs the error and continues **without a datastore** rather
than crashing — the same posture it already takes when the Redis pool fails to build. Cache reads
miss and cache writes are skipped until the process is restarted with working credentials.

Once running, a failed refresh is not fatal: the previous token keeps authenticating new
connections until it expires while the refresh retries in the background.
