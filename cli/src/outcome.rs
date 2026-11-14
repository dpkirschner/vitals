//! How a run ends, in the shape agent wrappers expect (the shared CLI
//! conventions): exit codes 0–7, and with `--json` exactly one JSON object
//! on stdout — `{"ok":true, ...fields}` or
//! `{"ok":false,"error":{"code":N,"message":"..."}}`, where `code` is the
//! exit code. Without `--json`, errors go to stderr as `error: <message>`.

use std::process::ExitCode;

/// A bug in the tool, or a failure it can't classify.
pub const INTERNAL: u8 = 1;
/// Bad arguments, or input the tool won't take.
pub const USAGE: u8 = 2;
/// Nothing matches.
pub const NOT_FOUND: u8 = 3;
/// Already done, or the state changed under it. (No vitals command hits
/// this yet; kept so the table is whole.)
#[allow(dead_code)]
pub const CONFLICT: u8 = 4;
/// A service is down, slow or rate-limited.
#[cfg_attr(not(any(feature = "client", feature = "oura")), allow(dead_code))]
pub const UNREACHABLE: u8 = 5;
/// Credentials refused or unreadable.
pub const DENIED: u8 = 6;
/// A guard said no (e.g. a read-only token on a write).
#[cfg_attr(not(feature = "client"), allow(dead_code))]
pub const REFUSED: u8 = 7;

/// A failed run: the exit code and what to tell the caller.
#[derive(Debug, PartialEq, Eq)]
pub struct Fail {
    pub code: u8,
    pub message: String,
}

impl Fail {
    pub fn new(code: u8, message: impl Into<String>) -> Fail {
        Fail {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for Fail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

pub fn usage(message: impl Into<String>) -> Fail {
    Fail::new(USAGE, message)
}

pub fn not_found(message: impl Into<String>) -> Fail {
    Fail::new(NOT_FOUND, message)
}

/// Unclassified errors are internal; callers that know better say so.
impl From<String> for Fail {
    fn from(message: String) -> Fail {
        Fail::new(INTERNAL, message)
    }
}

impl From<&str> for Fail {
    fn from(message: &str) -> Fail {
        Fail::new(INTERNAL, message)
    }
}

pub type CliResult<T = ()> = Result<T, Fail>;

/// Wrap one serialized JSON value as a success object. An object gets
/// `"ok":true` in front of its own fields (their order kept); a list goes
/// under `"items"`; `null` (a write with nothing to report) is just
/// `{"ok":true}`; any other scalar goes under `"value"`.
pub fn envelope(json: &str) -> String {
    let body = json.trim();
    match body.as_bytes().first() {
        Some(b'{') => {
            let rest = body[1..].trim_start();
            if rest.starts_with('}') {
                "{\"ok\":true}".to_string()
            } else {
                format!("{{\"ok\":true,{rest}")
            }
        }
        Some(b'[') => format!("{{\"ok\":true,\"items\":{body}}}"),
        None => "{\"ok\":true}".to_string(),
        _ if body == "null" => "{\"ok\":true}".to_string(),
        _ => format!("{{\"ok\":true,\"value\":{body}}}"),
    }
}

/// Print a serializable payload as a success object.
pub fn json_out<T: serde::Serialize>(v: &T) -> CliResult {
    let s = serde_json::to_string(v).map_err(|e| format!("serialize: {e}"))?;
    println!("{}", envelope(&s));
    Ok(())
}

/// Report a failure (JSON on stdout, or text on stderr) and give its exit code.
pub fn fail_exit(json: bool, f: &Fail) -> ExitCode {
    if json {
        // Built by hand so "ok" leads, as on success (json! sorts keys).
        let message = serde_json::Value::from(f.message.as_str());
        println!(
            "{{\"ok\":false,\"error\":{{\"code\":{},\"message\":{message}}}}}",
            f.code
        );
    } else {
        eprintln!("error: {}", f.message);
    }
    ExitCode::from(f.code)
}

/// The run's outcome as a process exit code.
pub fn finish(json: bool, r: CliResult) -> ExitCode {
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(f) => fail_exit(json, &f),
    }
}

/// Parse argv, reporting clap's usage errors in our shape (exit 2) when
/// `--json` is anywhere on the line; help and version print as usual.
pub fn parse<P: clap::Parser>() -> Result<P, ExitCode> {
    P::try_parse().map_err(|e| {
        use clap::error::ErrorKind;
        let json = std::env::args().any(|a| a == "--json");
        match e.kind() {
            ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => e.exit(),
            _ if !json => e.exit(),
            _ => {
                let text = e.render().to_string();
                let first = text.lines().next().unwrap_or("bad arguments");
                let msg = first.strip_prefix("error: ").unwrap_or(first);
                fail_exit(true, &usage(msg.to_string()))
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_merges_objects_and_names_lists() {
        assert_eq!(envelope(r#"{"b":1,"a":2}"#), r#"{"ok":true,"b":1,"a":2}"#);
        assert_eq!(envelope("{}"), r#"{"ok":true}"#);
        assert_eq!(envelope("[1,2]\n"), r#"{"ok":true,"items":[1,2]}"#);
        assert_eq!(envelope("null"), r#"{"ok":true}"#);
        assert_eq!(envelope("3"), r#"{"ok":true,"value":3}"#);
    }

    #[test]
    fn failure_codes_default_to_internal() {
        let f: Fail = "boom".into();
        assert_eq!(f.code, INTERNAL);
        assert_eq!(usage("x").code, USAGE);
    }
}
