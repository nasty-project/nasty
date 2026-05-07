//! OpenID Connect client wrapper.
//!
//! Builds an `openidconnect` Core client from `OidcSettings`, generates the
//! authorization URL with PKCE + nonce, and exchanges an authorization code
//! for a verified ID token. Group extraction is dynamic so the operator can
//! point `groups_claim` at any custom claim the IdP emits.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use openidconnect::core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata};
use openidconnect::{
    AuthorizationCode, ClientId, ClientSecret, CsrfToken, HttpRequest, HttpResponse, IssuerUrl,
    Nonce, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope, TokenResponse,
};
use tokio::sync::RwLock;

use nasty_system::settings::OidcSettings;

/// Window during which a started OIDC login flow can be completed via callback.
const PENDING_TTL_SECS: u64 = 5 * 60;

#[derive(Debug, thiserror::Error)]
pub enum OidcError {
    #[error("OIDC is not enabled")]
    NotEnabled,
    #[error("OIDC is not fully configured: {0}")]
    NotConfigured(&'static str),
    #[error("OIDC discovery failed: {0}")]
    Discovery(String),
    #[error("invalid configuration: {0}")]
    Config(String),
    #[error("invalid state cookie or expired")]
    StateMismatch,
    #[error("token exchange failed: {0}")]
    TokenExchange(String),
    #[error("ID token verification failed: {0}")]
    TokenVerification(String),
    #[error("missing ID token in response")]
    MissingIdToken,
    #[error("login denied: no role mapping matched and no default role configured")]
    NoRoleMatch,
}

/// Identity returned from a successful callback. Username preference order is
/// `preferred_username` → `email` → `subject`.
#[derive(Debug, Clone)]
pub struct OidcIdentity {
    pub issuer: String,
    pub subject: String,
    pub email: Option<String>,
    pub preferred_username: Option<String>,
    pub groups: Vec<String>,
}

/// CoreClient with the redirect URI set (`EndpointSet` for redirect),
/// auth URL set from discovery (`EndpointSet`), and token URL discovered
/// via `from_provider_metadata` (`EndpointMaybeSet`). The type-state generics
/// have to be spelled out here because `clippy::type_complexity` complains
/// when this appears as a return type, and `from_provider_metadata` can't
/// be stored in a struct field with a default-typed `CoreClient`.
type ConfiguredCoreClient = openidconnect::Client<
    openidconnect::EmptyAdditionalClaims,
    openidconnect::core::CoreAuthDisplay,
    openidconnect::core::CoreGenderClaim,
    openidconnect::core::CoreJweContentEncryptionAlgorithm,
    openidconnect::core::CoreJsonWebKey,
    openidconnect::core::CoreAuthPrompt,
    openidconnect::StandardErrorResponse<openidconnect::core::CoreErrorResponseType>,
    openidconnect::core::CoreTokenResponse,
    openidconnect::core::CoreTokenIntrospectionResponse,
    openidconnect::core::CoreRevocableToken,
    openidconnect::core::CoreRevocationErrorResponse,
    openidconnect::EndpointSet,
    openidconnect::EndpointNotSet,
    openidconnect::EndpointNotSet,
    openidconnect::EndpointNotSet,
    openidconnect::EndpointMaybeSet,
    openidconnect::EndpointMaybeSet,
>;

/// Error type for the closure-bridge HTTP client we hand to openidconnect.
/// `oauth2::AsyncHttpClient` requires the error type to be
/// `Error + 'static`; thiserror gives us that for free.
#[derive(Debug, thiserror::Error)]
enum OidcHttpError {
    #[error("OIDC HTTP request failed: {0}")]
    Reqwest(#[from] reqwest::Error),
    #[error("OIDC HTTP response build failed: {0}")]
    Build(#[from] http::Error),
}

/// Bridge between openidconnect's `http::Request<Vec<u8>>` and our
/// reqwest 0.13 client. openidconnect 4 / oauth2 5 ship an
/// `AsyncHttpClient` impl for `reqwest::Client`, but they pin reqwest at
/// 0.12, so the impl is for a *different* `reqwest::Client` type than
/// ours. oauth2 also has a blanket impl for any
/// `Fn(HttpRequest) -> Future<Result<HttpResponse, _>>`, which is what
/// the call sites use via `move |req| async move { http_call(...).await }`.
async fn http_call(
    client: &reqwest::Client,
    req: HttpRequest,
) -> Result<HttpResponse, OidcHttpError> {
    let (parts, body) = req.into_parts();
    let mut builder = client.request(parts.method, parts.uri.to_string());
    for (name, value) in parts.headers.iter() {
        builder = builder.header(name, value);
    }
    let resp = builder.body(body).send().await?;
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = resp.bytes().await?.to_vec();
    let mut http_resp = http::Response::builder().status(status);
    for (name, value) in headers.iter() {
        http_resp = http_resp.header(name, value);
    }
    Ok(http_resp.body(body)?)
}

struct Pending {
    pkce_verifier_secret: String,
    nonce: Nonce,
    expires_at: u64,
}

pub struct OidcClient {
    // openidconnect 4 puts a type-state on Client's endpoint generics, so
    // storing a configured client as a field would mean writing out
    // `ConfiguredCoreClient` everywhere. Cheaper to keep the building
    // blocks and reconstruct per-call via build_client() — construction
    // is a few clones, no I/O.
    metadata: CoreProviderMetadata,
    client_id: ClientId,
    client_secret: Option<ClientSecret>,
    redirect_url: RedirectUrl,
    http_client: reqwest::Client,
    settings: OidcSettings,
    pending: Arc<RwLock<HashMap<String, Pending>>>,
}

#[derive(Default, Clone)]
pub struct OidcHolder {
    inner: Arc<RwLock<Option<Arc<OidcClient>>>>,
}

impl OidcHolder {
    pub async fn current(&self) -> Option<Arc<OidcClient>> {
        self.inner.read().await.clone()
    }

    /// Rebuild the client from current settings. Called on engine startup
    /// and whenever settings change.
    pub async fn rebuild(&self, settings: &OidcSettings) -> Result<(), OidcError> {
        if !settings.enabled {
            *self.inner.write().await = None;
            return Ok(());
        }
        let client = OidcClient::from_settings(settings).await?;
        *self.inner.write().await = Some(Arc::new(client));
        Ok(())
    }
}

impl OidcClient {
    pub async fn from_settings(settings: &OidcSettings) -> Result<Self, OidcError> {
        let issuer = settings
            .issuer_url
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or(OidcError::NotConfigured("issuer_url"))?;
        let client_id_str = settings
            .client_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or(OidcError::NotConfigured("client_id"))?;
        let redirect = settings
            .redirect_uri
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or(OidcError::NotConfigured("redirect_uri"))?;

        validate_issuer_url(issuer)?;
        let issuer_url =
            IssuerUrl::new(issuer.to_string()).map_err(|e| OidcError::Config(e.to_string()))?;

        // Don't follow redirects on the OIDC backchannel — per the openidconnect
        // crate's recommendation: a compromised IdP could otherwise 302 us
        // into hitting an internal URL during discovery or token exchange.
        let http_client = reqwest::ClientBuilder::new()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| OidcError::Config(format!("reqwest client: {e}")))?;

        let discover_client = http_client.clone();
        let metadata = CoreProviderMetadata::discover_async(issuer_url, &move |req| {
            let c = discover_client.clone();
            async move { http_call(&c, req).await }
        })
        .await
        .map_err(|e| OidcError::Discovery(e.to_string()))?;

        let client_secret = settings
            .client_secret
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|s| ClientSecret::new(s.to_string()));

        let redirect_url =
            RedirectUrl::new(redirect.to_string()).map_err(|e| OidcError::Config(e.to_string()))?;

        Ok(Self {
            metadata,
            client_id: ClientId::new(client_id_str.to_string()),
            client_secret,
            redirect_url,
            http_client,
            settings: settings.clone(),
            pending: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    fn build_client(&self) -> ConfiguredCoreClient {
        CoreClient::from_provider_metadata(
            self.metadata.clone(),
            self.client_id.clone(),
            self.client_secret.clone(),
        )
        .set_redirect_uri(self.redirect_url.clone())
    }

    /// Build an authorization URL and stash the PKCE verifier + nonce keyed by
    /// the CSRF state value. Returns the URL to redirect the browser to.
    pub async fn authorize_url(&self) -> url::Url {
        let client = self.build_client();
        let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
        let mut builder = client.authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        );
        for scope in &self.settings.scopes {
            builder = builder.add_scope(Scope::new(scope.clone()));
        }
        let (url, csrf, nonce) = builder.set_pkce_challenge(pkce_challenge).url();

        let now = unix_now();
        let mut pending = self.pending.write().await;
        pending.retain(|_, p| p.expires_at > now);
        pending.insert(
            csrf.secret().clone(),
            Pending {
                pkce_verifier_secret: pkce_verifier.secret().clone(),
                nonce,
                expires_at: now + PENDING_TTL_SECS,
            },
        );
        url
    }

    /// Look up a pending flow by the state value the IdP echoed back, exchange
    /// the code, validate the ID token (signature, audience, nonce, expiry),
    /// and return the resolved identity.
    pub async fn exchange_code(&self, state: &str, code: &str) -> Result<OidcIdentity, OidcError> {
        let pending = {
            let mut p = self.pending.write().await;
            p.remove(state).ok_or(OidcError::StateMismatch)?
        };
        if pending.expires_at <= unix_now() {
            return Err(OidcError::StateMismatch);
        }

        let client = self.build_client();
        let exchange_client = self.http_client.clone();
        let token_response = client
            .exchange_code(AuthorizationCode::new(code.to_string()))
            .map_err(|e| OidcError::TokenExchange(format!("token endpoint not configured: {e}")))?
            .set_pkce_verifier(PkceCodeVerifier::new(pending.pkce_verifier_secret))
            .request_async(&move |req| {
                let c = exchange_client.clone();
                async move { http_call(&c, req).await }
            })
            .await
            .map_err(|e| OidcError::TokenExchange(e.to_string()))?;

        let id_token = token_response.id_token().ok_or(OidcError::MissingIdToken)?;
        let verifier = client.id_token_verifier();
        let claims = id_token
            .claims(&verifier, &pending.nonce)
            .map_err(|e| OidcError::TokenVerification(e.to_string()))?;

        let issuer = claims.issuer().to_string();
        let subject = claims.subject().to_string();
        let preferred_username = claims.preferred_username().map(|p| p.as_str().to_string());
        let email = claims.email().map(|e| e.as_str().to_string());

        // The Core verifier consumes only standard claims. Re-parse the JWT
        // payload for the configured `groups_claim` — signature has already
        // been validated, so trusting the bytes is fine.
        let raw_payload = decode_jwt_payload(&id_token.to_string()).unwrap_or_default();
        let groups = extract_groups(&raw_payload, &self.settings.groups_claim);

        Ok(OidcIdentity {
            issuer,
            subject,
            email,
            preferred_username,
            groups,
        })
    }
}

/// Decode the middle (payload) segment of a JWS-compact-serialized JWT into
/// raw JSON. Returns Null on any structural failure.
fn decode_jwt_payload(jwt: &str) -> Option<serde_json::Value> {
    let mut parts = jwt.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Walk a dotted JSON path and return any string value(s) found at the leaf.
/// Accepts a single string or an array of strings. Anything else → empty list.
fn extract_groups(payload: &serde_json::Value, claim_path: &str) -> Vec<String> {
    let mut cur = payload.clone();
    for segment in claim_path.split('.') {
        cur = match cur {
            serde_json::Value::Object(mut o) => {
                o.remove(segment).unwrap_or(serde_json::Value::Null)
            }
            _ => return Vec::new(),
        };
    }
    match cur {
        serde_json::Value::Array(arr) => arr
            .into_iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect(),
        serde_json::Value::String(s) => vec![s],
        _ => Vec::new(),
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Map the ID-token groups list to a NASty role using the operator-provided
/// mapping table. Returns the configured default role when nothing matches,
/// or `None` when there is no default (login should be denied).
pub fn role_for_groups(groups: &[String], settings: &OidcSettings) -> Option<String> {
    for mapping in &settings.role_mappings {
        if groups.iter().any(|g| g == &mapping.group) {
            return Some(mapping.role.clone());
        }
    }
    settings.default_role.clone().filter(|s| !s.is_empty())
}

/// Quick non-network sanity check used by `auth.oidc.test` — dry-run a sample
/// claim payload against the configured mappings without bouncing the IdP.
pub fn dry_run_role(
    sample_claims: &serde_json::Value,
    settings: &OidcSettings,
) -> Result<Option<String>, String> {
    let groups = match sample_claims.get(&settings.groups_claim) {
        Some(serde_json::Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect::<Vec<_>>(),
        Some(serde_json::Value::String(s)) => vec![s.clone()],
        Some(_) => {
            return Err(format!(
                "claim `{}` is not a string or array of strings",
                settings.groups_claim
            ));
        }
        None => Vec::new(),
    };
    Ok(role_for_groups(&groups, settings))
}

/// Reject obviously dangerous issuer URLs before we hand them to the OIDC
/// discovery client (which would otherwise happily fetch from
/// `http://169.254.169.254/latest/...` and return AWS instance metadata).
///
/// Hostnames are not resolved here — DNS rebinding is hard to defend
/// against in code, and the OIDC discovery flow is admin-configured anyway.
/// What we *can* catch is the obvious mistakes: `http://` (downgrade),
/// loopback, link-local, the unspecified address, and known metadata
/// hostnames. Private-network RFC1918 hosts are allowed because a
/// self-hosted Keycloak on the same LAN is a legitimate setup.
pub fn validate_issuer_url(s: &str) -> Result<(), OidcError> {
    let u = url::Url::parse(s).map_err(|e| OidcError::Config(format!("issuer URL: {e}")))?;

    if u.scheme() != "https" {
        return Err(OidcError::Config(format!(
            "issuer URL must use https (got '{}')",
            u.scheme()
        )));
    }

    let host_str = u
        .host_str()
        .ok_or_else(|| OidcError::Config("issuer URL missing host".to_string()))?;

    // Match well-known metadata hostnames regardless of how DNS resolves them.
    let lower = host_str.to_ascii_lowercase();
    let metadata_hosts = ["metadata.google.internal", "metadata.goog"];
    if metadata_hosts.iter().any(|h| lower == *h) {
        return Err(OidcError::Config(format!(
            "issuer URL host '{host_str}' is a cloud metadata service"
        )));
    }

    // Literal-IP checks. A hostname that secretly resolves to 169.254.169.254
    // would still pass, but a typo'd `https://169.254.169.254/...` won't.
    match u.host() {
        Some(url::Host::Ipv4(v4)) => {
            if v4.is_loopback() || v4.is_link_local() || v4.is_unspecified() || v4.is_broadcast() {
                return Err(OidcError::Config(format!(
                    "issuer URL host {v4} is a loopback/link-local/broadcast/unspecified address"
                )));
            }
        }
        Some(url::Host::Ipv6(v6)) => {
            if v6.is_loopback() || v6.is_unspecified() {
                return Err(OidcError::Config(format!(
                    "issuer URL host {v6} is a loopback/unspecified address"
                )));
            }
            // is_unicast_link_local isn't stable yet; check the prefix manually.
            // fe80::/10 — top 10 bits are 1111111010
            if v6.segments()[0] & 0xffc0 == 0xfe80 {
                return Err(OidcError::Config(format!(
                    "issuer URL host {v6} is a link-local address"
                )));
            }
        }
        Some(url::Host::Domain(_)) | None => {} // hostname or absent — already checked above
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_issuer_url;

    #[test]
    fn accepts_normal_https() {
        assert!(validate_issuer_url("https://accounts.google.com").is_ok());
        assert!(validate_issuer_url("https://keycloak.example.com/realms/main").is_ok());
        // Private RFC1918 — legitimate for self-hosted IdPs on a LAN.
        assert!(validate_issuer_url("https://192.168.1.50:8443/realms/main").is_ok());
        assert!(validate_issuer_url("https://10.0.0.10/realms/main").is_ok());
    }

    #[test]
    fn rejects_http() {
        assert!(validate_issuer_url("http://idp.example.com").is_err());
    }

    #[test]
    fn rejects_loopback() {
        assert!(validate_issuer_url("https://127.0.0.1/").is_err());
        assert!(validate_issuer_url("https://[::1]/").is_err());
    }

    #[test]
    fn rejects_link_local_and_metadata() {
        assert!(validate_issuer_url("https://169.254.169.254/latest/meta-data/").is_err());
        assert!(validate_issuer_url("https://metadata.google.internal/").is_err());
        assert!(validate_issuer_url("https://[fe80::1]/").is_err());
    }

    #[test]
    fn rejects_garbage() {
        assert!(validate_issuer_url("not a url").is_err());
        assert!(validate_issuer_url("https://").is_err());
    }
}
