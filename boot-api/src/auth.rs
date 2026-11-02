//! Authentication and authorization for the boot API.
//!
//! Tokens are opaque 256-bit random values of the form
//! `boot_v1_<base64url-no-pad>`. Only SHA-256 hashes are stored in the API
//! database. Requests authenticate with `Authorization: Bearer <token>`.

use std::any::Any;
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;

use base64::Engine;
use dropshot::{
    ApiEndpointBodyContentType, ClientErrorStatusCode, ExtensionMode, ExtractorMetadata, HttpError,
    RequestContext, ServerContext, SharedExtractor,
};
use http::HeaderMap;
use http::header::AUTHORIZATION;
use sha2::{Digest, Sha256};

/// Prefix for all boot tokens.
pub const TOKEN_PREFIX: &str = "boot_v1_";
/// Number of random bytes in a token.
pub const TOKEN_BYTES: usize = 32;
/// Length of a formatted token (`boot_v1_` + 43 base64url characters).
pub const TOKEN_LEN: usize = 51;
/// Maximum accepted length of an `Authorization` credential.
pub const MAX_TOKEN_LEN: usize = 128;

/// A capability that can be required by an endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Capability {
    Read,
    Control,
    Admin,
}

impl Capability {
    pub fn as_str(self) -> &'static str {
        match self {
            Capability::Read => "read",
            Capability::Control => "control",
            Capability::Admin => "admin",
        }
    }
}

/// A single scope stored in the API database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scope {
    Read,
    Control,
    Admin,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Read => "read",
            Scope::Control => "control",
            Scope::Admin => "admin",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "read" => Some(Scope::Read),
            "control" => Some(Scope::Control),
            "admin" => Some(Scope::Admin),
            _ => None,
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A canonical, expanded set of scopes.
///
/// `control` implies `read`, and `admin` implies `control`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scopes(Vec<Scope>);

impl Default for Scopes {
    fn default() -> Self {
        Self(vec![Scope::Read])
    }
}

impl Scopes {
    pub fn read() -> Self {
        Self(vec![Scope::Read])
    }

    pub fn control() -> Self {
        Self(vec![Scope::Read, Scope::Control])
    }

    pub fn admin() -> Self {
        Self(vec![Scope::Read, Scope::Control, Scope::Admin])
    }

    /// Parse scopes supplied by a client. Empty input is `read`; unknown names
    /// are an error. The result is expanded and sorted canonically.
    pub fn from_names<I, S>(names: I) -> Result<Self, String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut scopes = Vec::new();
        for name in names {
            let name = name.as_ref();
            let scope = Scope::parse(name).ok_or_else(|| format!("unknown scope '{}'", name))?;
            scopes.push(scope);
        }
        if scopes.is_empty() {
            return Ok(Self::read());
        }
        Ok(Self::expand(scopes))
    }

    /// Parse the canonical comma-separated representation from the database.
    /// Unknown scopes are ignored and returned so callers can warn about them.
    pub fn from_db(value: &str) -> (Self, Vec<String>) {
        let mut scopes = Vec::new();
        let mut unknown = Vec::new();
        for part in value.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            match Scope::parse(part) {
                Some(scope) => scopes.push(scope),
                None => unknown.push(part.to_string()),
            }
        }
        if scopes.is_empty() && unknown.is_empty() {
            // An empty stored value cannot be created by this binary, but
            // treating it as read keeps older hand-created rows usable.
            scopes.push(Scope::Read);
        }
        (Self::expand(scopes), unknown)
    }

    fn expand(mut scopes: Vec<Scope>) -> Self {
        if scopes.contains(&Scope::Admin) {
            scopes.push(Scope::Control);
        }
        if scopes.contains(&Scope::Control) {
            scopes.push(Scope::Read);
        }
        scopes.sort();
        scopes.dedup();
        Self(scopes)
    }

    pub fn as_slice(&self) -> &[Scope] {
        &self.0
    }

    pub fn has(&self, capability: Capability) -> bool {
        match capability {
            Capability::Read => self.0.contains(&Scope::Read),
            Capability::Control => self.0.contains(&Scope::Control),
            Capability::Admin => self.0.contains(&Scope::Admin),
        }
    }

    pub fn contains_scope(&self, scope: Scope) -> bool {
        self.0.contains(&scope)
    }

    pub fn canonical(&self) -> String {
        self.0
            .iter()
            .map(|scope| scope.as_str())
            .collect::<Vec<_>>()
            .join(",")
    }

    pub fn display(&self) -> String {
        self.0
            .iter()
            .map(|scope| scope.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    pub fn names(&self) -> Vec<String> {
        self.0
            .iter()
            .map(|scope| scope.as_str().to_string())
            .collect()
    }
}

impl fmt::Display for Scopes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.display())
    }
}

/// A token as stored in the API database. The plaintext is never part of this
/// type.
#[derive(Debug, Clone)]
pub struct TokenRecord {
    pub id: String,
    pub name: String,
    pub token_hash: [u8; 32],
    pub scopes: Scopes,
    pub created_at: i64,
    pub expires_at: Option<i64>,
}

impl TokenRecord {
    pub fn is_expired(&self, now: i64) -> bool {
        self.expires_at.is_some_and(|expires_at| expires_at <= now)
    }

    fn check(&self, capability: Capability, now: i64) -> Result<Principal, AuthFailure> {
        if self.is_expired(now) {
            return Err(AuthFailure::Expired);
        }
        if !self.scopes.has(capability) {
            return Err(AuthFailure::MissingScope(capability));
        }
        Ok(Principal {
            id: self.id.clone(),
            name: self.name.clone(),
            scopes: self.scopes.clone(),
        })
    }
}

/// The authenticated identity associated with a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    pub id: String,
    pub name: String,
    pub scopes: Scopes,
}

impl Principal {
    pub fn has(&self, capability: Capability) -> bool {
        self.scopes.has(capability)
    }
}

/// Errors that can occur while loading or modifying the token table.
#[derive(Debug, Clone)]
pub enum StoreError {
    NotFound,
    Conflict,
    Invalid(String),
    Database(String),
    Io(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::NotFound => f.write_str("token not found"),
            StoreError::Conflict => f.write_str("token name already exists"),
            StoreError::Invalid(message)
            | StoreError::Database(message)
            | StoreError::Io(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<StoreError> for HttpError {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::NotFound => HttpError::for_client_error(
                None,
                ClientErrorStatusCode::NOT_FOUND,
                "token not found".to_string(),
            ),
            StoreError::Conflict => HttpError::for_client_error(
                None,
                ClientErrorStatusCode::CONFLICT,
                "token name already exists".to_string(),
            ),
            StoreError::Invalid(message) => HttpError::for_bad_request(None, message),
            StoreError::Database(message) | StoreError::Io(message) => {
                HttpError::for_internal_error(message)
            }
        }
    }
}

/// Request to create a token.
#[derive(Debug, Clone)]
pub struct CreateTokenRequest {
    pub name: String,
    pub scopes: Scopes,
    pub expires_at: Option<i64>,
    pub replace: bool,
}

/// A newly created token. The plaintext is returned exactly once.
#[derive(Debug, Clone)]
pub struct CreatedToken {
    pub record: TokenRecord,
    pub token: String,
}

/// Persistence operations needed by HTTP token administration.
#[async_trait::async_trait]
pub trait TokenStore: Send + Sync {
    async fn find_token_by_hash(&self, hash: &[u8; 32]) -> Result<Option<TokenRecord>, StoreError>;
    async fn find_token_by_id(&self, id: &str) -> Result<Option<TokenRecord>, StoreError>;
    async fn create_token(&self, request: CreateTokenRequest) -> Result<CreatedToken, StoreError>;
    async fn list_tokens(&self) -> Result<Vec<TokenRecord>, StoreError>;
    async fn revoke_token(&self, selector: &str) -> Result<usize, StoreError>;
    async fn revoke_all(&self) -> Result<usize, StoreError>;
}

/// Shared authentication state used as the Dropshot server context.
#[derive(Clone)]
pub struct AuthState {
    store: Arc<dyn TokenStore>,
}

impl AuthState {
    pub fn new(store: Arc<dyn TokenStore>) -> Self {
        Self { store }
    }

    pub fn store(&self) -> &Arc<dyn TokenStore> {
        &self.store
    }

    pub(crate) async fn authorize_failure(
        &self,
        headers: &HeaderMap,
        capability: Capability,
    ) -> Result<Principal, AuthFailure> {
        let token = parse_authorization(headers)?;
        let record = self
            .store
            .find_token_by_hash(&hash_token(&token))
            .await
            .map_err(|error| AuthFailure::Store(error.to_string()))?
            .ok_or(AuthFailure::Unknown)?;
        record.check(capability, now_unix())
    }

    /// Revalidate an already-authenticated principal. Used by long-lived
    /// WebSocket handlers.
    pub async fn is_authorized(&self, principal: &Principal, capability: Capability) -> bool {
        self.store
            .find_token_by_id(&principal.id)
            .await
            .ok()
            .flatten()
            .is_some_and(|record| record.check(capability, now_unix()).is_ok())
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

/// Compute the SHA-256 hash of the complete token string.
pub fn hash_token(token: &str) -> [u8; 32] {
    let digest = Sha256::digest(token.as_bytes());
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&digest);
    hash
}

/// Generate a fresh opaque token.
#[cfg(feature = "run")]
pub fn generate_token() -> String {
    use rand::RngExt;

    let mut bytes = [0u8; TOKEN_BYTES];
    rand::rng().fill(&mut bytes);
    format!(
        "{}{}",
        TOKEN_PREFIX,
        base64::prelude::BASE64_URL_SAFE_NO_PAD.encode(bytes)
    )
}

/// Generate a non-secret 16-character lowercase hex token id.
#[cfg(feature = "run")]
pub fn generate_token_id() -> String {
    use rand::RngExt;

    let mut bytes = [0u8; 8];
    rand::rng().fill(&mut bytes);
    bytes.iter().map(|byte| format!("{:02x}", byte)).collect()
}

/// Validate the token format. Hand-edited weak tokens are rejected.
pub fn validate_token_format(token: &str) -> bool {
    if token.len() != TOKEN_LEN || token.len() > MAX_TOKEN_LEN {
        return false;
    }
    let Some(encoded) = token.strip_prefix(TOKEN_PREFIX) else {
        return false;
    };
    if encoded.len() != 43 {
        return false;
    }
    match base64::prelude::BASE64_URL_SAFE_NO_PAD.decode(encoded) {
        Ok(bytes) => bytes.len() == TOKEN_BYTES,
        Err(_) => false,
    }
}

/// Validate a human-readable token name.
pub fn validate_token_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("token name must not be empty".to_string());
    }
    if name.len() > 128 {
        return Err("token name must be at most 128 bytes".to_string());
    }
    if name.contains(|character: char| character.is_control()) {
        return Err("token name must not contain control characters".to_string());
    }
    if name.len() == 16
        && name
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err("token name must not look like a token id".to_string());
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AuthFailure {
    Missing,
    Malformed,
    Unknown,
    Expired,
    MissingScope(Capability),
    Store(String),
}

impl AuthFailure {
    pub(crate) fn reason(&self) -> &'static str {
        match self {
            AuthFailure::Missing => "missing",
            AuthFailure::Malformed => "malformed",
            AuthFailure::Unknown => "unknown",
            AuthFailure::Expired => "expired",
            AuthFailure::MissingScope(_) => "scope",
            AuthFailure::Store(_) => "store",
        }
    }

    fn into_http(self) -> HttpError {
        match self {
            AuthFailure::MissingScope(capability) => {
                let mut error = HttpError::for_client_error(
                    None,
                    ClientErrorStatusCode::FORBIDDEN,
                    format!("token lacks required scope: {}", capability.as_str()),
                );
                let _ = error.add_header(
                    http::header::WWW_AUTHENTICATE,
                    format!(
                        "Bearer realm=\"boot-api\", error=\"insufficient_scope\", scope=\"{}\"",
                        capability.as_str()
                    ),
                );
                error
            }
            _ => {
                let mut error = HttpError::for_client_error(
                    None,
                    ClientErrorStatusCode::UNAUTHORIZED,
                    "missing or invalid bearer token".to_string(),
                );
                let _ =
                    error.add_header(http::header::WWW_AUTHENTICATE, "Bearer realm=\"boot-api\"");
                error
            }
        }
    }
}

fn parse_authorization(headers: &HeaderMap) -> Result<String, AuthFailure> {
    let mut values = headers.get_all(AUTHORIZATION).iter();
    let value = values.next().ok_or(AuthFailure::Missing)?;
    if values.next().is_some() {
        return Err(AuthFailure::Malformed);
    }
    let value = value.to_str().map_err(|_| AuthFailure::Malformed)?;
    let (scheme, rest) = value.split_once(' ').ok_or(AuthFailure::Malformed)?;
    if !scheme.eq_ignore_ascii_case("Bearer") {
        return Err(AuthFailure::Malformed);
    }
    let token = rest.trim_start_matches(' ');
    if token.is_empty()
        || token.len() > MAX_TOKEN_LEN
        || token.contains(|character: char| character.is_whitespace())
    {
        return Err(AuthFailure::Malformed);
    }
    if !validate_token_format(token) {
        return Err(AuthFailure::Malformed);
    }
    Ok(token.to_string())
}

/// Marker trait for endpoints that require a particular capability.
pub trait RequiredScope: Send + Sync + 'static {
    const CAPABILITY: Capability;
}

pub struct Read;
impl RequiredScope for Read {
    const CAPABILITY: Capability = Capability::Read;
}

pub struct Control;
impl RequiredScope for Control {
    const CAPABILITY: Capability = Capability::Control;
}

pub struct Admin;
impl RequiredScope for Admin {
    const CAPABILITY: Capability = Capability::Admin;
}

/// Shared extractor that authenticates the request before the body or a
/// WebSocket upgrade is processed.
pub struct Auth<S: RequiredScope>(pub Principal, PhantomData<S>);

impl<S: RequiredScope> Auth<S> {
    pub fn principal(&self) -> &Principal {
        &self.0
    }
}

impl<S: RequiredScope> SharedExtractor for Auth<S> {
    fn from_request<'a, 'async_trait, C>(
        rqctx: &'a RequestContext<C>,
    ) -> Pin<Box<dyn Future<Output = Result<Self, HttpError>> + Send + 'async_trait>>
    where
        C: 'async_trait + ServerContext,
        Self: 'async_trait,
        'a: 'async_trait,
    {
        Box::pin(async move {
            let state = (rqctx.context() as &dyn Any)
                .downcast_ref::<AuthState>()
                .ok_or_else(|| HttpError::for_internal_error("auth state unavailable".into()))?;
            match state
                .authorize_failure(rqctx.request.headers(), S::CAPABILITY)
                .await
            {
                Ok(principal) => Ok(Auth(principal, PhantomData)),
                Err(failure) => {
                    slog::warn!(
                        rqctx.log,
                        "authentication failed";
                        "reason" => failure.reason(),
                        "method" => %rqctx.request.method(),
                        "path" => %rqctx.request.uri().path(),
                        "remote_addr" => %rqctx.request.remote_addr(),
                    );
                    Err(failure.into_http())
                }
            }
        })
    }

    fn metadata(_: ApiEndpointBodyContentType) -> ExtractorMetadata {
        ExtractorMetadata {
            extension_mode: ExtensionMode::None,
            parameters: Vec::new(),
        }
    }
}

#[cfg(all(test, feature = "run"))]
mod tests {
    use super::*;
    use http::HeaderValue;

    fn auth_header(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, HeaderValue::from_str(value).unwrap());
        headers
    }

    #[test]
    fn generated_tokens_have_expected_shape() {
        let token = generate_token();
        assert!(token.starts_with(TOKEN_PREFIX));
        assert_eq!(token.len(), TOKEN_LEN);
        assert!(validate_token_format(&token));
        let id = generate_token_id();
        assert_eq!(id.len(), 16);
        assert!(
            id.bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        );
    }

    #[test]
    fn rejects_weak_tokens() {
        assert!(!validate_token_format("boot_v1_short"));
        assert!(!validate_token_format(
            "boot_v2_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        ));
        assert!(!validate_token_format(&format!(
            "boot_v1_{}",
            "0".repeat(42)
        )));
        assert!(!validate_token_format(&format!(
            "boot_v1_{}",
            "!".repeat(43)
        )));
    }

    #[test]
    fn parses_authorization_headers() {
        let token = generate_token();
        assert_eq!(
            parse_authorization(&auth_header(&format!("Bearer {}", token))).unwrap(),
            token
        );
        assert_eq!(
            parse_authorization(&auth_header(&format!("bearer    {}", token))).unwrap(),
            token
        );
        assert_eq!(
            parse_authorization(&HeaderMap::new()),
            Err(AuthFailure::Missing)
        );
        assert_eq!(
            parse_authorization(&auth_header("Basic abc")),
            Err(AuthFailure::Malformed)
        );
        assert_eq!(
            parse_authorization(&auth_header("Bearer ")),
            Err(AuthFailure::Malformed)
        );
        assert_eq!(
            parse_authorization(&auth_header("Bearer a b")),
            Err(AuthFailure::Malformed)
        );
    }

    #[test]
    fn rejects_multiple_authorization_headers() {
        let token = generate_token();
        let mut headers = auth_header(&format!("Bearer {}", token));
        headers.append(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", token)).unwrap(),
        );
        assert_eq!(parse_authorization(&headers), Err(AuthFailure::Malformed));
    }

    #[test]
    fn scopes_expand_and_enforce() {
        let admin = Scopes::admin();
        assert!(admin.has(Capability::Read));
        assert!(admin.has(Capability::Control));
        assert!(admin.has(Capability::Admin));
        let control = Scopes::control();
        assert!(control.has(Capability::Read));
        assert!(control.has(Capability::Control));
        assert!(!control.has(Capability::Admin));
        assert!(
            Scopes::from_names(["admin"])
                .unwrap()
                .has(Capability::Admin)
        );
        assert!(
            Scopes::from_names(std::iter::empty::<&str>())
                .unwrap()
                .has(Capability::Read)
        );
        assert!(Scopes::from_names(["bogus"]).is_err());
    }

    #[test]
    fn names_reject_ids_and_empty_values() {
        assert!(validate_token_name("laptop").is_ok());
        assert!(validate_token_name("").is_err());
        assert!(validate_token_name("9f2c1a7b4e8d0c31").is_err());
        assert!(validate_token_name("9F2C1A7B4E8D0C31").is_ok());
    }

    #[test]
    fn record_rejects_expired_and_scoped_tokens() {
        let now = now_unix();
        let mut record = TokenRecord {
            id: "0123456789abcdef".to_string(),
            name: "laptop".to_string(),
            token_hash: [0u8; 32],
            scopes: Scopes::read(),
            created_at: now,
            expires_at: None,
        };
        assert!(record.check(Capability::Read, now).is_ok());
        assert!(matches!(
            record.check(Capability::Control, now),
            Err(AuthFailure::MissingScope(Capability::Control))
        ));
        record.expires_at = Some(now - 1);
        assert_eq!(
            record.check(Capability::Read, now),
            Err(AuthFailure::Expired)
        );
    }
}
