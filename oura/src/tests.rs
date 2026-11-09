//! Against a scripted local HTTP server standing in for Oura. Synthetic
//! tokens and documents only.

use super::*;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
struct Req {
    method: String,
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    body: String,
}

type Handler = dyn Fn(&Req) -> (u16, String) + Send + Sync;

/// Serve `handler` on a loopback port; returns the base URL and the log of
/// requests seen.
fn mock(handler: Box<Handler>) -> (String, Arc<Mutex<Vec<Req>>>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let log = Arc::new(Mutex::new(Vec::new()));
    let seen = log.clone();
    std::thread::spawn(move || {
        for stream in l.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut r = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if r.read_line(&mut line).unwrap_or(0) == 0 {
                continue;
            }
            let mut parts = line.split_whitespace();
            let method = parts.next().unwrap_or("").to_string();
            let target = parts.next().unwrap_or("").to_string();
            let mut headers = HashMap::new();
            loop {
                let mut h = String::new();
                r.read_line(&mut h).unwrap();
                let h = h.trim_end();
                if h.is_empty() {
                    break;
                }
                if let Some((k, v)) = h.split_once(':') {
                    headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
                }
            }
            let len: usize = headers
                .get("content-length")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let mut body = vec![0u8; len];
            r.read_exact(&mut body).unwrap();
            let (path, q) = target.split_once('?').unwrap_or((&target, ""));
            let req = Req {
                method,
                path: path.to_string(),
                query: query_params(&format!("?{q}")),
                headers,
                body: String::from_utf8_lossy(&body).into_owned(),
            };
            let (code, out) = handler(&req);
            seen.lock().unwrap().push(req);
            let _ = write!(
                stream,
                "HTTP/1.1 {code} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n{out}",
                out.len()
            );
        }
    });
    (base, log)
}

fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("vitals-oura-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn config(base: &str, dir: &Path) -> Config {
    Config {
        client_id: "cid".into(),
        client_secret: "csecret".into(),
        redirect_uri: "https://vitals.example/oura/callback".into(),
        tokens_file: dir.join("oura-tokens.json"),
        api_base: base.into(),
        sandbox: false,
    }
}

fn form(body: &str) -> HashMap<String, String> {
    query_params(&format!("?{body}"))
}

/// A fake Oura: refresh token r1 → (a2, r2), r2 → (a3, r3), anything else is
/// invalid_grant. Collections need a current access token; `daily_sleep`
/// has two pages.
fn fake_oura(valid_access: Arc<Mutex<String>>) -> Box<Handler> {
    Box::new(move |req| {
        if req.path == "/oauth/token" {
            assert_eq!(req.method, "POST");
            let f = form(&req.body);
            assert_eq!(f.get("client_secret").map(String::as_str), Some("csecret"));
            let next = match (
                f.get("grant_type").map(String::as_str),
                f.get("refresh_token").map(String::as_str),
            ) {
                (Some("refresh_token"), Some("r1")) => ("a2", "r2"),
                (Some("refresh_token"), Some("r2")) => ("a3", "r3"),
                (Some("authorization_code"), _) => {
                    assert_eq!(f.get("code").map(String::as_str), Some("the-code"));
                    assert!(f.get("code_verifier").is_some_and(|v| v.len() >= 43));
                    ("a1", "r1")
                }
                _ => return (400, r#"{"error":"invalid_grant"}"#.into()),
            };
            *valid_access.lock().unwrap() = next.0.into();
            return (
                200,
                format!(
                    r#"{{"access_token":"{}","refresh_token":"{}","expires_in":86400,"token_type":"bearer"}}"#,
                    next.0, next.1
                ),
            );
        }
        assert!(req
            .headers
            .get("user-agent")
            .is_some_and(|u| u.starts_with("vitals/")));
        let want = format!("Bearer {}", valid_access.lock().unwrap());
        if req.headers.get("authorization") != Some(&want) {
            return (401, "{}".into());
        }
        let kind = req.path.rsplit('/').next().unwrap();
        let today = fmt_date(Local::now().date_naive());
        match (kind, req.query.get("next_token").map(String::as_str)) {
            ("daily_sleep", None) => (
                200,
                format!(
                    r#"{{"data":[{{"id":"s1","day":"{today}","score":81}}],"next_token":"p2"}}"#
                ),
            ),
            ("daily_sleep", Some("p2")) => (
                200,
                r#"{"data":[{"id":"s0","day":"2015-06-01","score":70}],"next_token":null}"#.into(),
            ),
            _ => (200, r#"{"data":[],"next_token":null}"#.into()),
        }
    })
}

fn write_tokens(cfg: &Config, access: &str, refresh: &str, expires_at: i64) {
    save_tokens(
        &cfg.tokens_file,
        &Tokens {
            access_token: access.into(),
            refresh_token: refresh.into(),
            expires_at,
        },
    )
    .unwrap();
}

#[test]
fn expired_token_is_refreshed_and_persisted_before_use() {
    let valid = Arc::new(Mutex::new("nobody".to_string()));
    let (base, log) = mock(fake_oura(valid));
    let dir = tmpdir("refresh");
    let cfg = config(&base, &dir);
    write_tokens(&cfg, "a1", "r1", now() - 10);

    let store = Store::open_in_memory().unwrap();
    let r = sync(&cfg, &store, None).unwrap();
    assert_eq!(r.added, 2, "both pages of daily_sleep");

    let saved = load_tokens(&cfg.tokens_file).unwrap().unwrap();
    assert_eq!(
        (saved.access_token.as_str(), saved.refresh_token.as_str()),
        ("a2", "r2")
    );
    let log = log.lock().unwrap();
    assert_eq!(log[0].path, "/oauth/token", "refresh comes first");
    assert_eq!(log.iter().filter(|r| r.path == "/oauth/token").count(), 1);
    // A first sync backfills in chunks from FIRST_DAY.
    let readiness: Vec<_> = log
        .iter()
        .filter(|r| r.path.ends_with("/daily_readiness"))
        .collect();
    assert!(readiness.len() > 5);
    assert_eq!(
        readiness[0].query.get("start_date").map(String::as_str),
        Some(FIRST_DAY)
    );

    let st = core_oura::status(&store, 5).unwrap();
    assert_eq!(st.docs, 2);
    assert_eq!(st.syncs[0].error, None);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_401_refreshes_once_and_retries() {
    let valid = Arc::new(Mutex::new("a-new".to_string()));
    let (base, log) = mock(fake_oura(valid));
    let dir = tmpdir("401");
    let cfg = config(&base, &dir);
    // Looks fresh, but the server no longer accepts it.
    write_tokens(&cfg, "a-stale", "r1", now() + 86400);
    let store = Store::open_in_memory().unwrap();
    let since = Local::now().date_naive() - Duration::days(2);
    sync(&cfg, &store, Some(since)).unwrap();
    let log = log.lock().unwrap();
    assert_eq!(
        log[0].query.get("start_date"),
        Some(&fmt_date(since - Duration::days(5)))
    );
    assert_eq!(log[1].path, "/oauth/token");
    assert_eq!(log.iter().filter(|r| r.path == "/oauth/token").count(), 1);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_dead_refresh_token_fails_logs_and_keeps_the_file() {
    let valid = Arc::new(Mutex::new("nobody".to_string()));
    let (base, _) = mock(fake_oura(valid));
    let dir = tmpdir("dead");
    let cfg = config(&base, &dir);
    write_tokens(&cfg, "a1", "r-dead", now() - 10);
    let store = Store::open_in_memory().unwrap();
    let e = sync(&cfg, &store, None).unwrap_err();
    assert!(
        e.contains("invalid_grant") && e.contains("vitals oura auth"),
        "{e}"
    );
    let saved = load_tokens(&cfg.tokens_file).unwrap().unwrap();
    assert_eq!(saved.refresh_token, "r-dead");
    let st = core_oura::status(&store, 5).unwrap();
    assert!(st.syncs[0]
        .error
        .as_deref()
        .is_some_and(|e| e.contains("invalid_grant")));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn not_connected_says_so() {
    let dir = tmpdir("none");
    let cfg = config("http://127.0.0.1:9", &dir);
    let store = Store::open_in_memory().unwrap();
    let e = sync(&cfg, &store, None).unwrap_err();
    assert!(e.contains("vitals oura auth"), "{e}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn auth_checks_state_then_exchanges() {
    let valid = Arc::new(Mutex::new(String::new()));
    let (base, log) = mock(fake_oura(valid));
    let dir = tmpdir("auth");
    let cfg = config(&base, &dir);
    let start = begin_auth(&cfg).unwrap();
    assert!(start.url.starts_with(AUTHORIZE_URL));
    assert!(start.url.contains("code_challenge_method=S256"));
    assert!(start
        .url
        .contains("redirect_uri=https%3A%2F%2Fvitals.example%2Foura%2Fcallback"));

    let wrong = format!("{}?code=the-code&state=forged", cfg.redirect_uri);
    assert!(finish_auth(&cfg, &start, &wrong)
        .unwrap_err()
        .contains("state"));
    assert!(!is_connected(&cfg));
    assert!(log.lock().unwrap().is_empty(), "no exchange on a bad state");

    let denied = format!(
        "{}?error=access_denied&state={}",
        cfg.redirect_uri, start.state
    );
    assert!(finish_auth(&cfg, &start, &denied).is_err());

    let ok = format!(
        "  {}?code=the-code&state={}\n",
        cfg.redirect_uri,
        enc(&start.state)
    );
    finish_auth(&cfg, &start, &ok).unwrap();
    let saved = load_tokens(&cfg.tokens_file).unwrap().unwrap();
    assert_eq!(saved.refresh_token, "r1");
    let f = form(&log.lock().unwrap()[0].body);
    assert_eq!(f.get("redirect_uri"), Some(&cfg.redirect_uri));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn query_params_decode() {
    let p = query_params("https://h/x?code=a%2Fb+c&state=s#frag");
    assert_eq!(p.get("code").map(String::as_str), Some("a/b c"));
    assert_eq!(p.get("state").map(String::as_str), Some("s"));
    assert_eq!(dec("100%"), "100%");
}
