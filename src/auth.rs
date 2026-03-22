use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use chrono::{DateTime, Utc};
use rand::RngCore;
use reqwest::blocking::Client;
use reqwest::header::CONTENT_TYPE;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tiny_http::{Header, Response, Server, StatusCode};
use url::Url;

const AUTH_ISSUER: &str = "https://auth.openai.com";
const CHATGPT_BASE_URL: &str = "https://chatgpt.com/backend-api";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CALLBACK_PORT: u16 = 1455;
const CALLBACK_PATH: &str = "/auth/callback";
const ORIGINATOR: &str = "codex_cli_rs";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_mode: Option<String>,
    #[serde(rename = "OPENAI_API_KEY", default, skip_serializing_if = "Option::is_none")]
    pub openai_api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<TokenData>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_refresh: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenData {
    pub id_token: String,
    pub access_token: String,
    pub refresh_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LoginSummary {
    pub email: Option<String>,
    pub plan_type: Option<String>,
    pub auth_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct AuthSession {
    pub auth: AuthFile,
    pub auth_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct UserProfile {
    pub email: Option<String>,
    pub plan_type: Option<String>,
    pub account_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ChatGptAccess {
    pub access_token: String,
    pub account_id: String,
}

#[derive(Debug, Deserialize)]
struct CodeExchangeResponse {
    id_token: String,
    access_token: String,
    refresh_token: String,
}

#[derive(Debug, Deserialize)]
struct ApiKeyExchangeResponse {
    access_token: String,
}

#[derive(Debug, Deserialize)]
struct RefreshResponse {
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct IdClaims {
    #[serde(default)]
    email: Option<String>,
    #[serde(rename = "https://api.openai.com/profile", default)]
    profile: Option<ProfileClaims>,
    #[serde(rename = "https://api.openai.com/auth", default)]
    auth: Option<AuthClaims>,
}

#[derive(Debug, Deserialize)]
struct ProfileClaims {
    #[serde(default)]
    email: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AuthClaims {
    #[serde(default)]
    chatgpt_plan_type: Option<String>,
    #[serde(default)]
    chatgpt_account_id: Option<String>,
}

pub fn load_auth_session() -> Result<AuthSession> {
    let auth_path = auth_file_path()?;
    let raw = fs::read_to_string(&auth_path)
        .with_context(|| format!("failed to read {}", auth_path.display()))?;
    let auth: AuthFile = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse {}", auth_path.display()))?;
    Ok(AuthSession { auth, auth_path })
}

pub fn logout() -> Result<bool> {
    let auth_path = auth_file_path()?;
    match fs::remove_file(&auth_path) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err).with_context(|| format!("failed to remove {}", auth_path.display())),
    }
}

pub fn login_with_chatgpt() -> Result<LoginSummary> {
    let pkce = generate_pkce();
    let state = generate_state();
    let redirect_uri = redirect_uri();
    let auth_url = build_authorize_url(&redirect_uri, &pkce, &state);

    let bind_addr = format!("127.0.0.1:{CALLBACK_PORT}");
    let server = Server::http(&bind_addr)
        .map_err(|err| anyhow!("failed to bind local callback server on {bind_addr}: {err}"))?;

    println!("OpenAI login URL:\n{auth_url}\n");
    if webbrowser::open(&auth_url).is_ok() {
        println!("Opened your browser for login. Complete the flow there.");
    } else {
        println!("Browser open failed. Paste the URL above into your browser.");
    }
    println!("Waiting for the callback on {redirect_uri} ...");

    let request = server
        .recv_timeout(Duration::from_secs(300))
        .context("timed out waiting for the login callback")?
        .ok_or_else(|| anyhow!("login callback server closed before authentication completed"))?;

    let callback_url = Url::parse(&format!("http://localhost{}", request.url()))
        .context("failed to parse the callback URL")?;

    if callback_url.path() != CALLBACK_PATH {
        let _ = request.respond(
            Response::from_string("Not found").with_status_code(StatusCode(404)),
        );
        bail!("unexpected callback path: {}", callback_url.path());
    }

    let params: std::collections::HashMap<String, String> =
        callback_url.query_pairs().into_owned().collect();

    if let Some(error) = params.get("error") {
        let description = params
            .get("error_description")
            .cloned()
            .unwrap_or_else(|| error.clone());
        let body = format!("<html><body><h1>Login failed</h1><p>{description}</p></body></html>");
        let response = Response::from_data(body).with_header(html_header()?);
        let _ = request.respond(response);
        bail!("oauth callback failed: {description}");
    }

    let returned_state = params
        .get("state")
        .ok_or_else(|| anyhow!("oauth callback was missing state"))?;
    if returned_state != &state {
        let _ = request.respond(
            Response::from_string("State mismatch").with_status_code(StatusCode(400)),
        );
        bail!("oauth callback state did not match the login request");
    }

    let code = params
        .get("code")
        .ok_or_else(|| anyhow!("oauth callback was missing code"))?;

    let http = http_client()?;
    let tokens = exchange_code_for_tokens(&http, code, &redirect_uri, &pkce.code_verifier)?;
    let profile = parse_id_token(&tokens.id_token)?;
    let api_key = exchange_id_token_for_api_key(&http, &tokens.id_token).ok();

    let auth = AuthFile {
        auth_mode: Some("chatgpt".to_string()),
        openai_api_key: api_key,
        tokens: Some(TokenData {
            account_id: profile.account_id.clone(),
            id_token: tokens.id_token,
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
        }),
        last_refresh: Some(Utc::now()),
    };
    let auth_path = save_auth(&auth)?;

    let body = "<html><body><h1>Login complete</h1><p>You can return to the terminal.</p></body></html>";
    let response = Response::from_string(body).with_header(html_header()?);
    let _ = request.respond(response);

    Ok(LoginSummary {
        email: profile.email,
        plan_type: profile.plan_type,
        auth_path,
    })
}

pub fn auth_file_path() -> Result<PathBuf> {
    Ok(codex_home()?.join("auth.json"))
}

pub fn user_profile(auth: &AuthFile) -> Result<UserProfile> {
    let Some(tokens) = auth.tokens.as_ref() else {
        return Ok(UserProfile {
            email: None,
            plan_type: None,
            account_id: None,
        });
    };
    parse_id_token(&tokens.id_token)
}

pub fn chatgpt_access(auth: &AuthFile) -> Result<ChatGptAccess> {
    let tokens = auth
        .tokens
        .as_ref()
        .ok_or_else(|| anyhow!("stored auth does not contain ChatGPT tokens"))?;
    let account_id = tokens
        .account_id
        .clone()
        .or_else(|| parse_id_token(&tokens.id_token).ok().and_then(|profile| profile.account_id))
        .ok_or_else(|| anyhow!("stored auth does not contain a ChatGPT account id"))?;

    Ok(ChatGptAccess {
        access_token: tokens.access_token.clone(),
        account_id,
    })
}

pub fn refresh_session_tokens(session: &mut AuthSession) -> Result<()> {
    let refresh_token = session
        .auth
        .tokens
        .as_ref()
        .map(|tokens| tokens.refresh_token.clone())
        .ok_or_else(|| anyhow!("stored auth does not contain a refresh token"))?;
    let http = http_client()?;
    refresh_chatgpt_tokens(session, &http, &refresh_token)
}

pub fn chatgpt_api_url(path: &str) -> String {
    format!("{CHATGPT_BASE_URL}{path}")
}

fn save_auth(auth: &AuthFile) -> Result<PathBuf> {
    let auth_path = auth_file_path()?;
    if let Some(parent) = auth_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let data = serde_json::to_string_pretty(auth).context("failed to serialize auth payload")?;
    fs::write(&auth_path, data).with_context(|| format!("failed to write {}", auth_path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let permissions = fs::Permissions::from_mode(0o600);
        fs::set_permissions(&auth_path, permissions)
            .with_context(|| format!("failed to secure {}", auth_path.display()))?;
    }

    Ok(auth_path)
}

fn codex_home() -> Result<PathBuf> {
    if let Ok(value) = std::env::var("CODEX_HOME") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Ok(PathBuf::from(trimmed));
        }
    }

    let home = dirs::home_dir().ok_or_else(|| anyhow!("could not determine the home directory"))?;
    Ok(home.join(".codex"))
}

fn http_client() -> Result<Client> {
    Client::builder()
        .user_agent("agent-may/0.1.0")
        .build()
        .context("failed to build the HTTP client")
}

fn redirect_uri() -> String {
    format!("http://localhost:{CALLBACK_PORT}{CALLBACK_PATH}")
}

fn build_authorize_url(redirect_uri: &str, pkce: &PkceCodes, state: &str) -> String {
    let query = [
        ("response_type", "code".to_string()),
        ("client_id", CLIENT_ID.to_string()),
        ("redirect_uri", redirect_uri.to_string()),
        (
            "scope",
            "openid profile email offline_access api.connectors.read api.connectors.invoke"
                .to_string(),
        ),
        ("code_challenge", pkce.code_challenge.clone()),
        ("code_challenge_method", "S256".to_string()),
        ("id_token_add_organizations", "true".to_string()),
        ("codex_cli_simplified_flow", "true".to_string()),
        ("state", state.to_string()),
        ("originator", ORIGINATOR.to_string()),
    ];

    let query_string = query
        .into_iter()
        .map(|(key, value)| format!("{key}={}", urlencoding::encode(&value)))
        .collect::<Vec<_>>()
        .join("&");

    format!("{AUTH_ISSUER}/oauth/authorize?{query_string}")
}

fn exchange_code_for_tokens(
    http: &Client,
    code: &str,
    redirect_uri: &str,
    code_verifier: &str,
) -> Result<CodeExchangeResponse> {
    let body = format!(
        "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}&code_verifier={}",
        urlencoding::encode(code),
        urlencoding::encode(redirect_uri),
        urlencoding::encode(CLIENT_ID),
        urlencoding::encode(code_verifier),
    );

    let response = http
        .post(format!("{AUTH_ISSUER}/oauth/token"))
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .context("failed to exchange the login code for tokens")?;

    let status = response.status();
    let body = response.text().context("failed to read token response")?;
    if !status.is_success() {
        bail!("token exchange failed with status {status}: {}", parse_error_message(&body));
    }

    serde_json::from_str(&body).context("failed to parse the token exchange response")
}

fn exchange_id_token_for_api_key(http: &Client, id_token: &str) -> Result<String> {
    let body = format!(
        "grant_type={}&client_id={}&requested_token={}&subject_token={}&subject_token_type={}",
        urlencoding::encode("urn:ietf:params:oauth:grant-type:token-exchange"),
        urlencoding::encode(CLIENT_ID),
        urlencoding::encode("openai-api-key"),
        urlencoding::encode(id_token),
        urlencoding::encode("urn:ietf:params:oauth:token-type:id_token"),
    );

    let response = http
        .post(format!("{AUTH_ISSUER}/oauth/token"))
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .context("failed to exchange the ID token for an API key")?;

    let status = response.status();
    let body = response
        .text()
        .context("failed to read the API key exchange response")?;
    if !status.is_success() {
        bail!("API key exchange failed with status {status}: {}", parse_error_message(&body));
    }

    let parsed: ApiKeyExchangeResponse =
        serde_json::from_str(&body).context("failed to parse the API key exchange response")?;
    Ok(parsed.access_token)
}

fn refresh_chatgpt_tokens(session: &mut AuthSession, http: &Client, refresh_token: &str) -> Result<()> {
    let response = http
        .post(format!("{AUTH_ISSUER}/oauth/token"))
        .header(CONTENT_TYPE, "application/json")
        .json(&serde_json::json!({
            "client_id": CLIENT_ID,
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
        }))
        .send()
        .context("failed to refresh the stored ChatGPT tokens")?;

    let status = response.status();
    let body = response
        .text()
        .context("failed to read the token refresh response")?;
    if !status.is_success() {
        bail!("token refresh failed with status {status}: {}", parse_error_message(&body));
    }

    let refreshed: RefreshResponse =
        serde_json::from_str(&body).context("failed to parse the token refresh response")?;
    let tokens = session
        .auth
        .tokens
        .as_mut()
        .ok_or_else(|| anyhow!("stored auth is missing tokens"))?;

    if let Some(id_token) = refreshed.id_token {
        tokens.id_token = id_token;
    }
    if let Some(access_token) = refreshed.access_token {
        tokens.access_token = access_token;
    }
    if let Some(refresh_token) = refreshed.refresh_token {
        tokens.refresh_token = refresh_token;
    }
    session.auth.last_refresh = Some(Utc::now());
    save_auth(&session.auth)?;
    Ok(())
}

fn parse_error_message(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return "unknown error".to_string();
    }

    let Ok(json) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return trimmed.to_string();
    };

    json.get("error_description")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            json.get("error")
                .and_then(serde_json::Value::as_object)
                .and_then(|obj| obj.get("message"))
                .and_then(serde_json::Value::as_str)
        })
        .or_else(|| json.get("error").and_then(serde_json::Value::as_str))
        .unwrap_or(trimmed)
        .to_string()
}

fn parse_id_token(id_token: &str) -> Result<UserProfile> {
    let payload = jwt_payload(id_token)?;
    let claims: IdClaims = serde_json::from_slice(&payload).context("failed to parse id_token claims")?;

    let email = claims
        .email
        .or_else(|| claims.profile.and_then(|profile| profile.email));
    let plan_type = claims
        .auth
        .as_ref()
        .and_then(|auth| auth.chatgpt_plan_type.clone());
    let account_id = claims
        .auth
        .and_then(|auth| auth.chatgpt_account_id);

    Ok(UserProfile {
        email,
        plan_type,
        account_id,
    })
}

fn jwt_payload(jwt: &str) -> Result<Vec<u8>> {
    let parts = jwt.split('.').collect::<Vec<_>>();
    if parts.len() != 3 {
        bail!("invalid JWT format");
    }

    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1])
        .context("failed to decode JWT payload")
}

fn html_header() -> Result<Header> {
    Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..])
        .map_err(|_| anyhow!("failed to construct an HTML response header"))
}

#[derive(Debug, Clone)]
struct PkceCodes {
    code_verifier: String,
    code_challenge: String,
}

fn generate_pkce() -> PkceCodes {
    let mut bytes = [0_u8; 64];
    rand::rng().fill_bytes(&mut bytes);
    let code_verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let digest = Sha256::digest(code_verifier.as_bytes());
    let code_challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);

    PkceCodes {
        code_verifier,
        code_challenge,
    }
}

fn generate_state() -> String {
    let mut bytes = [0_u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

#[allow(dead_code)]
fn auth_exists(path: &Path) -> bool {
    path.exists()
}
