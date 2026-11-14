//! `vitals oura auth|sync`: the CLI side of the Oura sync (`vitals-oura`).

use crate::outcome::{json_out, usage, CliResult, Fail, DENIED, UNREACHABLE};
use crate::OuraCmd;
use vitals_core::time::parse_date;
use vitals_core::Store;

/// Missing or unreadable Oura credentials are both "denied" (exit 6).
fn config(db: &str) -> CliResult<vitals_oura::Config> {
    vitals_oura::Config::from_env(db)
        .map_err(|e| Fail::new(DENIED, e))?
        .ok_or_else(|| {
            Fail::new(
                DENIED,
                "Oura isn't configured: set OURA_CLIENT_ID_FILE, OURA_CLIENT_SECRET_FILE \
                 and OURA_REDIRECT_URI (docs/DEPLOYMENT.md)",
            )
        })
}

pub(crate) fn run(
    db: &str,
    cmd: &OuraCmd,
    json: bool,
    open_store: impl FnOnce() -> CliResult<Store>,
) -> CliResult {
    let cfg = config(db)?;
    match cmd {
        OuraCmd::Auth => auth(&cfg),
        OuraCmd::Sync { since } => {
            let since = since
                .as_deref()
                .map(|s| parse_date(s).ok_or_else(|| usage(format!("bad date: {s}"))))
                .transpose()?;
            let store = open_store()?;
            // The sync's failures are Oura's API or its tokens: unreachable.
            let r =
                vitals_oura::sync(&cfg, &store, since).map_err(|e| Fail::new(UNREACHABLE, e))?;
            if json {
                return json_out(&r);
            }
            for (kind, c) in &r.kinds {
                if c.added + c.updated + c.removed > 0 {
                    println!("  {kind:<26} +{} ~{} -{}", c.added, c.updated, c.removed);
                }
            }
            for e in &r.skipped {
                eprintln!("  skipped: {e}");
            }
            println!(
                "oura sync: +{} new, {} revised, {} removed in {:.1}s",
                r.added,
                r.updated,
                r.removed,
                r.elapsed_ms as f64 / 1000.0
            );
            Ok(())
        }
    }
}

fn auth(cfg: &vitals_oura::Config) -> CliResult {
    let start = vitals_oura::begin_auth(cfg)?;
    eprintln!(
        "1. Open this URL in a browser and approve vitals:\n\n{}\n",
        start.url
    );
    eprintln!(
        "2. The browser then lands on {} (that page may say it can't load, \
         which is fine).\n   Copy the whole address from the address bar and paste it here:",
        cfg.redirect_uri
    );
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| format!("read stdin: {e}"))?;
    vitals_oura::finish_auth(cfg, &start, &line)?;
    eprintln!(
        "connected: tokens saved to {} (0600). Run `vitals oura sync`, or let the server sync.",
        cfg.tokens_file.display()
    );
    Ok(())
}
