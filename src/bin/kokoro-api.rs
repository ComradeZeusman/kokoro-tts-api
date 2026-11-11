use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Multipart, State},
    http::{HeaderMap, StatusCode, header},
    response::Response,
    routing::{get, post},
};
use kokoro_en::{
    KokoroTts, Voice,
    account::{
        AccountDetails, AccountStore, AccountSummary, AuthenticatedAccount, Registration,
        SubscriptionDuration,
    },
};
use serde::{Deserialize, Serialize};
use std::{
    error::Error,
    fs,
    net::SocketAddr,
    path::PathBuf,
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{io::AsyncWriteExt, process::Command, sync::Semaphore};

struct AppState {
    tts: Arc<KokoroTts>,
    voices: Vec<String>,
    synthesis_slot: Arc<Semaphore>,
    stt_slot: Arc<Semaphore>,
    accounts: AccountStore,
}

const STT_MAX_UPLOAD_BYTES: usize = 25 * 1024 * 1024;
const STT_MAX_DURATION_MS: i64 = 30 * 60 * 1000;
const WHISPER_URL: &str = "http://whisper-stt:8081/inference";

struct TemporaryAudioFile(PathBuf);

impl Drop for TemporaryAudioFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[derive(Deserialize)]
struct WhisperResponse {
    task: Option<String>,
    language: Option<String>,
    duration: Option<f64>,
    text: Option<String>,
}

#[derive(Deserialize)]
struct RegistrationRequest {
    name: String,
    email: String,
}

#[derive(Serialize)]
struct UsageResponse {
    account_id: String,
    account_status: String,
    subscription: Option<UsageSubscription>,
}

#[derive(Serialize)]
struct UsageSubscription {
    id: i64,
    status: String,
    character_limit: i64,
    characters_used: i64,
    characters_remaining: i64,
    stt_limit_ms: i64,
    stt_used_ms: i64,
    stt_remaining_ms: i64,
    starts_at: i64,
    expires_at: i64,
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
}

#[derive(Serialize)]
struct VoicesResponse {
    voices: Vec<String>,
}

#[derive(Deserialize)]
struct TtsRequest {
    text: String,
    voice: Option<String>,
    speed: Option<f32>,
    format: Option<String>,
}

#[derive(Deserialize)]
struct AdminActivationRequest {
    days: Option<u32>,
    months: Option<u32>,
    characters: i64,
    stt_seconds: Option<i64>,
}

#[derive(Deserialize)]
struct AdminExtensionRequest {
    days: Option<u32>,
    months: Option<u32>,
    add_characters: Option<i64>,
    add_stt_seconds: Option<i64>,
}

const CUSTOMER_SESSION_COOKIE: &str = "voicemw_customer_session";
const ADMIN_SESSION_COOKIE: &str = "voicemw_admin_session";
const WEB_SESSION_LIFETIME: i64 = 12 * 60 * 60;

#[derive(Deserialize)]
struct BrowserLoginRequest {
    email: String,
    password: String,
}

#[derive(Deserialize)]
struct CustomerPasswordChangeRequest {
    current_password: String,
    new_password: String,
}

#[derive(Serialize)]
struct BrowserLoginResponse {
    authenticated: bool,
    principal_type: &'static str,
    must_change_password: bool,
}

#[derive(Serialize)]
struct CustomerSessionResponse {
    authenticated: bool,
    principal_type: &'static str,
    must_change_password: bool,
    account: AccountDetails,
}

#[derive(Serialize)]
struct AdminSessionResponse {
    authenticated: bool,
    principal_type: &'static str,
    email: String,
}

fn browser_origin_valid(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get("origin").and_then(|v| v.to_str().ok()) else {
        return false;
    };

    let Some(host) = headers.get("host").and_then(|v| v.to_str().ok()) else {
        return false;
    };

    let Some(authority) = origin.strip_prefix("https://") else {
        return false;
    };

    let authority = authority.split('/').next().unwrap_or_default();

    !authority.is_empty() && authority.eq_ignore_ascii_case(host)
}

fn browser_cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let cookies = headers.get(header::COOKIE)?.to_str().ok()?;

    cookies.split(';').find_map(|cookie| {
        let (key, value) = cookie.trim().split_once('=')?;
        if key == name { Some(value) } else { None }
    })
}

fn browser_json_response<T: Serialize>(
    status: StatusCode,
    value: &T,
    set_cookie: &str,
) -> Result<Response, (StatusCode, String)> {
    let body = serde_json::to_vec(value).map_err(|error| {
        eprintln!("Could not serialize browser response: {}", error);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not prepare response".into(),
        )
    })?;

    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::SET_COOKIE, set_cookie)
        .body(Body::from(body))
        .map_err(|error| {
            eprintln!("Could not build browser response: {}", error);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not prepare response".into(),
            )
        })
}

fn session_cookie(name: &str, token: &str) -> String {
    format!(
        "{}={}; Path=/; Max-Age={}; Secure; HttpOnly; SameSite=Lax",
        name, token, WEB_SESSION_LIFETIME
    )
}

fn clear_session_cookie(name: &str) -> String {
    format!(
        "{}=; Path=/; Max-Age=0; Secure; HttpOnly; SameSite=Lax",
        name
    )
}

fn browser_auth_error() -> (StatusCode, String) {
    (StatusCode::UNAUTHORIZED, "Invalid email or password".into())
}

async fn customer_browser_login(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<BrowserLoginRequest>,
) -> Result<Response, (StatusCode, String)> {
    if !browser_origin_valid(&headers) {
        return Err((
            StatusCode::FORBIDDEN,
            "Invalid browser request origin".into(),
        ));
    }

    let account = state
        .accounts
        .get_account_by_email(&request.email)
        .map_err(|error| {
            eprintln!("Customer login lookup failed: {}", error);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Login could not be completed".into(),
            )
        })?
        .ok_or_else(browser_auth_error)?;

    let credentials = state
        .accounts
        .get_customer_credentials(&account.account.id)
        .map_err(|error| {
            eprintln!("Customer credential lookup failed: {}", error);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Login could not be completed".into(),
            )
        })?
        .ok_or_else(browser_auth_error)?;

    let password_valid =
        kokoro_en::admin_auth::verify_password_hash(&request.password, &credentials.0).map_err(
            |error| {
                eprintln!("Customer password verification failed: {}", error);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Login could not be completed".into(),
                )
            },
        )?;

    if !password_valid {
        return Err(browser_auth_error());
    }

    if account.account.status == "suspended" {
        return Err((StatusCode::FORBIDDEN, "This account is suspended".into()));
    }

    let token = state
        .accounts
        .create_web_session("customer", &account.account.id, WEB_SESSION_LIFETIME)
        .map_err(|error| {
            eprintln!("Customer session creation failed: {}", error);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not create browser session".into(),
            )
        })?;

    let response = BrowserLoginResponse {
        authenticated: true,
        principal_type: "customer",
        must_change_password: credentials.1,
    };

    browser_json_response(
        StatusCode::OK,
        &response,
        &session_cookie(CUSTOMER_SESSION_COOKIE, &token),
    )
}

async fn admin_browser_login(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<BrowserLoginRequest>,
) -> Result<Response, (StatusCode, String)> {
    if !browser_origin_valid(&headers) {
        return Err((
            StatusCode::FORBIDDEN,
            "Invalid browser request origin".into(),
        ));
    }

    let valid = kokoro_en::admin_auth::verify_credentials(&request.email, &request.password)
        .map_err(|error| {
            eprintln!("Administrator authentication failed: {}", error);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Login could not be completed".into(),
            )
        })?;

    if !valid {
        return Err(browser_auth_error());
    }

    let email = request.email.trim().to_lowercase();

    let token = state
        .accounts
        .create_web_session("admin", &email, WEB_SESSION_LIFETIME)
        .map_err(|error| {
            eprintln!("Administrator session creation failed: {}", error);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not create browser session".into(),
            )
        })?;

    let response = BrowserLoginResponse {
        authenticated: true,
        principal_type: "admin",
        must_change_password: false,
    };

    browser_json_response(
        StatusCode::OK,
        &response,
        &session_cookie(ADMIN_SESSION_COOKIE, &token),
    )
}

async fn customer_browser_me(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<CustomerSessionResponse>, (StatusCode, String)> {
    let token = browser_cookie(&headers, CUSTOMER_SESSION_COOKIE).ok_or_else(browser_auth_error)?;

    let session = state
        .accounts
        .authenticate_web_session(token)
        .map_err(|error| {
            eprintln!("Customer session verification failed: {}", error);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not verify session".into(),
            )
        })?
        .ok_or_else(browser_auth_error)?;

    if session.0 != "customer" {
        return Err(browser_auth_error());
    }

    let account = state
        .accounts
        .get_account(&session.1)
        .map_err(|error| {
            eprintln!("Customer session account lookup failed: {}", error);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not retrieve account".into(),
            )
        })?
        .ok_or_else(browser_auth_error)?;

    if account.account.status == "suspended" {
        return Err((StatusCode::FORBIDDEN, "This account is suspended".into()));
    }

    let credentials = state
        .accounts
        .get_customer_credentials(&session.1)
        .map_err(|error| {
            eprintln!("Customer credential lookup failed: {}", error);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not retrieve account credentials".into(),
            )
        })?;

    Ok(Json(CustomerSessionResponse {
        authenticated: true,
        principal_type: "customer",
        must_change_password: credentials.map(|value| value.1).unwrap_or(true),
        account,
    }))
}

async fn customer_browser_change_password(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<CustomerPasswordChangeRequest>,
) -> Result<Response, (StatusCode, String)> {
    if !browser_origin_valid(&headers) {
        return Err((
            StatusCode::FORBIDDEN,
            "Invalid browser request origin".into(),
        ));
    }

    if request.new_password.len() < 12 {
        return Err((
            StatusCode::BAD_REQUEST,
            "New password must contain at least 12 characters".into(),
        ));
    }

    if request.new_password == request.current_password {
        return Err((
            StatusCode::BAD_REQUEST,
            "New password must be different from the current password".into(),
        ));
    }

    let token = browser_cookie(&headers, CUSTOMER_SESSION_COOKIE).ok_or_else(browser_auth_error)?;

    let session = state
        .accounts
        .authenticate_web_session(token)
        .map_err(|error| {
            eprintln!("Customer password session verification failed: {}", error);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not verify session".into(),
            )
        })?
        .ok_or_else(browser_auth_error)?;

    if session.0 != "customer" {
        return Err(browser_auth_error());
    }

    let account = state
        .accounts
        .get_account(&session.1)
        .map_err(|error| {
            eprintln!("Customer password account lookup failed: {}", error);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not retrieve account".into(),
            )
        })?
        .ok_or_else(browser_auth_error)?;

    if account.account.status == "suspended" {
        return Err((StatusCode::FORBIDDEN, "This account is suspended".into()));
    }

    let credentials = state
        .accounts
        .get_customer_credentials(&session.1)
        .map_err(|error| {
            eprintln!("Customer password credential lookup failed: {}", error);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not retrieve credentials".into(),
            )
        })?
        .ok_or_else(browser_auth_error)?;

    let current_password_valid =
        kokoro_en::admin_auth::verify_password_hash(&request.current_password, &credentials.0)
            .map_err(|error| {
                eprintln!("Customer current password verification failed: {}", error);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Could not verify current password".into(),
                )
            })?;

    if !current_password_valid {
        return Err((
            StatusCode::UNAUTHORIZED,
            "Current password is incorrect".into(),
        ));
    }

    let new_hash =
        kokoro_en::admin_auth::hash_customer_password(&request.new_password).map_err(|error| {
            eprintln!("Customer password hashing failed: {}", error);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not update password".into(),
            )
        })?;

    state
        .accounts
        .set_customer_password(&session.1, &new_hash, false)
        .map_err(|error| {
            eprintln!("Customer password update failed: {}", error);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not update password".into(),
            )
        })?;

    browser_json_response(
        StatusCode::OK,
        &serde_json::json!({
            "success": true,
            "message": "Password changed. Please log in again."
        }),
        &clear_session_cookie(CUSTOMER_SESSION_COOKIE),
    )
}

async fn authenticate_admin_browser_request(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<String, (StatusCode, String)> {
    let token = browser_cookie(headers, ADMIN_SESSION_COOKIE).ok_or_else(browser_auth_error)?;

    let session = state
        .accounts
        .authenticate_web_session(token)
        .map_err(|error| {
            eprintln!("Administrator session verification failed: {}", error);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not verify administrator session".into(),
            )
        })?
        .ok_or_else(browser_auth_error)?;

    if session.0 != "admin" {
        return Err(browser_auth_error());
    }

    Ok(session.1)
}

fn admin_subscription_duration(
    days: Option<u32>,
    months: Option<u32>,
) -> Result<SubscriptionDuration, (StatusCode, String)> {
    match (days, months) {
        (Some(value), None) if value > 0 => Ok(SubscriptionDuration::Days(value)),
        (None, Some(value)) if value > 0 => Ok(SubscriptionDuration::Months(value)),
        _ => Err((
            StatusCode::BAD_REQUEST,
            "Specify exactly one positive duration: days or months".into(),
        )),
    }
}

async fn admin_browser_activate(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Path(account_id): axum::extract::Path<String>,
    Json(request): Json<AdminActivationRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if !browser_origin_valid(&headers) {
        return Err((
            StatusCode::FORBIDDEN,
            "Invalid browser request origin".into(),
        ));
    }

    authenticate_admin_browser_request(&state, &headers).await?;

    let duration = admin_subscription_duration(request.days, request.months)?;
    let stt_seconds = request.stt_seconds.unwrap_or(0);

    if stt_seconds < 0 {
        return Err((
            StatusCode::BAD_REQUEST,
            "STT seconds cannot be negative".into(),
        ));
    }

    let stt_ms = stt_seconds
        .checked_mul(1000)
        .ok_or((StatusCode::BAD_REQUEST, "STT allowance is too large".into()))?;

    state
        .accounts
        .activate_with_stt(&account_id, duration, request.characters, stt_ms)
        .map_err(|error| {
            eprintln!("Administrator subscription activation failed: {}", error);
            (StatusCode::BAD_REQUEST, format!("{:#}", error))
        })?;

    Ok(Json(
        serde_json::json!({"success": true, "account_id": account_id}),
    ))
}

async fn admin_browser_extend(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Path(account_id): axum::extract::Path<String>,
    Json(request): Json<AdminExtensionRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if !browser_origin_valid(&headers) {
        return Err((
            StatusCode::FORBIDDEN,
            "Invalid browser request origin".into(),
        ));
    }

    authenticate_admin_browser_request(&state, &headers).await?;

    let duration = admin_subscription_duration(request.days, request.months)?;
    let additional_characters = request.add_characters.unwrap_or(0);
    let stt_seconds = request.add_stt_seconds.unwrap_or(0);

    if stt_seconds < 0 {
        return Err((
            StatusCode::BAD_REQUEST,
            "Additional STT seconds cannot be negative".into(),
        ));
    }

    let additional_stt_ms = stt_seconds.checked_mul(1000).ok_or((
        StatusCode::BAD_REQUEST,
        "Additional STT allowance is too large".into(),
    ))?;

    state
        .accounts
        .extend_subscription_with_stt(
            &account_id,
            duration,
            additional_characters,
            additional_stt_ms,
        )
        .map_err(|error| {
            eprintln!("Administrator subscription extension failed: {}", error);
            (StatusCode::BAD_REQUEST, format!("{:#}", error))
        })?;

    Ok(Json(
        serde_json::json!({"success": true, "account_id": account_id}),
    ))
}

async fn admin_browser_set_status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Path(account_id): axum::extract::Path<String>,
    status: &'static str,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if !browser_origin_valid(&headers) {
        return Err((
            StatusCode::FORBIDDEN,
            "Invalid browser request origin".into(),
        ));
    }

    authenticate_admin_browser_request(&state, &headers).await?;

    state
        .accounts
        .set_account_status(&account_id, status)
        .map_err(|error| {
            eprintln!("Administrator account status update failed: {}", error);
            (StatusCode::BAD_REQUEST, format!("{:#}", error))
        })?;

    Ok(Json(serde_json::json!({
        "success": true,
        "account_id": account_id,
        "status": status
    })))
}

async fn admin_browser_suspend(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    path: axum::extract::Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    admin_browser_set_status(State(state), headers, path, "suspended").await
}

async fn admin_browser_unsuspend(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    path: axum::extract::Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    admin_browser_set_status(State(state), headers, path, "active").await
}

async fn admin_browser_rotate_key(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Path(account_id): axum::extract::Path<String>,
) -> Result<Response, (StatusCode, String)> {
    if !browser_origin_valid(&headers) {
        return Err((
            StatusCode::FORBIDDEN,
            "Invalid browser request origin".into(),
        ));
    }

    authenticate_admin_browser_request(&state, &headers).await?;

    let key = state
        .accounts
        .rotate_api_key(&account_id)
        .map_err(|error| {
            eprintln!("Administrator API key rotation failed: {}", error);
            (StatusCode::BAD_REQUEST, format!("{:#}", error))
        })?;

    // Return the plaintext key once, without caching or logging it.
    let body = serde_json::to_vec(&serde_json::json!({
        "success": true,
        "account_id": account_id,
        "api_key": key
    }))
    .map_err(|error| {
        eprintln!("Could not serialize API key rotation response: {}", error);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not prepare API key response".into(),
        )
    })?;

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from(body))
        .map_err(|error| {
            eprintln!("Could not build API key rotation response: {}", error);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not prepare API key response".into(),
            )
        })
}

async fn admin_browser_revoke_keys(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Path(account_id): axum::extract::Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if !browser_origin_valid(&headers) {
        return Err((
            StatusCode::FORBIDDEN,
            "Invalid browser request origin".into(),
        ));
    }

    authenticate_admin_browser_request(&state, &headers).await?;

    // Confirm the account exists before attempting revocation.
    state
        .accounts
        .get_account(&account_id)
        .map_err(|error| {
            eprintln!(
                "Administrator account lookup failed during key revocation: {}",
                error
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not retrieve customer account".into(),
            )
        })?
        .ok_or((StatusCode::NOT_FOUND, "Customer account not found".into()))?;

    let revoked = state
        .accounts
        .revoke_api_keys(&account_id)
        .map_err(|error| {
            eprintln!("Administrator API key revocation failed: {}", error);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not revoke API keys".into(),
            )
        })?;

    Ok(Json(serde_json::json!({
        "success": true,
        "account_id": account_id,
        "revoked_keys": revoked
    })))
}

async fn admin_browser_accounts(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<AccountSummary>>, (StatusCode, String)> {
    authenticate_admin_browser_request(&state, &headers).await?;

    let accounts = state.accounts.list_accounts().map_err(|error| {
        eprintln!("Administrator account listing failed: {}", error);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not retrieve customer accounts".into(),
        )
    })?;

    Ok(Json(accounts))
}

async fn admin_browser_account_details(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Path(account_id): axum::extract::Path<String>,
) -> Result<Json<AccountDetails>, (StatusCode, String)> {
    authenticate_admin_browser_request(&state, &headers).await?;

    let account = state
        .accounts
        .get_account(&account_id)
        .map_err(|error| {
            eprintln!("Administrator account lookup failed: {}", error);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not retrieve customer account".into(),
            )
        })?
        .ok_or((StatusCode::NOT_FOUND, "Customer account not found".into()))?;

    Ok(Json(account))
}

async fn admin_browser_me(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<AdminSessionResponse>, (StatusCode, String)> {
    let email = authenticate_admin_browser_request(&state, &headers).await?;

    Ok(Json(AdminSessionResponse {
        authenticated: true,
        principal_type: "admin",
        email,
    }))
}

async fn customer_browser_logout(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response, (StatusCode, String)> {
    if !browser_origin_valid(&headers) {
        return Err((
            StatusCode::FORBIDDEN,
            "Invalid browser request origin".into(),
        ));
    }

    if let Some(token) = browser_cookie(&headers, CUSTOMER_SESSION_COOKIE) {
        state.accounts.revoke_web_session(token).map_err(|error| {
            eprintln!("Customer logout failed: {}", error);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not end session".into(),
            )
        })?;
    }

    browser_json_response(
        StatusCode::OK,
        &serde_json::json!({"authenticated": false}),
        &clear_session_cookie(CUSTOMER_SESSION_COOKIE),
    )
}

async fn admin_browser_logout(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response, (StatusCode, String)> {
    if !browser_origin_valid(&headers) {
        return Err((
            StatusCode::FORBIDDEN,
            "Invalid browser request origin".into(),
        ));
    }

    if let Some(token) = browser_cookie(&headers, ADMIN_SESSION_COOKIE) {
        state.accounts.revoke_web_session(token).map_err(|error| {
            eprintln!("Administrator logout failed: {}", error);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not end session".into(),
            )
        })?;
    }

    browser_json_response(
        StatusCode::OK,
        &serde_json::json!({"authenticated": false}),
        &clear_session_cookie(ADMIN_SESSION_COOKIE),
    )
}

fn api_key_from_headers(headers: &HeaderMap) -> Result<String, (StatusCode, String)> {
    let value = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .ok_or((
            StatusCode::UNAUTHORIZED,
            "Missing Authorization: Bearer <API_KEY> header".into(),
        ))?;

    let (scheme, key) = value.split_once(' ').ok_or((
        StatusCode::UNAUTHORIZED,
        "Use Authorization: Bearer <API_KEY>".into(),
    ))?;

    if !scheme.eq_ignore_ascii_case("bearer") || key.trim().is_empty() {
        return Err((
            StatusCode::UNAUTHORIZED,
            "Use Authorization: Bearer <API_KEY>".into(),
        ));
    }

    Ok(key.trim().to_string())
}

fn authenticate_request(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<AuthenticatedAccount, (StatusCode, String)> {
    let api_key = api_key_from_headers(headers)?;

    state
        .accounts
        .authenticate_any_status(&api_key)
        .map_err(|_| {
            (
                StatusCode::UNAUTHORIZED,
                "Invalid or revoked API key".into(),
            )
        })
}

async fn register(
    State(state): State<Arc<AppState>>,
    Json(request): Json<RegistrationRequest>,
) -> Result<Json<Registration>, (StatusCode, String)> {
    state
        .accounts
        .register(&request.name, &request.email)
        .map(Json)
        .map_err(|error| {
            let message = error.to_string();

            if message.contains("UNIQUE constraint failed") {
                (
                    StatusCode::CONFLICT,
                    "An account with this email already exists".into(),
                )
            } else if message.contains("Name must") || message.contains("email address") {
                (StatusCode::BAD_REQUEST, message)
            } else {
                eprintln!("Account registration failed: {}", message);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Registration could not be completed".into(),
                )
            }
        })
}

async fn authenticate_customer_browser_request(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<AuthenticatedAccount, (StatusCode, String)> {
    let token = browser_cookie(headers, CUSTOMER_SESSION_COOKIE).ok_or_else(browser_auth_error)?;

    let session = state
        .accounts
        .authenticate_web_session(token)
        .map_err(|error| {
            eprintln!("Customer browser session verification failed: {}", error);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not verify customer session".into(),
            )
        })?
        .ok_or_else(browser_auth_error)?;

    if session.0 != "customer" {
        return Err(browser_auth_error());
    }

    let details = state
        .accounts
        .get_account(&session.1)
        .map_err(|error| {
            eprintln!("Customer browser account lookup failed: {}", error);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not retrieve customer account".into(),
            )
        })?
        .ok_or_else(browser_auth_error)?;

    if details.account.status == "suspended" {
        return Err((StatusCode::FORBIDDEN, "This account is suspended".into()));
    }

    let credentials = state
        .accounts
        .get_customer_credentials(&details.account.id)
        .map_err(|error| {
            eprintln!("Customer browser credential lookup failed: {}", error);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not verify customer credentials".into(),
            )
        })?;

    if credentials.map(|value| value.1).unwrap_or(true) {
        return Err((
            StatusCode::FORBIDDEN,
            "Password change required before accessing customer features".into(),
        ));
    }

    Ok(AuthenticatedAccount {
        account_id: details.account.id,
        status: details.account.status,
    })
}

async fn customer_browser_account(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<AccountDetails>, (StatusCode, String)> {
    let account = authenticate_customer_browser_request(&state, &headers).await?;

    state
        .accounts
        .get_account(&account.account_id)
        .map_err(|error| {
            eprintln!("Customer browser account lookup failed: {}", error);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not retrieve account details".into(),
            )
        })?
        .map(Json)
        .ok_or((StatusCode::NOT_FOUND, "Account not found".into()))
}

async fn customer_browser_usage(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<UsageResponse>, (StatusCode, String)> {
    let account = authenticate_customer_browser_request(&state, &headers).await?;

    let details = state
        .accounts
        .get_account(&account.account_id)
        .map_err(|error| {
            eprintln!("Customer browser usage lookup failed: {}", error);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not retrieve usage details".into(),
            )
        })?
        .ok_or((StatusCode::NOT_FOUND, "Account not found".into()))?;

    let subscription = details.subscription.map(|sub| UsageSubscription {
        id: sub.id,
        status: sub.status,
        character_limit: sub.character_limit,
        characters_used: sub.characters_used,
        characters_remaining: (sub.character_limit - sub.characters_used).max(0),
        stt_limit_ms: sub.stt_limit_ms,
        stt_used_ms: sub.stt_used_ms,
        stt_remaining_ms: (sub.stt_limit_ms - sub.stt_used_ms).max(0),
        starts_at: sub.starts_at,
        expires_at: sub.expires_at,
    });

    Ok(Json(UsageResponse {
        account_id: account.account_id,
        account_status: account.status,
        subscription,
    }))
}

async fn account_details(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<AccountDetails>, (StatusCode, String)> {
    let account = authenticate_request(&state, &headers)?;

    state
        .accounts
        .get_account(&account.account_id)
        .map_err(|error| {
            eprintln!("Account lookup failed: {}", error);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not retrieve account details".into(),
            )
        })?
        .map(Json)
        .ok_or((StatusCode::NOT_FOUND, "Account not found".into()))
}

async fn usage_details(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<UsageResponse>, (StatusCode, String)> {
    let account = authenticate_request(&state, &headers)?;

    let details = state
        .accounts
        .get_account(&account.account_id)
        .map_err(|error| {
            eprintln!("Usage lookup failed: {}", error);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not retrieve usage details".into(),
            )
        })?
        .ok_or((StatusCode::NOT_FOUND, "Account not found".into()))?;

    let subscription = details.subscription.map(|sub| UsageSubscription {
        id: sub.id,
        status: sub.status,
        character_limit: sub.character_limit,
        characters_used: sub.characters_used,
        characters_remaining: (sub.character_limit - sub.characters_used).max(0),
        stt_limit_ms: sub.stt_limit_ms,
        stt_used_ms: sub.stt_used_ms,
        stt_remaining_ms: (sub.stt_limit_ms - sub.stt_used_ms).max(0),
        starts_at: sub.starts_at,
        expires_at: sub.expires_at,
    });

    Ok(Json(UsageResponse {
        account_id: account.account_id,
        account_status: account.status,
        subscription,
    }))
}

fn audio_format(format: &str) -> Option<(&'static str, &'static str, &'static [&'static str])> {
    match format {
        "wav" => Some(("audio/wav", "wav", &[])),
        "mp3" => Some((
            "audio/mpeg",
            "mp3",
            &["-c:a", "libmp3lame", "-q:a", "4", "-f", "mp3", "pipe:1"],
        )),
        "ogg" => Some((
            "audio/ogg",
            "ogg",
            &["-c:a", "libvorbis", "-q:a", "4", "-f", "ogg", "pipe:1"],
        )),
        "flac" => Some((
            "audio/flac",
            "flac",
            &["-c:a", "flac", "-f", "flac", "pipe:1"],
        )),
        "aac" => Some((
            "audio/aac",
            "aac",
            &["-c:a", "aac", "-b:a", "128k", "-f", "adts", "pipe:1"],
        )),
        _ => None,
    }
}

async fn convert_audio(wav: Vec<u8>, format: &str) -> Result<Vec<u8>, String> {
    let (_, _, encoder_args) =
        audio_format(format).ok_or_else(|| "Unsupported audio format".to_string())?;

    let mut child = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "wav",
            "-i",
            "pipe:0",
            "-vn",
        ])
        .args(encoder_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Could not start FFmpeg: {}", e))?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "Could not open FFmpeg input".to_string())?;

    // Write the WAV input concurrently so FFmpeg's output
    // pipes can be drained without blocking the conversion.
    let writer = tokio::spawn(async move { stdin.write_all(&wav).await });

    let output = child
        .wait_with_output()
        .await
        .map_err(|e| format!("FFmpeg execution failed: {}", e))?;

    writer
        .await
        .map_err(|e| format!("FFmpeg input task failed: {}", e))?
        .map_err(|e| format!("Could not send audio to FFmpeg: {}", e))?;

    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        return Err(format!("FFmpeg conversion failed: {}", error));
    }

    if output.stdout.is_empty() {
        return Err("FFmpeg produced empty audio output".into());
    }

    Ok(output.stdout)
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

async fn voices(State(state): State<Arc<AppState>>) -> Json<VoicesResponse> {
    Json(VoicesResponse {
        voices: state.voices.clone(),
    })
}

fn create_wav(samples: &[f32]) -> Vec<u8> {
    let mut wav = Vec::with_capacity(44 + samples.len() * 2);

    let sample_rate = 24000u32;
    let channels = 1u16;
    let bits_per_sample = 16u16;
    let data_size = (samples.len() * 2) as u32;

    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_size).to_le_bytes());
    wav.extend_from_slice(b"WAVE");

    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&channels.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&bits_per_sample.to_le_bytes());

    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_size.to_le_bytes());

    for sample in samples {
        let value = (sample.clamp(-1.0, 1.0) * 32767.0) as i16;
        wav.extend_from_slice(&value.to_le_bytes());
    }

    wav
}

async fn transcribe(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Result<Response, (StatusCode, String)> {
    // Preserve API-key authentication for external integrations.
    let account = authenticate_request(&state, &headers)?;
    transcribe_for_account(State(state), account, multipart).await
}

async fn customer_browser_transcribe(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Result<Response, (StatusCode, String)> {
    if !browser_origin_valid(&headers) {
        return Err((
            StatusCode::FORBIDDEN,
            "Invalid browser request origin".into(),
        ));
    }

    let account = authenticate_customer_browser_request(&state, &headers).await?;
    transcribe_for_account(State(state), account, multipart).await
}

async fn transcribe_for_account(
    State(state): State<Arc<AppState>>,
    account: AuthenticatedAccount,
    mut multipart: Multipart,
) -> Result<Response, (StatusCode, String)> {
    // Authenticate before accepting an expensive transcription request.

    if account.status != "active" {
        return Err((
            StatusCode::FORBIDDEN,
            format!("Account is not active: {}", account.status),
        ));
    }

    // Keep STT concurrency independent of TTS.
    let _permit = state.stt_slot.clone().try_acquire_owned().map_err(|_| {
        (
            StatusCode::TOO_MANY_REQUESTS,
            "Another STT request is currently running".into(),
        )
    })?;

    let mut audio_bytes: Option<Vec<u8>> = None;
    let mut filename = "audio.bin".to_string();
    let mut language = "auto".to_string();
    let mut task = "transcribe".to_string();

    while let Some(field) = multipart.next_field().await.map_err(|error| {
        eprintln!("STT multipart parsing failed: {}", error);
        (
            StatusCode::BAD_REQUEST,
            "Could not parse multipart upload".into(),
        )
    })? {
        let name = field.name().unwrap_or("").to_string();

        match name.as_str() {
            "file" => {
                if audio_bytes.is_some() {
                    return Err((
                        StatusCode::BAD_REQUEST,
                        "Only one audio file may be uploaded".into(),
                    ));
                }

                if let Some(original_name) = field.file_name() {
                    let extension = original_name.rsplit('.').next().filter(|ext| {
                        !ext.is_empty()
                            && ext.len() <= 10
                            && ext.chars().all(|c| c.is_ascii_alphanumeric())
                    });

                    if let Some(extension) = extension {
                        filename = format!("audio.{}", extension.to_ascii_lowercase());
                    }
                }

                let bytes = field.bytes().await.map_err(|error| {
                    eprintln!("STT file read failed: {}", error);
                    (
                        StatusCode::BAD_REQUEST,
                        "Could not read uploaded audio".into(),
                    )
                })?;

                if bytes.is_empty() {
                    return Err((StatusCode::BAD_REQUEST, "Audio file cannot be empty".into()));
                }

                if bytes.len() > STT_MAX_UPLOAD_BYTES {
                    return Err((
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "Audio upload exceeds the 25 MiB limit".into(),
                    ));
                }

                audio_bytes = Some(bytes.to_vec());
            }

            "language" => {
                let value = field
                    .text()
                    .await
                    .map_err(|_| (StatusCode::BAD_REQUEST, "Invalid language field".into()))?;

                language = value.trim().to_ascii_lowercase();

                if language.is_empty()
                    || language.len() > 32
                    || !language
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-')
                {
                    return Err((
                        StatusCode::BAD_REQUEST,
                        "Language must be 'auto' or a valid language code".into(),
                    ));
                }
            }

            "task" => {
                let value = field
                    .text()
                    .await
                    .map_err(|_| (StatusCode::BAD_REQUEST, "Invalid task field".into()))?;

                task = value.trim().to_ascii_lowercase();

                if task != "transcribe" && task != "translate" {
                    return Err((
                        StatusCode::BAD_REQUEST,
                        "Task must be transcribe or translate".into(),
                    ));
                }
            }

            _ => {
                // Ignore unrecognized multipart fields.
            }
        }
    }

    let audio_bytes = audio_bytes.ok_or((
        StatusCode::BAD_REQUEST,
        "Missing audio file field named 'file'".into(),
    ))?;

    // Create a unique temporary file without trusting the supplied filename.
    let extension = filename.rsplit('.').next().unwrap_or("bin");

    let temp_path = std::env::temp_dir().join(format!(
        "kokorov-stt-{}.{}",
        hex::encode(rand::random::<[u8; 16]>()),
        extension
    ));

    let mut temp_file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)
        .await
        .map_err(|error| {
            eprintln!("Could not create STT temporary file: {}", error);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not prepare audio for processing".into(),
            )
        })?;

    let _temporary_audio = TemporaryAudioFile(temp_path.clone());

    temp_file.write_all(&audio_bytes).await.map_err(|error| {
        eprintln!("Could not write STT temporary file: {}", error);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not prepare audio for processing".into(),
        )
    })?;

    temp_file.flush().await.map_err(|error| {
        eprintln!("Could not flush STT temporary file: {}", error);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not prepare audio for processing".into(),
        )
    })?;

    drop(temp_file);

    // Measure actual audio duration using FFprobe.
    let probe = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(&temp_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|error| {
            eprintln!("Could not run FFprobe: {}", error);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Audio validation is unavailable".into(),
            )
        })?;

    if !probe.status.success() {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "Uploaded file is not a supported audio file".into(),
        ));
    }

    let duration_text = std::str::from_utf8(&probe.stdout).map_err(|_| {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            "Could not determine audio duration".into(),
        )
    })?;

    let duration_seconds: f64 = duration_text.trim().parse().map_err(|_| {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            "Could not determine audio duration".into(),
        )
    })?;

    if !duration_seconds.is_finite() || duration_seconds <= 0.0 {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "Audio duration must be greater than zero".into(),
        ));
    }

    let audio_ms_float = (duration_seconds * 1000.0).ceil();

    if audio_ms_float > STT_MAX_DURATION_MS as f64 {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            "Audio duration exceeds the 30-minute limit".into(),
        ));
    }

    let audio_ms = audio_ms_float as i64;

    // Precheck subscription and quota before sending audio to Whisper.
    let details = state
        .accounts
        .get_account(&account.account_id)
        .map_err(|error| {
            eprintln!("STT account lookup failed: {}", error);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not verify STT subscription".into(),
            )
        })?
        .ok_or((StatusCode::NOT_FOUND, "Account not found".into()))?;

    if details.account.status != "active" {
        return Err((StatusCode::FORBIDDEN, "Account is not active".into()));
    }

    let subscription = details
        .subscription
        .ok_or((StatusCode::FORBIDDEN, "No subscription is available".into()))?;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    if subscription.status != "active"
        || subscription.starts_at > now
        || subscription.expires_at <= now
    {
        return Err((
            StatusCode::FORBIDDEN,
            "No active subscription is available".into(),
        ));
    }

    if subscription.stt_limit_ms <= 0 {
        return Err((
            StatusCode::FORBIDDEN,
            "STT is not enabled for this subscription".into(),
        ));
    }

    let projected_usage = subscription
        .stt_used_ms
        .checked_add(audio_ms)
        .ok_or((StatusCode::FORBIDDEN, "STT usage total overflow".into()))?;

    if projected_usage > subscription.stt_limit_ms {
        return Err((
            StatusCode::FORBIDDEN,
            format!(
                "STT quota exhausted. Remaining: {} ms, requested: {} ms.",
                (subscription.stt_limit_ms - subscription.stt_used_ms).max(0),
                audio_ms
            ),
        ));
    }

    // Forward the upload to the private Whisper service.
    let part = reqwest::multipart::Part::bytes(audio_bytes)
        .file_name(filename)
        .mime_str("application/octet-stream")
        .map_err(|error| {
            eprintln!("Could not prepare Whisper upload: {}", error);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not prepare audio for transcription".into(),
            )
        })?;

    let mut form = reqwest::multipart::Form::new()
        .part("file", part)
        .text("temperature", "0.0")
        .text("temperature_inc", "0.2")
        .text("response_format", "verbose_json")
        .text("language", language);

    if task == "translate" {
        form = form.text("translate", "true");
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(35 * 60))
        .build()
        .map_err(|error| {
            eprintln!("Could not build Whisper HTTP client: {}", error);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "STT service client could not be initialized".into(),
            )
        })?;

    let started = Instant::now();

    let whisper_response = client
        .post(WHISPER_URL)
        .multipart(form)
        .send()
        .await
        .map_err(|error| {
            eprintln!("Whisper request failed: {}", error);
            (StatusCode::BAD_GATEWAY, "STT service is unavailable".into())
        })?;

    let whisper_status = whisper_response.status();
    let response_bytes = whisper_response.bytes().await.map_err(|error| {
        eprintln!("Could not read Whisper response: {}", error);
        (
            StatusCode::BAD_GATEWAY,
            "Could not read STT service response".into(),
        )
    })?;

    if !whisper_status.is_success() {
        eprintln!(
            "Whisper returned HTTP {}: {}",
            whisper_status,
            String::from_utf8_lossy(&response_bytes)
        );

        return Err((
            StatusCode::BAD_GATEWAY,
            "STT service could not process the audio".into(),
        ));
    }

    let whisper_result: WhisperResponse =
        serde_json::from_slice(&response_bytes).map_err(|error| {
            eprintln!("Invalid Whisper JSON response: {}", error);
            (
                StatusCode::BAD_GATEWAY,
                "STT service returned an invalid response".into(),
            )
        })?;

    let actual_task = whisper_result.task.as_deref().unwrap_or("");

    if actual_task != task {
        eprintln!(
            "Whisper task mismatch: requested {}, received {}",
            task, actual_task
        );

        return Err((
            StatusCode::BAD_GATEWAY,
            "STT service performed an unexpected task".into(),
        ));
    }

    let detected_language = whisper_result.language.as_deref().unwrap_or("").trim();

    if detected_language.is_empty() || detected_language.len() > 32 {
        return Err((
            StatusCode::BAD_GATEWAY,
            "STT service did not return a valid language".into(),
        ));
    }

    if whisper_result.text.is_none() {
        return Err((
            StatusCode::BAD_GATEWAY,
            "STT service did not return transcription text".into(),
        ));
    }

    // Whisper reports duration rounded to approximately one decimal place.
    // FFprobe remains the authoritative value for quota charging.
    let whisper_duration = whisper_result.duration.ok_or((
        StatusCode::BAD_GATEWAY,
        "STT service did not return audio duration".into(),
    ))?;

    if !whisper_duration.is_finite()
        || whisper_duration <= 0.0
        || (whisper_duration - duration_seconds).abs() > 0.25
    {
        eprintln!(
            "Whisper duration mismatch: ffprobe={}s, whisper={}s",
            duration_seconds, whisper_duration
        );

        return Err((
            StatusCode::BAD_GATEWAY,
            "STT service returned an inconsistent audio duration".into(),
        ));
    }

    let processing_ms = started.elapsed().as_millis().min(i64::MAX as u128) as i64;

    // Record only successful transcription/translation results.
    state
        .accounts
        .record_stt_usage(
            &account.account_id,
            audio_ms,
            detected_language,
            task.as_str(),
            processing_ms,
        )
        .map_err(|error| {
            let message = error.to_string();

            if message.to_lowercase().contains("quota")
                || message.to_lowercase().contains("subscription")
                || message.to_lowercase().contains("account")
            {
                (StatusCode::FORBIDDEN, message)
            } else {
                eprintln!("STT usage recording failed: {}", message);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Could not record STT usage".into(),
                )
            }
        })?;

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-audio-duration-ms", audio_ms.to_string())
        .header("x-stt-processing-ms", processing_ms.to_string())
        .header("x-stt-task", task)
        .body(axum::body::Body::from(response_bytes))
        .map_err(|error| {
            eprintln!("Could not build STT response: {}", error);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not prepare STT response".into(),
            )
        })
}

async fn synthesize(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<TtsRequest>,
) -> Result<Response, (StatusCode, String)> {
    // Preserve API-key authentication for external integrations.
    let account = authenticate_request(&state, &headers)?;
    synthesize_for_account(State(state), account, Json(request)).await
}

async fn customer_browser_synthesize(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<TtsRequest>,
) -> Result<Response, (StatusCode, String)> {
    if !browser_origin_valid(&headers) {
        return Err((
            StatusCode::FORBIDDEN,
            "Invalid browser request origin".into(),
        ));
    }

    let account = authenticate_customer_browser_request(&state, &headers).await?;
    synthesize_for_account(State(state), account, Json(request)).await
}

async fn synthesize_for_account(
    State(state): State<Arc<AppState>>,
    account: AuthenticatedAccount,
    Json(request): Json<TtsRequest>,
) -> Result<Response, (StatusCode, String)> {
    let text = request.text.trim();

    if text.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "Text cannot be empty".into()));
    }

    if text.chars().count() > 2000 {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            "Text cannot exceed 2000 characters".into(),
        ));
    }

    if account.status != "active" {
        return Err((
            StatusCode::FORBIDDEN,
            format!("Account is not active: {}", account.status),
        ));
    }

    let character_count = text.chars().count() as i64;

    let voice = request.voice.unwrap_or_else(|| "af_heart".to_string());

    if !state.voices.contains(&voice) {
        return Err((StatusCode::BAD_REQUEST, format!("Unknown voice: {}", voice)));
    }

    let speed = request.speed.unwrap_or(1.0);

    if !speed.is_finite() || !(0.5..=2.0).contains(&speed) {
        return Err((
            StatusCode::BAD_REQUEST,
            "Speed must be between 0.5 and 2.0".into(),
        ));
    }

    let format = request
        .format
        .unwrap_or_else(|| "wav".to_string())
        .to_ascii_lowercase();

    let (content_type, extension, _) = audio_format(&format).ok_or((
        StatusCode::BAD_REQUEST,
        "Unsupported format. Use wav, mp3, ogg, flac or aac.".into(),
    ))?;

    let _permit = state
        .synthesis_slot
        .clone()
        .try_acquire_owned()
        .map_err(|_| {
            (
                StatusCode::TOO_MANY_REQUESTS,
                "Another synthesis request is currently running".into(),
            )
        })?;

    state
        .accounts
        .check_quota(&account.account_id, character_count)
        .map_err(|error| {
            let message = error.to_string();

            if message.to_lowercase().contains("quota")
                || message.to_lowercase().contains("subscription")
            {
                (StatusCode::FORBIDDEN, message)
            } else {
                eprintln!("Quota check failed: {}", message);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Could not verify character quota".into(),
                )
            }
        })?;

    let (samples, duration) = state
        .tts
        .synth(text, Voice::new(voice.clone()).with_speed(speed))
        .await
        .map_err(|error| (StatusCode::UNPROCESSABLE_ENTITY, error.to_string()))?;

    let wav = create_wav(&samples);

    let audio = if format == "wav" {
        wav
    } else {
        convert_audio(wav, &format)
            .await
            .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error))?
    };

    let filename = format!("kokoro-speech.{}", extension);

    let response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}\"", filename),
        )
        .header(header::CONTENT_LENGTH, audio.len().to_string())
        .header("x-synthesis-ms", duration.as_millis().to_string())
        .header("x-audio-format", &format)
        .body(axum::body::Body::from(audio))
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;

    state
        .accounts
        .record_usage(
            &account.account_id,
            character_count,
            &format,
            &voice,
            speed as f64,
            duration.as_millis() as i64,
        )
        .map_err(|error| {
            let message = error.to_string();

            if message.to_lowercase().contains("quota")
                || message.to_lowercase().contains("subscription")
                || message.to_lowercase().contains("account")
            {
                (StatusCode::FORBIDDEN, message)
            } else {
                eprintln!("Usage recording failed: {}", message);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Could not record synthesis usage".into(),
                )
            }
        })?;

    Ok(response)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    println!("Loading Kokoro model...");

    let tts = KokoroTts::new("models/model_quantized.onnx", "voices").await?;

    println!("Kokoro model loaded!");

    let mut available_voices = Vec::new();

    for entry in fs::read_dir("voices")? {
        let entry = entry?;
        let path = entry.path();

        if path.extension().and_then(|ext| ext.to_str()) == Some("bin") {
            if let Some(name) = path.file_stem().and_then(|n| n.to_str()) {
                available_voices.push(name.to_string());
            }
        }
    }

    if available_voices.is_empty() {
        return Err("No voice files found in voices/".into());
    }

    println!("Available voices: {:?}", available_voices);

    let db_path = std::env::var("KOKORO_DB_PATH").unwrap_or_else(|_| "data/kokoro.db".to_string());

    let accounts = AccountStore::new(&db_path)?;
    println!("Account database initialized at {}", db_path);

    let state = Arc::new(AppState {
        tts: Arc::new(tts),
        voices: available_voices,
        synthesis_slot: Arc::new(Semaphore::new(1)),
        stt_slot: Arc::new(Semaphore::new(1)),
        accounts,
    });

    let app = Router::new()
        .route("/health", get(health))
        .route("/voices", get(voices))
        .route("/auth/customer/login", post(customer_browser_login))
        .route("/auth/customer/me", get(customer_browser_me))
        .route(
            "/auth/customer/password",
            post(customer_browser_change_password),
        )
        .route("/auth/customer/logout", post(customer_browser_logout))
        .route("/app/customer/account", get(customer_browser_account))
        .route("/app/customer/usage", get(customer_browser_usage))
        .route("/app/customer/tts", post(customer_browser_synthesize))
        .route(
            "/app/customer/stt",
            post(customer_browser_transcribe).layer(DefaultBodyLimit::max(STT_MAX_UPLOAD_BYTES)),
        )
        .route("/auth/admin/login", post(admin_browser_login))
        .route("/auth/admin/me", get(admin_browser_me))
        .route("/auth/admin/logout", post(admin_browser_logout))
        .route("/app/admin/accounts", get(admin_browser_accounts))
        .route(
            "/app/admin/accounts/{account_id}",
            get(admin_browser_account_details),
        )
        .route(
            "/app/admin/accounts/{account_id}/activate",
            post(admin_browser_activate),
        )
        .route(
            "/app/admin/accounts/{account_id}/extend",
            post(admin_browser_extend),
        )
        .route(
            "/app/admin/accounts/{account_id}/suspend",
            post(admin_browser_suspend),
        )
        .route(
            "/app/admin/accounts/{account_id}/unsuspend",
            post(admin_browser_unsuspend),
        )
        .route(
            "/app/admin/accounts/{account_id}/keys/rotate",
            post(admin_browser_rotate_key),
        )
        .route(
            "/app/admin/accounts/{account_id}/keys/revoke",
            post(admin_browser_revoke_keys),
        )
        .route("/v1/register", post(register))
        .route("/v1/account", get(account_details))
        .route("/v1/usage", get(usage_details))
        .route(
            "/v1/stt",
            post(transcribe).layer(DefaultBodyLimit::max(STT_MAX_UPLOAD_BYTES)),
        )
        .route("/v1/tts", post(synthesize))
        // Compatibility alias: this route is authenticated too.
        .route("/tts", post(synthesize))
        .with_state(state);

    let address: SocketAddr = std::env::var("KOKORO_API_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:8080".to_string())
        .parse()?;

    let listener = tokio::net::TcpListener::bind(address).await?;

    println!("Kokoro API listening on {}", address);

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c().await.ok();
        })
        .await?;

    Ok(())
}
