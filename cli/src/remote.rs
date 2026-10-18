//! HTTP client mode: run CLI subcommands against a running vitals-server
//! (`--server URL` / `VITALS_SERVER`), e.g. over the tailnet from another
//! machine or an agent. Every command maps to one API call and prints the
//! server's JSON verbatim — identical shapes to local `--json` output, so
//! consumers don't care which mode produced it.
//!
//! Auth: `VITALS_API_TOKEN` (if the server requires a bearer token). Env only —
//! never argv. The server's read-only token goes in the same variable: it is
//! just a bearer token with fewer rights, so read-only callers (agents,
//! scripts) get 403 on anything beyond plain reads (including raw GPS routes).

use crate::{date_range, opt_ts, parse_since, parse_until, Cmd};

pub struct Remote {
    base: String,
    token: Option<String>,
}

impl Remote {
    pub fn new(server: &str) -> Remote {
        Remote {
            base: format!("{}/api", server.trim_end_matches('/')),
            token: std::env::var("VITALS_API_TOKEN")
                .ok()
                .filter(|t| !t.is_empty()),
        }
    }

    pub fn get(&self, path: &str) -> Result<String, String> {
        let url = format!("{}{}", self.base, path);
        let mut req = ureq::get(&url);
        if let Some(t) = &self.token {
            req = req.set("authorization", &format!("Bearer {t}"));
        }
        match req.call() {
            Ok(resp) => resp.into_string().map_err(|e| format!("read body: {e}")),
            // 403 means the token is real but read-only: say so, rather than
            // leaving a bare status for an agent to guess at (the server sends
            // no body on this one).
            Err(ureq::Error::Status(403, _)) => Err(format!(
                "{url}: HTTP 403: read-only token — this needs the full VITALS_API_TOKEN \
                 (raw GPS routes are full-token only)"
            )),
            Err(ureq::Error::Status(code, resp)) => {
                let detail = resp.into_string().unwrap_or_default();
                Err(format!("{url}: HTTP {code}: {detail}"))
            }
            Err(e) => Err(format!("{url}: {e}")),
        }
    }
}

fn enc(s: &str) -> String {
    // Percent-encode the characters that matter in a query value.
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

/// `?k=v&…` from the pairs whose value is present.
fn qs(pairs: &[(&str, Option<String>)]) -> String {
    let parts: Vec<String> = pairs
        .iter()
        .filter_map(|(k, v)| v.as_ref().map(|v| format!("{k}={}", enc(v))))
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!("?{}", parts.join("&"))
    }
}

/// Dispatch a subcommand to the server; returns the response body to print.
pub fn dispatch(remote: &Remote, cmd: &Cmd) -> Result<String, String> {
    let ts = |s: &Option<String>, f: fn(&str) -> Result<i64, String>| {
        opt_ts(s, f).map(|v| v.map(|v| v.to_string()))
    };
    match cmd {
        Cmd::Import { .. } => {
            Err("import reads a LOCAL file into a LOCAL db; drop --server and run it on the server host".into())
        }
        Cmd::Status { limit } => remote.get(&format!("/status{}", qs(&[("limit", Some(limit.to_string()))]))),
        Cmd::Types => remote.get("/types"),
        Cmd::Records {
            type_,
            since,
            until,
            source,
            limit,
            offset,
        } => remote.get(&format!(
            "/records{}",
            qs(&[
                ("type", type_.clone()),
                ("since", ts(since, parse_since)?),
                ("until", ts(until, parse_until)?),
                ("source", source.clone()),
                ("limit", Some(limit.to_string())),
                ("offset", Some(offset.to_string())),
            ])
        )),
        Cmd::Series {
            type_,
            since,
            until,
            bucket,
            agg,
        } => remote.get(&format!(
            "/series{}",
            qs(&[
                ("type", Some(type_.clone())),
                ("since", ts(since, parse_since)?),
                ("until", ts(until, parse_until)?),
                ("bucket", Some(bucket.clone())),
                ("agg", agg.clone()),
            ])
        )),
        Cmd::Daily {
            since,
            until,
            metrics,
        } => {
            let r = date_range(since, until)?;
            remote.get(&format!(
                "/daily{}",
                qs(&[
                    ("since", r.since),
                    ("until", r.until),
                    ("metrics", (!metrics.is_empty()).then(|| metrics.join(","))),
                ])
            ))
        }
        Cmd::Sleep { since, until } => {
            let r = date_range(since, until)?;
            remote.get(&format!("/sleep{}", qs(&[("since", r.since), ("until", r.until)])))
        }
        Cmd::Activity { since, until } => {
            let r = date_range(since, until)?;
            remote.get(&format!("/activity{}", qs(&[("since", r.since), ("until", r.until)])))
        }
        Cmd::Workouts {
            activity,
            since,
            until,
            limit,
        } => remote.get(&format!(
            "/workouts{}",
            qs(&[
                ("activity", activity.clone()),
                ("since", ts(since, parse_since)?),
                ("until", ts(until, parse_until)?),
                ("limit", Some(limit.to_string())),
            ])
        )),
        Cmd::Workout { id } => remote.get(&format!("/workouts/{id}")),
        Cmd::Route { workout_id, .. } => remote.get(&format!("/workouts/{workout_id}/route")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_strings_skip_missing_and_encode() {
        assert_eq!(qs(&[("a", None)]), "");
        assert_eq!(
            qs(&[
                ("source", Some("Dan’s Watch".into())),
                ("x", None),
                ("n", Some("1".into()))
            ]),
            "?source=Dan%E2%80%99s%20Watch&n=1"
        );
    }
}
