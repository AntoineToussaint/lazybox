//! `lazybox auth` — native provider login without the `gh` CLI.
//!
//! Drives the GitHub OAuth device flow (see [`lazybox_gh::oauth`]) so a
//! machine with no `gh` installed can still obtain a token:
//!
//! ```text
//! lazybox auth login [github]   run the device flow and store the token
//! lazybox auth status           show whether a token is stored
//! lazybox auth logout [github]  remove the stored token
//! ```
//!
//! Linear OAuth is not yet wired here (its authorization-code flow needs a
//! redirect callback); `lazybox auth login linear` reports that plainly.
//!
//! All user-facing text goes to **stdout**: `init_tracing` redirects the
//! process stderr into the log file, so a returned `Err` (or `eprintln!`)
//! never reaches the terminal. Error paths print, then `exit(1)`.

use lazybox_gh::oauth;

pub async fn auth_subcommand(args: &[String]) -> anyhow::Result<()> {
    match args.first().map(String::as_str) {
        Some("login") => login(&args[1..]).await,
        Some("status") => status(),
        Some("logout") => logout(&args[1..]),
        Some(other) => {
            println!("unknown `auth` subcommand: {other}\n");
            print_usage();
            std::process::exit(2);
        }
        None => {
            print_usage();
            Ok(())
        }
    }
}

fn print_usage() {
    println!(
        "usage:\n  \
         lazybox auth login [github]    log in via GitHub OAuth device flow\n  \
         lazybox auth status            show stored login status\n  \
         lazybox auth logout [github]   remove the stored token"
    );
}

fn provider_arg(args: &[String]) -> &str {
    args.first().map(String::as_str).unwrap_or("github")
}

async fn login(args: &[String]) -> anyhow::Result<()> {
    match provider_arg(args) {
        "github" => login_github().await,
        "linear" => {
            println!(
                "Linear OAuth login is not yet implemented. Set LINEAR_API_KEY \
                 or install the `linear` CLI for now."
            );
            std::process::exit(1);
        }
        other => {
            println!("unknown provider `{other}` (expected `github`)");
            std::process::exit(2);
        }
    }
}

async fn login_github() -> anyhow::Result<()> {
    let Some(client_id) = oauth::client_id() else {
        println!(
            "No GitHub OAuth client id is configured. Set {} to your OAuth \
             app's client id and re-run `lazybox auth login github`.",
            oauth::CLIENT_ID_ENV
        );
        std::process::exit(1);
    };

    let dc = match oauth::request_device_code(&client_id, oauth::DEFAULT_SCOPES).await {
        Ok(dc) => dc,
        Err(e) => {
            println!("Could not start GitHub login: {e}");
            std::process::exit(1);
        }
    };

    println!(
        "\nTo authorize lazybox, open:\n\n    {}\n\nand enter the code:\n\n    {}\n",
        dc.verification_uri, dc.user_code
    );
    // Best-effort: pop the verification page open. A headless box has no
    // browser, so a failure here is expected and non-fatal — the URL is
    // already printed above.
    let _ = lazybox_tui_core::editors::open_url(&dc.verification_uri, None);

    println!("Waiting for authorization…");
    let token = match oauth::poll_for_token(&client_id, &dc).await {
        Ok(t) => t,
        Err(e) => {
            println!("GitHub login failed: {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = oauth::save_token(&token) {
        println!("Authorized, but could not save the token: {e}");
        std::process::exit(1);
    }

    println!(
        "\n✓ Logged in to GitHub (scopes: {}). Token stored at {}.",
        if token.scope.is_empty() {
            "default"
        } else {
            &token.scope
        },
        oauth::token_path().display()
    );
    Ok(())
}

fn status() -> anyhow::Result<()> {
    match oauth::load_token() {
        Some(t) => {
            let scopes = if t.scope.is_empty() {
                "default".to_string()
            } else {
                t.scope
            };
            let when = t
                .obtained_at
                .map(|w| format!(", obtained {w}"))
                .unwrap_or_default();
            println!("GitHub: logged in via OAuth (scopes: {scopes}{when}).");
        }
        None => {
            println!("GitHub: not logged in via OAuth. Run `lazybox auth login github`.");
        }
    }
    Ok(())
}

fn logout(args: &[String]) -> anyhow::Result<()> {
    match provider_arg(args) {
        "github" => {
            let existed = oauth::load_token().is_some();
            if let Err(e) = oauth::delete_token() {
                println!("Could not remove the stored GitHub token: {e}");
                std::process::exit(1);
            }
            if existed {
                println!("Removed the stored GitHub OAuth token.");
            } else {
                println!("No stored GitHub OAuth token to remove.");
            }
            Ok(())
        }
        other => {
            println!("unknown provider `{other}` (expected `github`)");
            std::process::exit(2);
        }
    }
}
