//! Sign-in: two-factor codes (TOTP and recovery codes) and OAuth/OIDC with
//! account creation, linking by verified email and placeholder claiming.

use std::sync::Arc;

use axum::routing::{get, post};
use flux_server::auth::{AuthMode, Principal};
use flux_server::settings::{OAuthProvider, Settings};
use serde_json::json;

fn no_redirects() -> reqwest::Client {
    reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap()
}

fn location(r: &reqwest::Response) -> String {
    r.headers().get("location").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string()
}

fn session(r: &reqwest::Response) -> Option<String> {
    r.headers().get_all("set-cookie").iter().filter_map(|v| v.to_str().ok()).find_map(|c| {
        let v = c.strip_prefix("flux_token=")?.split(';').next()?;
        (!v.is_empty()).then(|| v.to_string())
    })
}

fn param(url: &str, key: &str) -> Option<String> {
    let q = url.split_once('?')?.1;
    url::form_urlencoded::parse(q.as_bytes()).find(|(k, _)| k == key).map(|(_, v)| v.into_owned())
}

/// A fake OpenID Connect provider: any code works; the user is chosen by
/// the code (`code=<sub>:<email>:<login>`).
async fn mock_idp() -> String {
    let app = axum::Router::new()
        .route(
            "/token",
            post(|body: String| async move {
                let code = url::form_urlencoded::parse(body.as_bytes()).find(|(k, _)| k == "code").map(|(_, v)| v.into_owned()).unwrap();
                let verifier = url::form_urlencoded::parse(body.as_bytes()).any(|(k, v)| k == "code_verifier" && v.len() >= 43);
                assert!(verifier, "PKCE verifier sent");
                axum::Json(json!({ "access_token": code, "token_type": "bearer" }))
            }),
        )
        .route(
            "/userinfo",
            get(|headers: axum::http::HeaderMap| async move {
                let tok = headers["authorization"].to_str().unwrap().trim_start_matches("Bearer ").to_string();
                let parts: Vec<&str> = tok.split(':').collect();
                axum::Json(json!({ "sub": parts[0], "email": parts[1], "email_verified": true, "preferred_username": parts[2], "name": "Test" }))
            }),
        );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{addr}")
}

#[tokio::test]
async fn two_factor_and_oidc() {
    let idp = mock_idp().await;
    let dir = tempfile::tempdir().unwrap();
    let cfg = flux_server::StorageConfig::data_dir(dir.path()).unwrap();
    let mut provider = OAuthProvider::oidc(&idp, "client".into(), "secret".into());
    provider.auth_url = format!("{idp}/authorize");
    provider.token_url = format!("{idp}/token");
    provider.userinfo_url = format!("{idp}/userinfo");
    let settings = Settings { secret_key: Some([3; 32]), oauth: vec![provider], ..Default::default() };
    let flux: Arc<flux_server::Flux> =
        flux_server::open_with_settings(&cfg, AuthMode::Token, Default::default(), settings).await.unwrap();
    let alice = flux.sign_up("alice", Some("alice@x.com"), "Alice", "correct horse battery").await.unwrap();
    let (listener, addr) = flux_server::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
    tokio::spawn(flux_server::serve(flux.clone(), listener, std::future::pending()));
    let base = format!("http://{addr}");
    let http = no_redirects();

    // ── two-factor ──
    let who = Principal { user: Some(alice.clone()), mode: AuthMode::Token, ..Default::default() };
    let e = flux.begin_totp(&who).await.unwrap();
    let secret = flux_server::twofactor::base32_decode(&e.secret_b32).unwrap();
    let now = || std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    assert!(flux.enable_totp(&who, "000000").await.is_err() || flux_server::twofactor::totp_at(&secret, now() / 30) == "000000");
    let codes = flux.enable_totp(&who, &flux_server::twofactor::totp_at(&secret, now() / 30)).await.unwrap();
    assert_eq!(codes.len(), 10);
    // The password alone is not enough.
    assert!(flux.password_login("alice", "correct horse battery", "t").await.is_err());
    let r = http
        .post(format!("{base}/-/login"))
        .form(&[("login", "alice"), ("password", "correct horse battery"), ("next", "/x")])
        .send()
        .await
        .unwrap();
    assert!(session(&r).is_none());
    let loc = location(&r);
    assert!(loc.starts_with("/-/login/2fa"), "{loc}");
    let ticket = param(&loc, "ticket").unwrap();
    let r = http
        .post(format!("{base}/-/login/2fa"))
        .form(&[("ticket", ticket.as_str()), ("code", "123456"), ("next", "/x")])
        .send()
        .await
        .unwrap();
    assert!(session(&r).is_none() && location(&r).contains("err="), "wrong code refused");
    let r = http
        .post(format!("{base}/-/login/2fa"))
        .form(&[("ticket", ticket.as_str()), ("code", codes[0].as_str()), ("next", "/x")])
        .send()
        .await
        .unwrap();
    assert!(session(&r).is_some(), "recovery code accepted");
    assert_eq!(location(&r), "/x");
    assert_eq!(flux.recovery_codes_left(alice.id).await.unwrap(), 9);
    // Recovery codes work once; TOTP codes too.
    assert!(!flux.check_second_factor(alice.id, &codes[0]).await.unwrap());
    let code = flux_server::twofactor::totp_at(&secret, now() / 30);
    assert!(flux.check_second_factor(alice.id, &code).await.unwrap());
    assert!(!flux.check_second_factor(alice.id, &code).await.unwrap(), "no replay");
    // gRPC-style password login with a code.
    assert!(flux.password_login_2fa("alice", "correct horse battery", Some(&codes[1]), "t").await.is_ok());

    // ── OIDC ──
    let sign_in = |code: &'static str| {
        let http = http.clone();
        let base = base.clone();
        async move {
            let r = http.get(format!("{base}/-/oauth/oidc/start?next=/home")).send().await.unwrap();
            let auth = location(&r);
            assert!(auth.contains("code_challenge_method=S256"), "{auth}");
            let state = param(&auth, "state").unwrap();
            http.get(format!(
                "{base}/-/oauth/oidc/callback?code={}&state={state}",
                url::form_urlencoded::byte_serialize(code.as_bytes()).collect::<String>()
            ))
            .send()
            .await
            .unwrap()
        }
    };
    // A new person gets an account (the IdP is the organization's own).
    let r = sign_in("101:bob@corp.com:bob").await;
    assert!(session(&r).is_some(), "{}", location(&r));
    assert_eq!(location(&r), "/home");
    let bob = flux.meta.get_user("bob").await.unwrap().unwrap();
    let r = sign_in("101:bob@corp.com:bob").await;
    assert!(session(&r).is_some());
    assert_eq!(
        flux.meta.list_users().await.unwrap().iter().filter(|u| u.name.starts_with("bob")).count(),
        1,
        "same account again"
    );
    assert_eq!(flux.meta.identity("oidc", "101").await.unwrap().unwrap().user_id, bob.id);
    // A verified email links to the existing account — which still asks for 2FA.
    let r = sign_in("202:alice@x.com:alice-corp").await;
    assert!(session(&r).is_none());
    assert!(location(&r).starts_with("/-/login/2fa"), "{}", location(&r));
    assert_eq!(flux.meta.identity("oidc", "202").await.unwrap().unwrap().user_id, alice.id);
    // Placeholders from a migration are claimed on first sign-in.
    let carol = flux.meta.create_account(&flux_store::NewAccount { name: "carol".into(), ..Default::default() }).await.unwrap();
    flux.meta.set_user_kind(carol.id, "placeholder").await.unwrap();
    flux.meta.link_identity("oidc", "303", carol.id, "carol").await.unwrap();
    let r = sign_in("303:carol@corp.com:carol").await;
    assert!(session(&r).is_some());
    assert_eq!(flux.meta.user_kind(carol.id).await.unwrap(), "user");
    // A replayed or forged state is refused.
    let r = http.get(format!("{base}/-/oauth/oidc/callback?code=x&state=forged")).send().await.unwrap();
    assert!(session(&r).is_none() && location(&r).contains("err="));
}
