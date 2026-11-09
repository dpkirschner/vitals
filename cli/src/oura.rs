//! `vitals oura auth|sync`: the CLI side of the Oura sync (`vitals-oura`).

use crate::{json_out, OuraCmd};
use vitals_core::time::parse_date;
use vitals_core::Store;

fn config(db: &str) -> Result<vitals_oura::Config, String> {
    vitals_oura::Config::from_env(db)?.ok_or_else(|| {
        "Oura isn't configured: set OURA_CLIENT_ID_FILE, OURA_CLIENT_SECRET_FILE \
         and OURA_REDIRECT_URI (docs/DEPLOYMENT.md)"
            .to_string()
    })
}

pub(crate) fn run(
    db: &str,
    cmd: &OuraCmd,
    json: bool,
    open_store: impl FnOnce() -> Result<Store, String>,
) -> Result<(), String> {
    let cfg = config(db)?;
    match cmd {
        OuraCmd::Auth => auth(&cfg),
        OuraCmd::Sync { since } => {
            let since = since
                .as_deref()
                .map(|s| parse_date(s).ok_or_else(|| format!("bad date: {s}")))
                .transpose()?;
            let store = open_store()?;
            let r = vitals_oura::sync(&cfg, &store, since)?;
            if json {
                return json_out(&r);
            }
            for (kind, c) in &r.kinds {
                if c.added + c.updated + c.removed > 0 {
                    println!("  {kind:<26} +{} ~{} -{}", c.added, c.updated, c.removed);
                }
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

fn auth(cfg: &vitals_oura::Config) -> Result<(), String> {
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
