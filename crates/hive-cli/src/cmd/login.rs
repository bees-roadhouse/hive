//! `hive login` / `hive logout` (hive-auth-mcp-design.md §8 Phase 3, §3.2).
//!
//! `login` runs the password -> /authorize (PKCE) -> /token flow and caches the
//! access token. The password is read from `HIVE_PASSWORD` (so it stays out of
//! shell history / argv), falling back to a stdin prompt when a terminal is
//! attached. The username comes from `--username` or `HIVE_USERNAME`.
//!
//! `--device` reserves the RFC 8628 device-grant surface (Phase 5); it returns
//! an explicit not-implemented error today.

use std::io::Write;

use crate::auth;
use crate::cli::LoginArgs;

pub async fn run(args: LoginArgs) -> anyhow::Result<()> {
    if args.device {
        return auth::login_device(args.scope.as_deref()).await;
    }

    let username = args
        .username
        .or_else(|| std::env::var("HIVE_USERNAME").ok())
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("provide --username (or set HIVE_USERNAME)"))?;

    let password = resolve_password()?;
    auth::login(username.trim(), &password, args.scope.as_deref()).await
}

pub async fn logout() -> anyhow::Result<()> {
    if auth::clear_token()? {
        println!("logged out (cached token removed)");
    } else {
        println!("no cached token to remove");
    }
    Ok(())
}

/// Password resolution: `HIVE_PASSWORD` env first (scriptable, no argv leak),
/// else a single stdin line. We deliberately do not echo-suppress here (no extra
/// dependency); for unattended use prefer `HIVE_PASSWORD`. Flagged so a later
/// pass can add a hidden TTY prompt (`rpassword`) if Nate wants it.
fn resolve_password() -> anyhow::Result<String> {
    if let Ok(p) = std::env::var("HIVE_PASSWORD")
        && !p.is_empty()
    {
        return Ok(p);
    }
    print!("password: ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    let n = std::io::stdin().read_line(&mut line)?;
    if n == 0 {
        anyhow::bail!("no password provided (set HIVE_PASSWORD or type one)");
    }
    let pw = line.trim_end_matches(['\r', '\n']).to_string();
    if pw.is_empty() {
        anyhow::bail!("empty password");
    }
    Ok(pw)
}
