//! Live smoke test against a real GitHub Enterprise Server host.
//!
//! Requires network access to the host plus:
//! - `LAZYBOX_GHES_HOST` — bare hostname (e.g. `ghe.example.com`)
//! - a token the credential chain resolves (`LAZYBOX_GITHUB_TOKEN` / `GH_TOKEN`,
//!   or `gh auth login --hostname <host>` for the `gh auth token --hostname`
//!   fallback)
//!
//! Run explicitly:
//! `cargo test -p lazybox-gh --test ghes_live -- --ignored --nocapture`

use std::time::Duration;

#[tokio::test]
#[ignore = "live network test against a GHES host; set LAZYBOX_GHES_HOST and a token env"]
async fn ghes_rest_and_graphql_smoke() {
    let body = async {
        let host = std::env::var("LAZYBOX_GHES_HOST").expect("LAZYBOX_GHES_HOST not set");
        let cred = lazybox_gh::credential_chain(Some(&host))
            .resolve(&lazybox_gh::credential_scope(Some(&host)))
            .await
            .expect("no GitHub credential resolved");

        // Constructing the client already proves the REST base URI:
        // `from_credential_with_host` calls `GET /user` on the enterprise
        // host, which only succeeds at `https://<host>/api/v3/user`.
        let client = lazybox_gh::GhClient::from_credential_with_host(cred, Some(&host))
            .await
            .expect("client init (REST /user on the enterprise host) failed");
        println!("REST OK — authenticated as {}", client.username());

        // GraphQL rides a separate transport based at `https://<host>/api`
        // so the relative `/graphql` route lands on `/api/graphql`.
        client
            .bootstrap_graphql_budget()
            .await
            .expect("GraphQL request to /api/graphql failed");
        println!("GraphQL OK — rate-budget query answered");
    };
    tokio::time::timeout(Duration::from_secs(60), body)
        .await
        .expect("live smoke test timed out");
}
