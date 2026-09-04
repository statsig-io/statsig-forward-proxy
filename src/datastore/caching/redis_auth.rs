//! Redis credential sources and the bb8 connection manager that consumes them.
//!
//! Two auth modes are supported:
//!
//! * [`RedisCredentials::Static`] — today's behavior. A username/password pair resolved once at
//!   startup (including the empty-password ACL case) and reused for every connection.
//! * [`RedisCredentials::Oauth`] — an opt-in Microsoft Entra ID (Azure AD) client-credentials
//!   flow where a short-lived access token is used as the Redis password.
//!
//! Because Entra tokens expire (and Azure de-authorizes connections whose token has expired),
//! OAuth mode needs a token lifecycle rather than a one-shot credential. That is what
//! [`EntraTokenProvider`] provides: a background task refreshes the token ahead of expiry and
//! bumps a monotonic generation counter, and [`StatsigRedisConnectionManager`] reads the
//! *current* token every time it opens a connection.

use std::{
    fmt,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::sync::OnceCell;

use arc_swap::ArcSwap;
use redis::{
    aio::{ConnectionLike, MultiplexedConnection},
    Client, Cmd, ConnectionAddr, ConnectionInfo, ErrorKind, Pipeline, ProtocolVersion,
    RedisConnectionInfo, RedisError, RedisFuture, Value,
};
use serde::Deserialize;

use crate::datastore::data_providers::OUTBOUND_USER_AGENT;

/// Fraction of a token's lifetime used as the default refresh margin, i.e. refresh at ~75% of
/// the lifetime (~45 minutes for a standard 1 hour Entra token). This is deliberately more
/// conservative than a two minute margin so there is room for token-endpoint latency, retries,
/// and rolling the pool's connections over to the new token.
const DEFAULT_REFRESH_MARGIN_DIVISOR: u32 = 4;

/// Lower bound on how long the refresh task sleeps between refreshes, so a misconfigured margin
/// (or an unexpectedly short token) cannot turn into a hot loop against the token endpoint.
const MIN_REFRESH_DELAY: Duration = Duration::from_secs(30);

const MIN_RETRY_BACKOFF: Duration = Duration::from_secs(1);
const MAX_RETRY_BACKOFF: Duration = Duration::from_secs(60);

/// Shortest token lifetime we will accept. Anything below this is unusable anyway — every
/// connection opened with it would be refused at checkout by the hard-expiry guard — and
/// accepting it would drive the refresh loop at the token endpoint with no delay between
/// attempts. Treated as a malformed response so the existing retry backoff applies instead.
///
/// The value is not arbitrary: it is the shortest lifetime for which [`refresh_margin`] can still
/// return at least [`MIN_REFRESH_MARGIN`], since the margin is also capped at
/// `lifetime - MIN_REFRESH_DELAY`. Lowering this without lowering the margin floor would
/// reintroduce lifetimes whose refresh fires after the checkout guard has already engaged.
const MIN_USABLE_TOKEN_LIFETIME: Duration = Duration::from_secs(120);

/// Ceiling on the startup retry budget, which comes from `--redis-connection-timeout-in-s`.
/// `Instant + Duration` panics on overflow, so an operator passing something enormous to mean
/// "never give up" would crash the process in OAuth mode while password mode started fine. A day
/// is already indistinguishable from forever for a startup path.
const MAX_STARTUP_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

/// Longest token lifetime we will honor. Entra issues ~1 hour tokens, so this is really a guard
/// on `Instant` arithmetic: `now + lifetime` panics on overflow, and `expires_in` is untrusted
/// input. Refreshing more often than a very long-lived token strictly requires is harmless.
const MAX_TOKEN_LIFETIME: Duration = Duration::from_secs(24 * 60 * 60);

/// Smallest refresh margin, whether configured or derived from the lifetime. Must stay
/// comfortably above [`TOKEN_HARD_EXPIRY_MARGIN`] plus the time a token request itself can take:
/// a margin at or below that would mint the replacement token only once the checkout guard has
/// already started refusing connections, turning the tail of every token lifetime into a
/// recurring outage window.
pub const MIN_REFRESH_MARGIN: Duration = Duration::from_secs(60);

/// A connection is refused at checkout once its token has this little life left. This is the
/// backstop for a connection that went idle right after a refresh: it never checks back in, so
/// [`bb8::ManageConnection::has_broken`] never sees it, and it must not be handed out after its
/// token actually dies.
const TOKEN_HARD_EXPIRY_MARGIN: Duration = Duration::from_secs(30);

/// Fraction of the refresh margin used as the pooled-connection age cap. See
/// [`max_connection_lifetime`].
const CONNECTION_LIFETIME_MARGIN_NUMERATOR: u32 = 3;
const CONNECTION_LIFETIME_MARGIN_DENOMINATOR: u32 = 4;

/// Floor on the connection age cap. An unusually small configured refresh margin should not churn
/// the whole pool every couple of minutes; past this point we accept the (correctness-preserving)
/// checkout-time reconnect instead.
const MIN_CONNECTION_LIFETIME: Duration = Duration::from_secs(5 * 60);

const TOKEN_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Cap on how much of a failed token-endpoint response body we echo into logs.
const MAX_LOGGED_ERROR_BODY_LEN: usize = 512;

pub const DEFAULT_ENTRA_AUTHORITY_HOST: &str = "https://login.microsoftonline.com";

/// Everything needed to run the Entra client-credentials flow. `client_secret` is deliberately
/// kept out of any `Debug` output.
#[derive(Clone)]
pub struct EntraOauthConfig {
    pub tenant_id: String,
    pub client_id: String,
    pub client_secret: String,
    /// The app-registration scope, e.g. `https://<resource>/<app-id>/.default`.
    pub scope: String,
    /// Only needed by Entra setups that authenticate as an object ID rather than the default
    /// user. When `None` we send single-argument `AUTH <token>`.
    pub username: Option<String>,
    /// Overrides the default refresh margin (75% of the token lifetime).
    pub refresh_margin: Option<Duration>,
    /// Overridable for sovereign clouds and for tests.
    pub authority_host: String,
}

impl fmt::Debug for EntraOauthConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EntraOauthConfig")
            .field("tenant_id", &self.tenant_id)
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .field("scope", &self.scope)
            .field("username", &self.username)
            .field("refresh_margin", &self.refresh_margin)
            .field("authority_host", &self.authority_host)
            .finish()
    }
}

#[derive(Debug)]
pub enum TokenError {
    Transport(String),
    /// The token endpoint answered with a non-2xx status.
    Endpoint {
        status: u16,
        body: String,
    },
    Malformed(String),
}

impl fmt::Display for TokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TokenError::Transport(e) => write!(f, "failed to reach the Entra token endpoint: {e}"),
            TokenError::Endpoint { status, body } => {
                write!(f, "Entra token endpoint returned {status}: {body}")
            }
            TokenError::Malformed(e) => write!(f, "malformed Entra token response: {e}"),
        }
    }
}

impl std::error::Error for TokenError {}

/// An immutable view of the token currently in use, published atomically on every refresh.
pub struct TokenSnapshot {
    secret: Arc<str>,
    expires_at: Instant,
    /// When the background task should mint the next token.
    refresh_at: Instant,
    /// Bumped on every successful refresh so connections can tell whether they were opened with
    /// a token that has since been superseded.
    generation: u64,
    /// The bb8 `max_lifetime` derived from this token's lifetime.
    max_connection_lifetime: Duration,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: u64,
}

pub struct EntraTokenProvider {
    config: EntraOauthConfig,
    http_client: reqwest::Client,
    token: ArcSwap<TokenSnapshot>,
    generation: AtomicU64,
    refresh_task_started: AtomicBool,
}

/// Every `RedisCache` in the process shares one provider, so a single refresh loop serves all of
/// them. Without this each cache would poll Entra on its own schedule and track its own
/// generation, doubling token traffic and allowing a startup where one cache has a token and
/// another does not.
static SHARED_ENTRA_PROVIDER: OnceCell<Result<Arc<EntraTokenProvider>, String>> =
    OnceCell::const_new();

/// Returns the process-wide provider, performing the initial fetch on first call. Callers pass
/// the same config (both read it from the same environment), so whichever call arrives first wins.
///
/// The *outcome* is cached, failure included, which is why this holds a `Result` rather than using
/// `get_or_try_init` (that would discard an error and re-run the initializer). `server.rs` builds
/// two caches in sequence, so retrying per caller would mean an Entra outage blocks startup for
/// twice `--redis-connection-timeout-in-s`, and worse, the second attempt could succeed where the
/// first failed and leave one cache with a datastore and one without. Both callers now observe the
/// same result, within one budget. Recovery is a restart, which is already the documented posture
/// for exhausting the startup budget.
pub async fn shared_entra_provider(
    config: EntraOauthConfig,
    startup_timeout: Duration,
) -> Result<Arc<EntraTokenProvider>, String> {
    SHARED_ENTRA_PROVIDER
        .get_or_init(|| async {
            EntraTokenProvider::connect(config, startup_timeout)
                .await
                .map_err(|error| error.to_string())
        })
        .await
        .clone()
}

impl EntraTokenProvider {
    /// Performs the initial token fetch, retrying with backoff until `startup_timeout` elapses.
    ///
    /// The retry matters because the alternative is a pod that stays cacheless until it is
    /// restarted: a single 429 or DNS blip during a rolling deploy should not be permanent. This
    /// mirrors the Redis pool, which retries for its whole connection-timeout window rather than
    /// giving up on the first refused connection.
    ///
    /// Returns an error rather than panicking once the budget is spent, so the caller can degrade
    /// to running without a datastore exactly as a failed pool build does.
    pub async fn connect(
        config: EntraOauthConfig,
        startup_timeout: Duration,
    ) -> Result<Arc<Self>, TokenError> {
        let http_client = reqwest::Client::builder()
            .user_agent(OUTBOUND_USER_AGENT)
            .timeout(TOKEN_REQUEST_TIMEOUT)
            // The token request carries `client_secret` in its body, and reqwest both follows
            // redirects by default and replays the body across a 307/308. The header-stripping it
            // does on cross-host redirects does not help here, because the secret is not in a
            // header. A redirect could therefore hand the secret to another host, over plain HTTP,
            // sidestepping the https check applied to the configured authority host. A token
            // endpoint has no reason to redirect, so refuse rather than validate every hop.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| TokenError::Transport(e.to_string()))?;

        let provider = Arc::new(EntraTokenProvider {
            config,
            http_client,
            // Replaced below before the provider is handed out; only the very first fetch sees it.
            token: ArcSwap::from_pointee(TokenSnapshot {
                secret: Arc::from(""),
                expires_at: Instant::now(),
                refresh_at: Instant::now(),
                generation: 0,
                max_connection_lifetime: Duration::from_secs(0),
            }),
            generation: AtomicU64::new(0),
            refresh_task_started: AtomicBool::new(false),
        });

        let deadline = Instant::now() + startup_timeout.min(MAX_STARTUP_TIMEOUT);
        let mut backoff = MIN_RETRY_BACKOFF;
        loop {
            let error = match provider.refresh().await {
                Ok(()) => return Ok(provider),
                Err(error) => error,
            };
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(error);
            }
            eprintln!(
                "Failed to acquire the initial Entra token for redis, retrying in {backoff:?}: {error}"
            );
            tokio::time::sleep(backoff.min(remaining)).await;
            backoff = (backoff * 2).min(MAX_RETRY_BACKOFF);
        }
    }

    /// Starts the background refresh loop, at most once. Repeat calls are ignored so that sharing
    /// one provider across several caches cannot end up with several loops racing to mint tokens.
    pub fn spawn_refresh_task(self: &Arc<Self>) {
        if self.refresh_task_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let provider = Arc::clone(self);
        tokio::spawn(async move { provider.refresh_loop().await });
    }

    pub fn current(&self) -> Arc<TokenSnapshot> {
        self.token.load_full()
    }

    pub fn current_generation(&self) -> u64 {
        self.token.load().generation
    }

    pub fn max_connection_lifetime(&self) -> Duration {
        self.token.load().max_connection_lifetime
    }

    fn username(&self) -> Option<String> {
        self.config.username.clone()
    }

    async fn refresh_loop(self: Arc<Self>) {
        loop {
            let refresh_at = self.token.load().refresh_at;
            tokio::time::sleep(refresh_at.saturating_duration_since(Instant::now())).await;

            let mut backoff = MIN_RETRY_BACKOFF;
            while let Err(e) = self.refresh().await {
                // The previous token is still published and, until it expires, still authenticates
                // new connections; keep serving with it while we retry.
                eprintln!("Failed to refresh Entra token for Redis, retrying in {backoff:?}: {e}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_RETRY_BACKOFF);
            }
        }
    }

    async fn refresh(&self) -> Result<(), TokenError> {
        let response = self.request_token().await?;
        let lifetime = usable_token_lifetime(response.expires_in)?;
        warn_if_configured_margin_does_not_fit(lifetime, self.config.refresh_margin);
        let now = Instant::now();
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;

        self.token.store(Arc::new(TokenSnapshot {
            secret: Arc::from(response.access_token.as_str()),
            expires_at: now + lifetime,
            refresh_at: now + refresh_delay(lifetime, self.config.refresh_margin),
            generation,
            max_connection_lifetime: max_connection_lifetime(lifetime, self.config.refresh_margin),
        }));
        Ok(())
    }

    async fn request_token(&self) -> Result<TokenResponse, TokenError> {
        let url = format!(
            "{}/{}/oauth2/v2.0/token",
            self.config.authority_host.trim_end_matches('/'),
            self.config.tenant_id
        );
        let form = [
            ("grant_type", "client_credentials"),
            ("client_id", self.config.client_id.as_str()),
            ("client_secret", self.config.client_secret.as_str()),
            ("scope", self.config.scope.as_str()),
        ];

        let response = self
            .http_client
            .post(url)
            .form(&form)
            .send()
            .await
            .map_err(|e| TokenError::Transport(e.to_string()))?;

        let status = response.status();
        // Read before the body consumes the response. Since redirects are not followed, a 3xx
        // lands here as an ordinary failure, and "returned 302" with an empty body is a puzzling
        // thing to find in the logs of a pod that will not start.
        let redirect_location = status.is_redirection().then(|| {
            response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .map(truncate_for_log)
                .unwrap_or_else(|| "an unspecified location".to_string())
        });
        let body = response
            .text()
            .await
            .map_err(|e| TokenError::Transport(e.to_string()))?;

        if let Some(location) = redirect_location {
            return Err(TokenError::Endpoint {
                status: status.as_u16(),
                body: format!(
                    "refused to follow a redirect to {location}; this request carries \
                     client_secret, so it is only ever sent to the configured authority host"
                ),
            });
        }
        if !status.is_success() {
            return Err(TokenError::Endpoint {
                status: status.as_u16(),
                body: truncate_for_log(&body),
            });
        }
        let response = serde_json::from_str::<TokenResponse>(&body)
            .map_err(|e| TokenError::Malformed(e.to_string()))?;

        // A blank token parses fine but authenticates nothing, and publishing it is worse than
        // failing: the initial fetch would report success, so the retry loop that exists for
        // exactly this stops, the pool build then fails against Redis, and because the pool never
        // came up the refresh task is never started. The empty token would be cached in the
        // process-wide provider with nothing left running to replace it, leaving every cache
        // datastore-less until the pod restarts. Treating it as malformed keeps it on the
        // retry paths instead.
        if response.access_token.trim().is_empty() {
            return Err(TokenError::Malformed(
                "token endpoint returned an empty access_token".to_string(),
            ));
        }
        Ok(response)
    }
}

/// Validates the `expires_in` a token endpoint reported and converts it to a lifetime we can do
/// arithmetic on. Rejecting the low end keeps the refresh loop from spinning on a token that is
/// dead on arrival; clamping the high end keeps `now + lifetime` from overflowing.
///
/// The upper bound only ever shortens the lifetime we assume, which is the safe direction. The
/// lower bound deliberately rejects rather than rounding up, since treating a token as longer
/// lived than it is would hand out connections that Redis has already de-authorized.
fn usable_token_lifetime(expires_in: u64) -> Result<Duration, TokenError> {
    let lifetime = Duration::from_secs(expires_in);
    if lifetime < MIN_USABLE_TOKEN_LIFETIME {
        return Err(TokenError::Malformed(format!(
            "token lifetime of {expires_in}s is below the {}s minimum",
            MIN_USABLE_TOKEN_LIFETIME.as_secs()
        )));
    }
    Ok(lifetime.min(MAX_TOKEN_LIFETIME))
}

/// How far ahead of expiry the next token is minted. Defaults to a quarter of the lifetime, and
/// is capped so an oversized configured margin cannot schedule back-to-back refreshes.
///
/// The [`MIN_REFRESH_MARGIN`] floor applies to the derived margin too, not just a configured one:
/// a quarter of a short lifetime can land at or below [`TOKEN_HARD_EXPIRY_MARGIN`], which would
/// schedule the refresh only after checkouts had already started being refused. `RedisCache`
/// rejects a too-small configured margin at startup so operators get told rather than silently
/// overridden; this floor is what keeps the invariant true for every other path into the
/// function, including a directly constructed [`EntraOauthConfig`].
fn refresh_margin(lifetime: Duration, configured_margin: Option<Duration>) -> Duration {
    let margin = configured_margin
        .filter(|margin| configured_margin_fits(lifetime, *margin))
        .unwrap_or(lifetime / DEFAULT_REFRESH_MARGIN_DIVISOR)
        .max(MIN_REFRESH_MARGIN);
    let shortest_delay = MIN_REFRESH_DELAY.min(lifetime / 2);
    margin.min(lifetime.saturating_sub(shortest_delay))
}

/// Whether a configured margin leaves a sane gap before the next mint.
///
/// A margin at or beyond the lifetime is a misconfiguration — most easily reached by giving the
/// value in milliseconds — and it is the damaging direction. Merely capping it, which is what this
/// used to do, leaves the refresh firing every [`MIN_REFRESH_DELAY`]: thousands of token requests
/// a day per pod, and since every refresh bumps the generation, the whole connection pool retired
/// and rebuilt on that same cadence. Falling back to the default margin instead keeps an absurd
/// value from turning into pathological behavior, and the caller warns so it is not silent.
fn configured_margin_fits(lifetime: Duration, configured_margin: Duration) -> bool {
    configured_margin <= lifetime.saturating_sub(MIN_REFRESH_DELAY.min(lifetime / 2))
}

fn warn_if_configured_margin_does_not_fit(lifetime: Duration, configured_margin: Option<Duration>) {
    let Some(configured) = configured_margin else {
        return;
    };
    if configured_margin_fits(lifetime, configured) {
        return;
    }
    eprintln!(
        "REDIS_OAUTH_REFRESH_MARGIN_IN_S={} does not fit the {}s token lifetime the endpoint \
         reported, so the {}s default is being used instead. Honoring it would mint a replacement \
         token every {}s and retire the whole connection pool on each rotation.",
        configured.as_secs(),
        lifetime.as_secs(),
        refresh_margin(lifetime, None).as_secs(),
        MIN_REFRESH_DELAY.as_secs(),
    );
}

/// How long to wait before minting the next token, i.e. refresh at ~75% of the lifetime by
/// default (~45 minutes for a 1 hour token).
fn refresh_delay(lifetime: Duration, configured_margin: Option<Duration>) -> Duration {
    lifetime.saturating_sub(refresh_margin(lifetime, configured_margin))
}

/// Ceiling on pooled-connection age, used as bb8's `max_lifetime`.
///
/// This has to be derived from the refresh margin rather than from the token lifetime, because
/// bb8 measures a connection's age from when it was *created*, not from when its token was
/// issued. A connection opened just before a refresh inherits a token with only one margin of
/// life left, so a cap based on the lifetime (say, half of it) would let that connection sit idle
/// past its token's expiry. One margin is the shortest remaining token life any new connection
/// can inherit, so reaping at a fraction of that keeps idle connections ahead of expiry.
///
/// This is a best-effort backstop, not the correctness guarantee: it is fixed at pool build (bb8
/// cannot change `max_lifetime` afterwards) and is floored to avoid churn, so
/// [`is_token_expiring`] at checkout remains what actually prevents an expired connection from
/// being served.
fn max_connection_lifetime(lifetime: Duration, configured_margin: Option<Duration>) -> Duration {
    let target = refresh_margin(lifetime, configured_margin) * CONNECTION_LIFETIME_MARGIN_NUMERATOR
        / CONNECTION_LIFETIME_MARGIN_DENOMINATOR;
    // bb8's builder panics on a zero `max_lifetime`, so the ceiling can never reach zero either.
    let ceiling = (lifetime / 2).max(Duration::from_secs(1));
    target.max(MIN_CONNECTION_LIFETIME).min(ceiling)
}

/// True once `expires_at` is within `margin` of `now` (or already past).
fn is_token_expiring(now: Instant, expires_at: Instant, margin: Duration) -> bool {
    expires_at
        .checked_duration_since(now)
        .is_none_or(|remaining| remaining <= margin)
}

/// True when a connection opened under `connection_generation` should be retired because a newer
/// token has since been published. Always false in static mode, where generations are inert.
fn should_retire_connection(credentials: &RedisCredentials, connection_generation: u64) -> bool {
    match credentials {
        RedisCredentials::Oauth(provider) => connection_generation != provider.current_generation(),
        RedisCredentials::Static { .. } => false,
    }
}

fn truncate_for_log(body: &str) -> String {
    match body.char_indices().nth(MAX_LOGGED_ERROR_BODY_LEN) {
        Some((idx, _)) => format!("{}...", &body[..idx]),
        None => body.to_string(),
    }
}

/// Where [`StatsigRedisConnectionManager`] gets the credentials for each new connection.
#[derive(Clone)]
pub enum RedisCredentials {
    Static {
        username: Option<String>,
        password: Option<String>,
    },
    Oauth(Arc<EntraTokenProvider>),
}

/// A Redis connection tagged with the token it authenticated with, so the pool can retire it once
/// that token is superseded or expired. In static mode both fields are inert.
pub struct TrackedConnection {
    inner: MultiplexedConnection,
    generation: u64,
    token_expires_at: Option<Instant>,
}

impl ConnectionLike for TrackedConnection {
    fn req_packed_command<'a>(&'a mut self, cmd: &'a Cmd) -> RedisFuture<'a, Value> {
        self.inner.req_packed_command(cmd)
    }

    fn req_packed_commands<'a>(
        &'a mut self,
        cmd: &'a Pipeline,
        offset: usize,
        count: usize,
    ) -> RedisFuture<'a, Vec<Value>> {
        self.inner.req_packed_commands(cmd, offset, count)
    }

    fn get_db(&self) -> i64 {
        self.inner.get_db()
    }
}

/// Stands in for `bb8_redis::RedisConnectionManager`, which snapshots its `ConnectionInfo` at
/// construction. This one rebuilds it per connection so OAuth mode picks up the current token.
pub struct StatsigRedisConnectionManager {
    addr: ConnectionAddr,
    credentials: RedisCredentials,
}

impl StatsigRedisConnectionManager {
    pub fn new(addr: ConnectionAddr, credentials: RedisCredentials) -> Self {
        StatsigRedisConnectionManager { addr, credentials }
    }
}

impl bb8::ManageConnection for StatsigRedisConnectionManager {
    type Connection = TrackedConnection;
    type Error = RedisError;

    async fn connect(&self) -> Result<Self::Connection, Self::Error> {
        let (username, password, generation, token_expires_at) = match &self.credentials {
            RedisCredentials::Static { username, password } => {
                (username.clone(), password.clone(), 0, None)
            }
            RedisCredentials::Oauth(provider) => {
                // Read-only: the background task is the only thing that mints tokens, so
                // concurrent connects cannot stampede the Entra endpoint.
                let token = provider.current();
                (
                    provider.username(),
                    Some(token.secret.to_string()),
                    token.generation,
                    Some(token.expires_at),
                )
            }
        };

        let connection_info = ConnectionInfo {
            addr: self.addr.clone(),
            redis: RedisConnectionInfo {
                db: 0,
                username,
                password,
                protocol: ProtocolVersion::RESP2,
            },
        };
        let inner = Client::open(connection_info)?
            .get_multiplexed_async_connection()
            .await?;

        Ok(TrackedConnection {
            inner,
            generation,
            token_expires_at,
        })
    }

    async fn is_valid(&self, conn: &mut Self::Connection) -> Result<(), Self::Error> {
        // A merely superseded token still authenticates, so only reject connections whose token
        // is actually dead. Those that are just stale are retired by `has_broken` at check-in,
        // which keeps the reconnect off the request path.
        if let Some(expires_at) = conn.token_expires_at {
            if is_token_expiring(Instant::now(), expires_at, TOKEN_HARD_EXPIRY_MARGIN) {
                return Err(RedisError::from((
                    ErrorKind::AuthenticationFailed,
                    "Redis connection's Entra token has expired",
                )));
            }
        }
        redis::cmd("PING").query_async(&mut conn.inner).await
    }

    fn has_broken(&self, conn: &mut Self::Connection) -> bool {
        // bb8 calls this on check-in. Returning true drops the connection and spawns a background
        // task to replenish it, which reconnects with the current token without the caller ever
        // waiting on a handshake.
        should_retire_connection(&self.credentials, conn.generation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datastore::caching::fake_redis::FakeRedis;
    use bb8::ManageConnection;
    use redis::AsyncCommands;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    const HOUR: Duration = Duration::from_secs(3600);

    #[test]
    fn refresh_defaults_to_three_quarters_of_lifetime() {
        assert_eq!(refresh_delay(HOUR, None), Duration::from_secs(2700));
    }

    #[test]
    fn refresh_honors_configured_margin() {
        assert_eq!(
            refresh_delay(HOUR, Some(Duration::from_secs(120))),
            Duration::from_secs(3480)
        );
    }

    #[test]
    fn a_margin_larger_than_the_lifetime_falls_back_to_the_default() {
        // Capping it instead would refresh every MIN_REFRESH_DELAY, churning the whole pool on
        // that cadence, so an unusable value has to degrade to the default rather than to the
        // floor. The millisecond mix-up is the realistic way to get here.
        for configured in [
            HOUR,
            Duration::from_secs(7200),
            Duration::from_secs(3_600_000),
        ] {
            assert_eq!(
                refresh_delay(HOUR, Some(configured)),
                refresh_delay(HOUR, None),
                "a {configured:?} margin should behave as if unset"
            );
        }
    }

    #[test]
    fn a_margin_that_fits_is_still_honored_exactly() {
        // The fallback must not swallow legitimate values, including the largest usable one.
        let largest_usable = HOUR - MIN_REFRESH_DELAY;
        assert_eq!(
            refresh_delay(HOUR, Some(largest_usable)),
            MIN_REFRESH_DELAY,
            "the boundary value should still be honored"
        );
        assert_eq!(
            refresh_delay(HOUR, Some(Duration::from_secs(600))),
            HOUR - Duration::from_secs(600)
        );
    }

    #[test]
    fn refresh_floor_never_exceeds_short_lifetimes() {
        // The 30s floor must not push the refresh past a 10s token's expiry.
        let lifetime = Duration::from_secs(10);
        let delay = refresh_delay(lifetime, Some(Duration::from_secs(60)));
        assert!(delay < lifetime, "expected {delay:?} < {lifetime:?}");
    }

    #[test]
    fn max_connection_lifetime_stays_under_the_refresh_margin() {
        // A connection opened just before a refresh inherits only one margin of token life, so
        // the cap has to be under the margin (15 min here) rather than under the lifetime.
        let cap = max_connection_lifetime(HOUR, None);
        assert_eq!(cap, Duration::from_secs(675));
        assert!(cap < refresh_margin(HOUR, None), "{cap:?}");
    }

    #[test]
    fn max_connection_lifetime_is_floored_for_small_margins() {
        // A 2 minute margin must not recycle the entire pool every 90 seconds.
        let cap = max_connection_lifetime(HOUR, Some(Duration::from_secs(120)));
        assert_eq!(cap, MIN_CONNECTION_LIFETIME);
    }

    #[test]
    fn max_connection_lifetime_never_exceeds_half_the_lifetime() {
        // The floor must not push the cap above the token lifetime for short-lived tokens.
        let lifetime = Duration::from_secs(60);
        assert_eq!(
            max_connection_lifetime(lifetime, None),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn max_connection_lifetime_is_never_zero() {
        // bb8's builder panics on a zero max_lifetime.
        assert_eq!(
            max_connection_lifetime(Duration::from_secs(0), None),
            Duration::from_secs(1)
        );
    }

    #[test]
    fn a_normal_token_lifetime_is_accepted_unchanged() {
        assert_eq!(usable_token_lifetime(3600).expect("accepted"), HOUR);
    }

    #[test]
    fn a_zero_lifetime_token_is_rejected() {
        // Accepting it would make refresh_delay zero and spin the refresh loop against the token
        // endpoint with no delay between attempts.
        let error = usable_token_lifetime(0).expect_err("expected an error");
        assert!(matches!(error, TokenError::Malformed(_)), "{error:?}");
        assert!(refresh_delay(HOUR, None) >= MIN_REFRESH_DELAY);
    }

    #[test]
    fn a_token_below_the_usable_floor_is_rejected() {
        assert!(usable_token_lifetime(119).is_err());
        assert!(usable_token_lifetime(120).is_ok());
    }

    #[test]
    fn an_absurd_lifetime_is_clamped_rather_than_overflowing() {
        // `now + lifetime` panics on overflow, and expires_in is untrusted input.
        let lifetime = usable_token_lifetime(u64::MAX).expect("should clamp rather than reject");
        assert_eq!(lifetime, MAX_TOKEN_LIFETIME);
        let _ = Instant::now() + lifetime;
    }

    #[test]
    fn every_accepted_lifetime_yields_a_nonzero_refresh_delay() {
        // The property that keeps the refresh loop from hot-looping, checked across the whole
        // accepted range rather than at a single point.
        for expires_in in [120, 121, 300, 3600, 86_400, u64::MAX] {
            let lifetime = usable_token_lifetime(expires_in).expect("should be accepted");
            let delay = refresh_delay(lifetime, None);
            assert!(
                delay >= MIN_REFRESH_DELAY,
                "expires_in={expires_in} gave {delay:?}"
            );
            let margin_delay = refresh_delay(lifetime, Some(MIN_REFRESH_MARGIN));
            assert!(
                margin_delay >= MIN_REFRESH_DELAY,
                "expires_in={expires_in} with the smallest margin gave {margin_delay:?}"
            );
        }
    }

    #[test]
    fn the_smallest_allowed_margin_stays_ahead_of_the_checkout_guard() {
        // Otherwise the replacement token is minted only after connections start being refused.
        assert!(MIN_REFRESH_MARGIN > TOKEN_HARD_EXPIRY_MARGIN);
    }

    #[test]
    fn no_accepted_lifetime_refreshes_after_the_checkout_guard_engages() {
        // The invariant the whole rotation scheme rests on: the replacement token is always
        // minted while the current one can still serve checkouts. A margin at or below the hard
        // expiry margin opens a recurring outage in the tail of every token lifetime, so check
        // the derived margin too, not just configured ones — a quarter of a short lifetime is
        // small enough to fall through on its own.
        let margins = [
            None,
            Some(Duration::ZERO),
            Some(Duration::from_secs(1)),
            Some(TOKEN_HARD_EXPIRY_MARGIN),
            Some(MIN_REFRESH_MARGIN),
            Some(Duration::from_secs(600)),
            Some(Duration::from_secs(86_400)),
        ];
        for expires_in in [120, 121, 180, 300, 3600, 86_400, u64::MAX] {
            let lifetime = usable_token_lifetime(expires_in).expect("should be accepted");
            for margin in margins {
                let resolved = refresh_margin(lifetime, margin);
                assert!(
                    resolved > TOKEN_HARD_EXPIRY_MARGIN,
                    "expires_in={expires_in} margin={margin:?} resolved to {resolved:?}"
                );
                let delay = refresh_delay(lifetime, margin);
                assert!(
                    delay >= MIN_REFRESH_DELAY,
                    "expires_in={expires_in} margin={margin:?} gave delay {delay:?}"
                );
            }
        }
    }

    #[test]
    fn token_with_ample_life_is_not_expiring() {
        let now = Instant::now();
        assert!(!is_token_expiring(
            now,
            now + Duration::from_secs(600),
            TOKEN_HARD_EXPIRY_MARGIN
        ));
    }

    #[test]
    fn token_inside_margin_is_expiring() {
        let now = Instant::now();
        assert!(is_token_expiring(
            now,
            now + Duration::from_secs(5),
            TOKEN_HARD_EXPIRY_MARGIN
        ));
    }

    #[test]
    fn already_expired_token_is_expiring() {
        let now = Instant::now();
        assert!(is_token_expiring(
            now,
            now - Duration::from_secs(1),
            TOKEN_HARD_EXPIRY_MARGIN
        ));
    }

    /// Connects with no retry budget, so tests that queue a single failing response stay
    /// deterministic instead of consuming the next queued response on a retry.
    async fn connect_provider(
        config: EntraOauthConfig,
    ) -> Result<Arc<EntraTokenProvider>, TokenError> {
        EntraTokenProvider::connect(config, Duration::ZERO).await
    }

    fn config_for(authority_host: &str) -> EntraOauthConfig {
        EntraOauthConfig {
            tenant_id: "tenant".to_string(),
            client_id: "client".to_string(),
            client_secret: "secret".to_string(),
            scope: "https://example.com/app/.default".to_string(),
            username: None,
            refresh_margin: None,
            authority_host: authority_host.to_string(),
        }
    }

    /// Builds a provider holding a pre-seeded token so the generation logic can be exercised
    /// without touching the network.
    fn provider_at_generation(generation: u64) -> Arc<EntraTokenProvider> {
        let now = Instant::now();
        Arc::new(EntraTokenProvider {
            config: config_for(DEFAULT_ENTRA_AUTHORITY_HOST),
            http_client: reqwest::Client::new(),
            token: ArcSwap::from_pointee(TokenSnapshot {
                secret: Arc::from("token"),
                expires_at: now + HOUR,
                refresh_at: now + Duration::from_secs(2700),
                generation,
                max_connection_lifetime: HOUR / 2,
            }),
            generation: AtomicU64::new(generation),
            refresh_task_started: AtomicBool::new(false),
        })
    }

    #[test]
    fn static_credentials_never_retire_connections() {
        let credentials = RedisCredentials::Static {
            username: Some("acl-user".to_string()),
            password: Some(String::new()),
        };
        // Static mode must not opt into any generation bookkeeping, whatever the tag says.
        assert!(!should_retire_connection(&credentials, 0));
        assert!(!should_retire_connection(&credentials, 42));
    }

    #[test]
    fn oauth_connection_on_current_generation_is_kept() {
        let credentials = RedisCredentials::Oauth(provider_at_generation(7));
        assert!(!should_retire_connection(&credentials, 7));
    }

    #[test]
    fn oauth_connection_on_superseded_generation_is_retired() {
        let credentials = RedisCredentials::Oauth(provider_at_generation(8));
        assert!(should_retire_connection(&credentials, 7));
    }

    #[test]
    fn client_secret_is_redacted_in_debug_output() {
        let config = EntraOauthConfig {
            tenant_id: "tenant".to_string(),
            client_id: "client".to_string(),
            client_secret: "super-secret".to_string(),
            scope: "https://example.com/app/.default".to_string(),
            username: None,
            refresh_margin: None,
            authority_host: DEFAULT_ENTRA_AUTHORITY_HOST.to_string(),
        };
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("super-secret"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
    }

    #[test]
    fn long_error_bodies_are_truncated_for_logs() {
        let body = "x".repeat(MAX_LOGGED_ERROR_BODY_LEN * 2);
        let truncated = truncate_for_log(&body);
        assert_eq!(truncated.len(), MAX_LOGGED_ERROR_BODY_LEN + 3);
        assert_eq!(truncate_for_log("short"), "short");
    }

    /// A throwaway HTTP server that answers each request with the next queued `(status, body)`
    /// and records the raw requests it saw, so the real fetch/parse/publish path can be tested
    /// end to end without a mocking dependency.
    struct MockTokenEndpoint {
        base_url: String,
        requests: Arc<parking_lot::Mutex<Vec<String>>>,
    }

    async fn spawn_token_endpoint(responses: Vec<(u16, &'static str)>) -> MockTokenEndpoint {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
        let requests = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);

        tokio::spawn(async move {
            for (status, body) in responses {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let request = read_request(&mut socket).await;
                recorded.lock().push(request);
                let response = format!(
                    "HTTP/1.1 {status} STATUS\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });

        MockTokenEndpoint { base_url, requests }
    }

    /// Answers every request with a redirect to `target`, so the client's redirect policy can be
    /// observed without needing a second real token endpoint to be reachable.
    async fn spawn_redirecting_endpoint(status: u16, target: &str) -> MockTokenEndpoint {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
        let requests = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        let location = format!("{target}/redirected");

        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let request = read_request(&mut socket).await;
                recorded.lock().push(request);
                let response = format!(
                    "HTTP/1.1 {status} STATUS\r\nlocation: {location}\r\n\
                     content-length: 0\r\nconnection: close\r\n\r\n"
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });

        MockTokenEndpoint { base_url, requests }
    }

    async fn read_request(socket: &mut tokio::net::TcpStream) -> String {
        let mut raw = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            match socket.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => raw.extend_from_slice(&buf[..n]),
            }
            let text = String::from_utf8_lossy(&raw).to_string();
            if let Some((headers, body)) = text.split_once("\r\n\r\n") {
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())?
                    })
                    .unwrap_or(0);
                if body.len() >= content_length {
                    return text;
                }
            }
        }
        String::from_utf8_lossy(&raw).to_string()
    }

    #[tokio::test]
    async fn token_lifecycle_parses_publishes_and_survives_a_failed_refresh() {
        let endpoint = spawn_token_endpoint(vec![
            (200, r#"{"access_token":"token-one","expires_in":3600}"#),
            (500, r#"{"error":"temporarily_unavailable"}"#),
            (200, r#"{"access_token":"token-two","expires_in":3600}"#),
        ])
        .await;

        let provider = connect_provider(config_for(&endpoint.base_url))
            .await
            .expect("initial token fetch should succeed");

        let first = provider.current();
        assert_eq!(&*first.secret, "token-one");
        assert_eq!(first.generation, 1);
        assert_eq!(provider.current_generation(), 1);
        assert_eq!(provider.max_connection_lifetime(), Duration::from_secs(675));
        assert!(first.expires_at > Instant::now() + Duration::from_secs(3500));
        assert!(first.refresh_at < first.expires_at);

        // A failed refresh must keep serving the previous, still-valid token rather than
        // invalidating every connection.
        let error = provider
            .refresh()
            .await
            .expect_err("the 500 should surface as an error");
        assert!(
            matches!(error, TokenError::Endpoint { status: 500, .. }),
            "{error:?}"
        );
        let after_failure = provider.current();
        assert_eq!(&*after_failure.secret, "token-one");
        assert_eq!(after_failure.generation, 1);

        // Only a success supersedes the token, which is what retires existing connections.
        provider.refresh().await.expect("refresh should succeed");
        let second = provider.current();
        assert_eq!(&*second.secret, "token-two");
        assert_eq!(second.generation, 2);
        assert!(should_retire_connection(
            &RedisCredentials::Oauth(Arc::clone(&provider)),
            first.generation
        ));

        let requests = endpoint.requests.lock().clone();
        assert_eq!(requests.len(), 3);
        assert!(
            requests[0].starts_with("POST /tenant/oauth2/v2.0/token HTTP/1.1"),
            "{}",
            requests[0]
        );
        for field in [
            "grant_type=client_credentials",
            "client_id=client",
            "client_secret=secret",
            "scope=https",
        ] {
            assert!(
                requests[0].contains(field),
                "missing {field} in {}",
                requests[0]
            );
        }
    }

    #[tokio::test]
    async fn malformed_token_response_is_rejected() {
        let endpoint = spawn_token_endpoint(vec![(200, r#"{"unexpected":true}"#)]).await;
        let error = connect_provider(config_for(&endpoint.base_url))
            .await
            .err()
            .expect("a response without access_token should fail");
        assert!(matches!(error, TokenError::Malformed(_)), "{error:?}");
    }

    fn manager_for(port: u16, credentials: RedisCredentials) -> StatsigRedisConnectionManager {
        StatsigRedisConnectionManager::new(
            ConnectionAddr::Tcp("127.0.0.1".to_string(), port),
            credentials,
        )
    }

    #[tokio::test]
    async fn static_user_only_sends_auth_with_an_empty_password() {
        // The STADXP-64 case, asserted on the wire rather than only at `resolve_redis_auth`:
        // redis-rs skips AUTH entirely when the password is None, so it must be Some("").
        let redis = FakeRedis::start().await;
        let manager = manager_for(
            redis.port,
            RedisCredentials::Static {
                username: Some("acl-user".to_string()),
                password: Some(String::new()),
            },
        );

        manager.connect().await.expect("connect should succeed");

        assert_eq!(
            redis.auth_commands(),
            vec![vec![
                "AUTH".to_string(),
                "acl-user".to_string(),
                String::new()
            ]]
        );
    }

    #[tokio::test]
    async fn static_user_and_password_send_both_arguments() {
        let redis = FakeRedis::start().await;
        let manager = manager_for(
            redis.port,
            RedisCredentials::Static {
                username: Some("acl-user".to_string()),
                password: Some("hunter2".to_string()),
            },
        );

        manager.connect().await.expect("connect should succeed");

        assert_eq!(
            redis.auth_commands(),
            vec![vec![
                "AUTH".to_string(),
                "acl-user".to_string(),
                "hunter2".to_string()
            ]]
        );
    }

    #[tokio::test]
    async fn static_password_only_authenticates_as_the_default_user() {
        let redis = FakeRedis::start().await;
        let manager = manager_for(
            redis.port,
            RedisCredentials::Static {
                username: None,
                password: Some("hunter2".to_string()),
            },
        );

        manager.connect().await.expect("connect should succeed");

        assert_eq!(
            redis.auth_commands(),
            vec![vec!["AUTH".to_string(), "hunter2".to_string()]]
        );
    }

    #[tokio::test]
    async fn static_without_credentials_sends_no_auth() {
        let redis = FakeRedis::start().await;
        let manager = manager_for(
            redis.port,
            RedisCredentials::Static {
                username: None,
                password: None,
            },
        );

        manager.connect().await.expect("connect should succeed");

        assert!(
            redis.auth_commands().is_empty(),
            "expected no AUTH, got {:?}",
            redis.commands()
        );
    }

    #[tokio::test]
    async fn static_connections_are_never_retired_on_check_in() {
        let redis = FakeRedis::start().await;
        let manager = manager_for(
            redis.port,
            RedisCredentials::Static {
                username: Some("acl-user".to_string()),
                password: Some(String::new()),
            },
        );

        let mut conn = manager.connect().await.expect("connect should succeed");

        assert!(!manager.has_broken(&mut conn));
    }

    #[tokio::test]
    async fn oauth_sends_the_token_as_a_single_argument_auth() {
        let token_endpoint = spawn_token_endpoint(vec![(
            200,
            r#"{"access_token":"token-one","expires_in":3600}"#,
        )])
        .await;
        let provider = connect_provider(config_for(&token_endpoint.base_url))
            .await
            .expect("initial token fetch should succeed");
        let redis = FakeRedis::start().await;
        let manager = manager_for(redis.port, RedisCredentials::Oauth(Arc::clone(&provider)));

        let conn = manager.connect().await.expect("connect should succeed");

        // No username configured means authenticating as the default user, so AUTH carries the
        // token alone rather than `AUTH <user> <token>`.
        assert_eq!(
            redis.auth_commands(),
            vec![vec!["AUTH".to_string(), "token-one".to_string()]]
        );
        assert_eq!(conn.generation, 1);
        assert_eq!(conn.token_expires_at, Some(provider.current().expires_at));
    }

    #[tokio::test]
    async fn oauth_username_is_sent_when_configured() {
        let token_endpoint = spawn_token_endpoint(vec![(
            200,
            r#"{"access_token":"token-one","expires_in":3600}"#,
        )])
        .await;
        let mut config = config_for(&token_endpoint.base_url);
        config.username = Some("object-id".to_string());
        let provider = connect_provider(config)
            .await
            .expect("initial token fetch should succeed");
        let redis = FakeRedis::start().await;
        let manager = manager_for(redis.port, RedisCredentials::Oauth(provider));

        manager.connect().await.expect("connect should succeed");

        assert_eq!(
            redis.auth_commands(),
            vec![vec![
                "AUTH".to_string(),
                "object-id".to_string(),
                "token-one".to_string()
            ]]
        );
    }

    #[tokio::test]
    async fn oauth_reconnect_after_a_refresh_uses_the_new_token() {
        let token_endpoint = spawn_token_endpoint(vec![
            (200, r#"{"access_token":"token-one","expires_in":3600}"#),
            (200, r#"{"access_token":"token-two","expires_in":3600}"#),
        ])
        .await;
        let provider = connect_provider(config_for(&token_endpoint.base_url))
            .await
            .expect("initial token fetch should succeed");
        let redis = FakeRedis::start().await;
        let manager = manager_for(redis.port, RedisCredentials::Oauth(Arc::clone(&provider)));

        let mut before_refresh = manager.connect().await.expect("first connect");
        provider.refresh().await.expect("refresh should succeed");
        let mut after_refresh = manager.connect().await.expect("second connect");

        // This is the property that makes rotation work at all: the manager reads the current
        // token at connect time rather than a startup snapshot.
        assert_eq!(
            redis.auth_commands(),
            vec![
                vec!["AUTH".to_string(), "token-one".to_string()],
                vec!["AUTH".to_string(), "token-two".to_string()],
            ]
        );
        // The pre-refresh connection is retired on check-in; the new one is kept.
        assert!(manager.has_broken(&mut before_refresh));
        assert!(!manager.has_broken(&mut after_refresh));
    }

    #[tokio::test]
    async fn is_valid_pings_a_healthy_connection() {
        let redis = FakeRedis::start().await;
        let manager = manager_for(
            redis.port,
            RedisCredentials::Static {
                username: None,
                password: None,
            },
        );
        let mut conn = manager.connect().await.expect("connect should succeed");

        manager.is_valid(&mut conn).await.expect("connection is up");

        assert!(
            redis.received_command("PING"),
            "expected a PING, got {:?}",
            redis.commands()
        );
    }

    #[tokio::test]
    async fn is_valid_refuses_a_connection_whose_token_expired() {
        let token_endpoint = spawn_token_endpoint(vec![(
            200,
            r#"{"access_token":"token-one","expires_in":3600}"#,
        )])
        .await;
        let provider = connect_provider(config_for(&token_endpoint.base_url))
            .await
            .expect("initial token fetch should succeed");
        let redis = FakeRedis::start().await;
        let manager = manager_for(redis.port, RedisCredentials::Oauth(provider));
        let mut conn = manager.connect().await.expect("connect should succeed");

        // Stand in for a connection that went idle right after a refresh and outlived its token.
        conn.token_expires_at = Some(Instant::now() - Duration::from_secs(1));
        let error = manager
            .is_valid(&mut conn)
            .await
            .expect_err("an expired token must never be handed to a caller");

        assert_eq!(error.kind(), ErrorKind::AuthenticationFailed);
    }

    #[tokio::test]
    async fn pooled_connections_run_commands_through_the_tracked_wrapper() {
        let redis = FakeRedis::start().await;
        let manager = manager_for(
            redis.port,
            RedisCredentials::Static {
                username: None,
                password: None,
            },
        );
        let pool = bb8::Pool::builder()
            .max_size(2)
            .min_idle(1)
            .build(manager)
            .await
            .expect("pool should build");

        let mut conn = pool.get().await.expect("checkout should succeed");
        // Mirrors both shapes RedisCache uses against a pooled connection: a raw command through
        // `&mut *conn`, and an AsyncCommands helper resolved by auto-deref.
        let pong: String = redis::cmd("PING")
            .query_async(&mut *conn)
            .await
            .expect("PING should round-trip");
        let _: () = conn
            .del("statsig|key")
            .await
            .expect("DEL should round-trip");

        assert_eq!(pong, "PONG");
        assert!(
            redis.received_command("DEL"),
            "expected a DEL, got {:?}",
            redis.commands()
        );
    }

    #[tokio::test]
    async fn token_endpoint_failure_at_startup_is_surfaced_to_the_caller() {
        // RedisCache::new relies on this to degrade to running without a datastore.
        let endpoint = spawn_token_endpoint(vec![(401, r#"{"error":"invalid_client"}"#)]).await;
        let error = connect_provider(config_for(&endpoint.base_url))
            .await
            .err()
            .expect("an unauthorized client should fail");
        match error {
            TokenError::Endpoint { status, body } => {
                assert_eq!(status, 401);
                assert!(body.contains("invalid_client"), "{body}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
        // An exhausted budget means exactly one attempt.
        assert_eq!(endpoint.requests.lock().len(), 1);
    }

    #[tokio::test]
    async fn initial_token_fetch_retries_a_transient_failure() {
        // Without this a single 429 or DNS blip during a rolling deploy would leave the pod
        // cacheless until someone restarted it.
        let endpoint = spawn_token_endpoint(vec![
            (503, r#"{"error":"temporarily_unavailable"}"#),
            (200, r#"{"access_token":"token-one","expires_in":3600}"#),
        ])
        .await;

        let provider =
            EntraTokenProvider::connect(config_for(&endpoint.base_url), MIN_RETRY_BACKOFF * 10)
                .await
                .expect("a transient failure should not be terminal");

        assert_eq!(&*provider.current().secret, "token-one");
        assert_eq!(endpoint.requests.lock().len(), 2);
    }

    #[tokio::test]
    async fn an_effectively_infinite_startup_budget_does_not_panic() {
        // `--redis-connection-timeout-in-s` is operator-supplied, and a huge value meaning "never
        // give up" would overflow `Instant + Duration` and crash the process in OAuth mode while
        // password mode started fine.
        let endpoint = spawn_token_endpoint(vec![
            (503, r#"{"error":"temporarily_unavailable"}"#),
            (200, r#"{"access_token":"token-one","expires_in":3600}"#),
        ])
        .await;

        let provider = EntraTokenProvider::connect(config_for(&endpoint.base_url), Duration::MAX)
            .await
            .expect("an oversized budget should still resolve");

        assert_eq!(&*provider.current().secret, "token-one");
    }

    #[tokio::test]
    async fn a_failed_shared_init_is_not_retried_by_the_next_caller() {
        // server.rs builds two caches in sequence. Retrying per caller would spend the startup
        // budget twice during an Entra outage, and could hand the second caller a datastore the
        // first never got.
        let endpoint = spawn_token_endpoint(vec![
            (503, r#"{"error":"temporarily_unavailable"}"#),
            (200, r#"{"access_token":"token-one","expires_in":3600}"#),
        ])
        .await;
        let config = config_for(&endpoint.base_url);

        let first = shared_entra_provider(config.clone(), Duration::ZERO).await;
        let second = shared_entra_provider(config, Duration::ZERO).await;

        assert!(first.is_err(), "the first call should exhaust its budget");
        assert!(
            second.is_err(),
            "the second caller should observe the cached failure, not a fresh success"
        );
        assert_eq!(
            endpoint.requests.lock().len(),
            1,
            "the second caller must not spend the budget again"
        );
    }

    #[tokio::test]
    async fn a_token_endpoint_returning_an_unusable_lifetime_does_not_spin() {
        // The refresh path must reject it rather than publish a token that is already dead.
        let endpoint = spawn_token_endpoint(vec![(
            200,
            r#"{"access_token":"token-one","expires_in":0}"#,
        )])
        .await;

        let error = connect_provider(config_for(&endpoint.base_url))
            .await
            .err()
            .expect("a zero-lifetime token should be rejected");

        assert!(matches!(error, TokenError::Malformed(_)), "{error:?}");
        assert_eq!(endpoint.requests.lock().len(), 1);
    }

    #[tokio::test]
    async fn a_blank_access_token_is_retried_rather_than_published() {
        // A blank token parses fine, so without an explicit check the initial fetch reports
        // success: the retry loop stops, the pool build fails against Redis, and the refresh task
        // is never started because it is gated on a live pool. Nothing would then be left to
        // replace the cached empty token.
        let endpoint = spawn_token_endpoint(vec![
            (200, r#"{"access_token":"","expires_in":3600}"#),
            (200, r#"{"access_token":"token-one","expires_in":3600}"#),
        ])
        .await;

        let provider =
            EntraTokenProvider::connect(config_for(&endpoint.base_url), MIN_RETRY_BACKOFF * 10)
                .await
                .expect("a blank token should be retried, not published");

        assert_eq!(&*provider.current().secret, "token-one");
        assert_eq!(endpoint.requests.lock().len(), 2);
    }

    #[tokio::test]
    async fn a_redirected_token_request_does_not_replay_the_secret() {
        // reqwest follows redirects by default and replays a POST body across a 307, so this
        // would otherwise hand `client_secret` to whatever host and scheme the response names,
        // regardless of the https check on the configured authority host.
        let elsewhere = spawn_token_endpoint(vec![(
            200,
            r#"{"access_token":"token-one","expires_in":3600}"#,
        )])
        .await;
        let redirector = spawn_redirecting_endpoint(307, &elsewhere.base_url).await;

        let error = connect_provider(config_for(&redirector.base_url))
            .await
            .err()
            .expect("a redirected token request should fail");

        assert!(
            matches!(error, TokenError::Endpoint { status: 307, .. }),
            "{error:?}"
        );
        assert!(
            elsewhere.requests.lock().is_empty(),
            "client_secret was replayed to the redirect target"
        );
    }

    #[tokio::test]
    async fn a_whitespace_only_access_token_is_rejected() {
        // Same failure mode as a blank one, and just as unusable as a Redis password.
        let endpoint =
            spawn_token_endpoint(vec![(200, r#"{"access_token":"  \n ","expires_in":3600}"#)])
                .await;

        let error = connect_provider(config_for(&endpoint.base_url))
            .await
            .err()
            .expect("a whitespace-only token should be rejected");

        assert!(matches!(error, TokenError::Malformed(_)), "{error:?}");
    }

    #[tokio::test]
    async fn the_refresh_task_is_only_started_once() {
        // The provider is shared across caches, so every one of them calls this.
        let endpoint = spawn_token_endpoint(vec![(
            200,
            r#"{"access_token":"token-one","expires_in":3600}"#,
        )])
        .await;
        let provider = connect_provider(config_for(&endpoint.base_url))
            .await
            .expect("initial token fetch should succeed");

        provider.spawn_refresh_task();
        provider.spawn_refresh_task();
        provider.spawn_refresh_task();

        assert!(provider.refresh_task_started.load(Ordering::SeqCst));
        // A second loop would mint tokens on its own schedule; only the initial fetch happened.
        assert_eq!(endpoint.requests.lock().len(), 1);
    }
}
