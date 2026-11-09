//! Oura ring sync: OAuth2 and HTTP against the Oura API v2. The network half
//! of the feature; `vitals_core::oura` parses, stores and rolls up
//! (INVARIANTS #7).
//!
//! Opt-in: with no Oura config, nothing here runs and vitals makes no network
//! calls. Secrets come from env or 0600 files, and the token file is written
//! by vitals itself (INVARIANTS #6).
//!
//! Oura refresh tokens work **once**: each refresh returns a new pair and
//! kills the old refresh token. So the new pair is written to disk (atomic
//! rename, 0600) before the new access token is used, and every token use
//! happens under an exclusive lock file, so the server's background sync and
//! a manual `vitals oura sync` never refresh at the same moment.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chrono::{Duration, Local, NaiveDate};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Instant;
use vitals_core::oura::{self as core_oura, ApplyCounts, SyncEntry, KINDS};
use vitals_core::time::{fmt_date, parse_date};
use vitals_core::Store;

const AUTHORIZE_URL: &str = "https://cloud.ouraring.com/oauth/authorize";
const DEFAULT_API_BASE: &str = "https://api.ouraring.com";
const SCOPES: &str = "email personal daily heartrate workout tag session spo2 heart_health";
/// A first sync asks for everything from here on: before any Oura ring
/// that has an API.
const FIRST_DAY: &str = "2015-01-01";
/// Backfill requests span at most this many days each.
const CHUNK_DAYS: i64 = 365;
/// Refresh when the access token has less than this left.
const REFRESH_MARGIN_SECS: i64 = 300;

/// Oura settings, from the environment.
#[derive(Clone, Debug)]
pub struct Config {
    pub client_id: String,
    client_secret: String,
    pub redirect_uri: String,
    pub tokens_file: PathBuf,
    /// `https://api.ouraring.com`, or a test server.
    pub api_base: String,
    /// Read from Oura's sandbox (fake data) instead of the account's.
    pub sandbox: bool,
}

impl Config {
    /// `Ok(None)` when Oura isn't configured: neither `OURA_CLIENT_ID` nor
    /// `OURA_CLIENT_ID_FILE` is set. A partial config, or a `_FILE` that
    /// can't be read, is an error, never a silent "off".
    ///
    /// - `OURA_CLIENT_ID[_FILE]`, `OURA_CLIENT_SECRET[_FILE]`: the app's
    ///   credentials (prefer the 0600 `_FILE` forms).
    /// - `OURA_REDIRECT_URI`: exactly as registered with Oura.
    /// - `OURA_TOKENS_FILE`: default `oura-tokens.json` next to the DB.
    /// - `OURA_API_BASE`, `OURA_SANDBOX=1`: for testing.
    pub fn from_env(db_path: &str) -> Result<Option<Config>, String> {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let secret = |k: &str| -> Result<Option<String>, String> {
            if let Some(v) = env(k) {
                return Ok(Some(v));
            }
            match env(&format!("{k}_FILE")) {
                None => Ok(None),
                Some(p) => vitals_core::secrets::read_secret_file(&p)
                    .map(Some)
                    .map_err(|e| format!("{k}_FILE: {e}")),
            }
        };
        let Some(client_id) = secret("OURA_CLIENT_ID")? else {
            return Ok(None);
        };
        let client_secret = secret("OURA_CLIENT_SECRET")?
            .ok_or("Oura is configured but OURA_CLIENT_SECRET_FILE is not set")?;
        let redirect_uri = env("OURA_REDIRECT_URI").ok_or(
            "Oura is configured but OURA_REDIRECT_URI is not set (the exact URI registered with Oura)",
        )?;
        let tokens_file = env("OURA_TOKENS_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                Path::new(db_path)
                    .parent()
                    .unwrap_or(Path::new("."))
                    .join("oura-tokens.json")
            });
        Ok(Some(Config {
            client_id,
            client_secret,
            redirect_uri,
            tokens_file,
            api_base: env("OURA_API_BASE")
                .unwrap_or_else(|| DEFAULT_API_BASE.into())
                .trim_end_matches('/')
                .to_string(),
            sandbox: env("OURA_SANDBOX").is_some_and(|v| v == "1"),
        }))
    }

    fn token_url(&self) -> String {
        format!("{}/oauth/token", self.api_base)
    }

    fn collection_url(&self, kind: &str) -> String {
        let sandbox = if self.sandbox { "sandbox/" } else { "" };
        format!("{}/v2/{sandbox}usercollection/{kind}", self.api_base)
    }
}

fn agent() -> ureq::Agent {
    let ua = format!(
        "vitals/{} (personal Oura sync)",
        vitals_core::build::VERSION.unwrap_or("dev")
    );
    ureq::AgentBuilder::new()
        .user_agent(&ua)
        .timeout(std::time::Duration::from_secs(60))
        .build()
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

// ── tokens ────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
struct Tokens {
    access_token: String,
    refresh_token: String,
    /// Epoch seconds.
    expires_at: i64,
}

/// What Oura's token endpoint returns.
#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: Option<i64>,
}

impl From<TokenResponse> for Tokens {
    fn from(t: TokenResponse) -> Tokens {
        Tokens {
            access_token: t.access_token,
            refresh_token: t.refresh_token,
            // Lifetimes aren't documented; without one, refresh within the
            // hour rather than trust it forever.
            expires_at: now() + t.expires_in.unwrap_or(3600),
        }
    }
}

fn load_tokens(path: &Path) -> Result<Option<Tokens>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let text = vitals_core::secrets::read_secret_file(&path.to_string_lossy())?;
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|e| format!("{}: not an Oura token file: {e}", path.display()))
}

fn save_tokens(path: &Path, t: &Tokens) -> Result<(), String> {
    let json = serde_json::to_string(t).map_err(|e| e.to_string())?;
    vitals_core::secrets::replace_secret_file(path, &json)
}

/// Exclusive lock around every token read-refresh-write. Released on drop.
struct TokenLock {
    _file: std::fs::File,
}

fn lock(cfg: &Config) -> Result<TokenLock, String> {
    let mut p = cfg.tokens_file.clone().into_os_string();
    p.push(".lock");
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&p)
        .map_err(|e| format!("open {}: {e}", PathBuf::from(&p).display()))?;
    f.lock()
        .map_err(|e| format!("lock {}: {e}", PathBuf::from(&p).display()))?;
    Ok(TokenLock { _file: f })
}

/// POST the token endpoint. Errors never include the request body.
fn token_request(cfg: &Config, form: &[(&str, &str)]) -> Result<Tokens, String> {
    let mut all = vec![
        ("client_id", cfg.client_id.as_str()),
        ("client_secret", cfg.client_secret.as_str()),
    ];
    all.extend_from_slice(form);
    match agent().post(&cfg.token_url()).send_form(&all) {
        Ok(r) => r
            .into_string()
            .map_err(|e| e.to_string())
            .and_then(|b| serde_json::from_str::<TokenResponse>(&b).map_err(|e| e.to_string()))
            .map(Tokens::from)
            .map_err(|e| format!("Oura token response: {e}")),
        Err(ureq::Error::Status(code, r)) => {
            // Oura's error JSON names the problem (invalid_grant, …) and
            // holds no secret.
            let body: serde_json::Value = r
                .into_string()
                .ok()
                .and_then(|b| serde_json::from_str(&b).ok())
                .unwrap_or_default();
            let what = body
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("no detail");
            Err(format!("Oura token endpoint: HTTP {code}: {what}"))
        }
        Err(e) => Err(format!("Oura token endpoint: {e}")),
    }
}

/// Refresh and persist the new pair before returning it. A failed refresh
/// leaves the file as it was.
fn refresh(cfg: &Config, old: &Tokens) -> Result<Tokens, String> {
    let t = token_request(
        cfg,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", old.refresh_token.as_str()),
        ],
    )
    .map_err(|e| format!("{e}; run `vitals oura auth` to reconnect"))?;
    save_tokens(&cfg.tokens_file, &t)?;
    Ok(t)
}

// ── authorization ─────────────────────────────────────────────────────────

/// A started authorization: the URL to open, plus the `state` and PKCE
/// verifier that only this process knows.
pub struct AuthStart {
    pub url: String,
    state: String,
    verifier: String,
}

fn random_token(bytes: usize) -> Result<String, String> {
    let mut buf = vec![0u8; bytes];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .map_err(|e| format!("read /dev/urandom: {e}"))?;
    Ok(URL_SAFE_NO_PAD.encode(buf))
}

fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn dec(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Start an authorization-code grant with `state` and PKCE (S256).
pub fn begin_auth(cfg: &Config) -> Result<AuthStart, String> {
    let state = random_token(24)?;
    let verifier = random_token(48)?;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let url = format!(
        "{AUTHORIZE_URL}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}\
         &code_challenge={}&code_challenge_method=S256",
        enc(&cfg.client_id),
        enc(&cfg.redirect_uri),
        enc(SCOPES),
        enc(&state),
        enc(&challenge),
    );
    Ok(AuthStart {
        url,
        state,
        verifier,
    })
}

/// The query parameters of a pasted redirect URL (or a bare query string).
fn query_params(pasted: &str) -> HashMap<String, String> {
    let q = pasted.trim();
    let q = q.split_once('?').map_or(q, |(_, q)| q);
    let q = q.split('#').next().unwrap_or("");
    q.split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (dec(k), dec(v)))
        .collect()
}

/// Finish the grant from the URL the browser landed on: check `state`,
/// exchange the code, and write the token file.
pub fn finish_auth(cfg: &Config, start: &AuthStart, pasted: &str) -> Result<(), String> {
    let p = query_params(pasted);
    if let Some(err) = p.get("error") {
        return Err(format!("Oura declined the authorization: {err}"));
    }
    if p.get("state") != Some(&start.state) {
        return Err(
            "the pasted URL's state doesn't match this run; start `vitals oura auth` again".into(),
        );
    }
    let code = p.get("code").ok_or("the pasted URL has no `code`")?;
    let _guard = lock(cfg)?;
    let t = token_request(
        cfg,
        &[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", cfg.redirect_uri.as_str()),
            ("code_verifier", start.verifier.as_str()),
        ],
    )?;
    save_tokens(&cfg.tokens_file, &t)
}

/// Whether a token file exists (vitals has been authorized).
pub fn is_connected(cfg: &Config) -> bool {
    cfg.tokens_file.exists()
}

// ── fetching ──────────────────────────────────────────────────────────────

/// Holds the current tokens during one sync, refreshing as needed.
struct Session<'a> {
    cfg: &'a Config,
    tokens: Tokens,
    http: ureq::Agent,
}

impl Session<'_> {
    /// One page; refreshes once on 401 and waits out a 429.
    fn page(
        &mut self,
        kind: &str,
        start: NaiveDate,
        end: NaiveDate,
        next: Option<&str>,
    ) -> Result<String, String> {
        let url = self.cfg.collection_url(kind);
        let mut refreshed = false;
        let mut waits = 0;
        loop {
            if self.tokens.expires_at - now() < REFRESH_MARGIN_SECS {
                self.tokens = refresh(self.cfg, &self.tokens)?;
                refreshed = true;
            }
            let mut req = self
                .http
                .get(&url)
                .set(
                    "authorization",
                    &format!("Bearer {}", self.tokens.access_token),
                )
                .query("start_date", &fmt_date(start))
                .query("end_date", &fmt_date(end));
            if let Some(t) = next {
                req = req.query("next_token", t);
            }
            match req.call() {
                Ok(r) => {
                    return r
                        .into_string()
                        .map_err(|e| format!("oura {kind}: read: {e}"))
                }
                Err(ureq::Error::Status(401, _)) if !refreshed => {
                    self.tokens = refresh(self.cfg, &self.tokens)?;
                    refreshed = true;
                }
                Err(ureq::Error::Status(429, r)) if waits < 3 => {
                    let secs = r
                        .header("retry-after")
                        .and_then(|v| v.trim().parse::<u64>().ok())
                        .unwrap_or(30)
                        .min(120);
                    std::thread::sleep(std::time::Duration::from_secs(secs));
                    waits += 1;
                }
                Err(ureq::Error::Status(403, _)) => {
                    return Err(format!(
                        "oura {kind}: HTTP 403: Oura refused access (membership lapsed, \
                         or the authorization lacks this scope)"
                    ))
                }
                Err(ureq::Error::Status(code, _)) => {
                    return Err(format!("oura {kind}: HTTP {code}"))
                }
                Err(e) => return Err(format!("oura {kind}: {e}")),
            }
        }
    }

    /// Every document of `kind` with a day in `start..=end`, deduplicated by
    /// id (backfill chunks overlap by a day).
    fn fetch(
        &mut self,
        kind: &str,
        start: NaiveDate,
        end: NaiveDate,
    ) -> Result<Vec<core_oura::OuraDoc>, String> {
        let mut by_id: HashMap<String, core_oura::OuraDoc> = HashMap::new();
        let mut from = start;
        loop {
            let to = (from + Duration::days(CHUNK_DAYS)).min(end);
            let mut next: Option<String> = None;
            loop {
                let body = self.page(kind, from, to, next.as_deref())?;
                let page = core_oura::parse_page(kind, &body)?;
                for d in page.docs {
                    by_id.insert(d.id.clone(), d);
                }
                match page.next_token {
                    Some(t) if Some(&t) != next.as_ref() => next = Some(t),
                    _ => break,
                }
            }
            if to >= end {
                break;
            }
            from = to;
        }
        let mut docs: Vec<_> = by_id.into_values().collect();
        docs.sort_by(|a, b| (a.day, &a.id).cmp(&(b.day, &b.id)));
        Ok(docs)
    }
}

/// What one sync did.
#[derive(Debug, Default, Serialize)]
pub struct SyncReport {
    /// Counts per collection.
    pub kinds: Vec<(String, ApplyCounts)>,
    pub added: usize,
    pub updated: usize,
    pub removed: usize,
    pub elapsed_ms: i64,
}

/// Sync every collection into `store`, and log the outcome in
/// `oura_sync_log` (failures too, so `status` shows them). `since`
/// re-fetches from that day; otherwise each collection picks up a week
/// before its last stored day, and a first sync takes all history.
pub fn sync(cfg: &Config, store: &Store, since: Option<NaiveDate>) -> Result<SyncReport, String> {
    let started = Instant::now();
    let ts = now();
    let result = sync_inner(cfg, store, since);
    let elapsed_ms = started.elapsed().as_millis() as i64;
    let entry = match &result {
        Ok(r) => SyncEntry {
            ts,
            since: since.map(fmt_date),
            added: r.added as i64,
            updated: r.updated as i64,
            removed: r.removed as i64,
            error: None,
            elapsed_ms,
        },
        Err(e) => SyncEntry {
            ts,
            since: since.map(fmt_date),
            added: 0,
            updated: 0,
            removed: 0,
            error: Some(e.clone()),
            elapsed_ms,
        },
    };
    core_oura::log_sync(store, &entry)?;
    result.map(|mut r| {
        r.elapsed_ms = elapsed_ms;
        r
    })
}

fn sync_inner(cfg: &Config, store: &Store, since: Option<NaiveDate>) -> Result<SyncReport, String> {
    let _guard = lock(cfg)?;
    let tokens =
        load_tokens(&cfg.tokens_file)?.ok_or("Oura isn't connected yet: run `vitals oura auth`")?;
    let mut s = Session {
        cfg,
        tokens,
        http: agent(),
    };
    let today = Local::now().date_naive();
    let first = parse_date(FIRST_DAY).expect("FIRST_DAY is a date");
    let mut report = SyncReport::default();
    for kind in KINDS {
        let last = core_oura::last_day(store, kind)?;
        let (start, end, reconcile) = core_oura::window(since, last, first, today);
        // Ask through tomorrow, in case end_date is exclusive.
        let docs = s.fetch(kind, start, end + Duration::days(1))?;
        let c = core_oura::apply(store, kind, &docs, reconcile, now())?;
        report.added += c.added;
        report.updated += c.updated;
        report.removed += c.removed;
        report.kinds.push((kind.to_string(), c));
    }
    Ok(report)
}

#[cfg(test)]
mod tests;
