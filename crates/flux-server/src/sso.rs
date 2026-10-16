//! Sign-in with external providers (GitHub, Google, any OpenID Connect
//! IdP) and the second step of two-factor sign-in.
//!
//! OAuth uses the authorization-code flow with PKCE and a one-time `state`.
//! A provider identity resolves to an account in this order:
//! 1. an identity already linked (placeholder accounts created by `flux
//!    migrate github --create-users` are claimed on first sign-in);
//! 2. the signed-in user, when linking from settings;
//! 3. an account with the same *verified* email address;
//! 4. a new account, when the provider allows sign-up (public providers
//!    also follow the server's sign-up policy; an organization's own OIDC
//!    IdP does not).
//!
//! Accounts with two-factor auth still need a code after any first factor.

use base64::Engine as _;
use flux_store::{NewAccount, User};
use serde::Deserialize;
use sha2::Digest as _;

use crate::core::{Flux, FluxError, FluxResult};
use crate::settings::OAuthProvider;

/// Short-lived sign-in state, keyed by a random handle.
#[derive(Debug, Clone)]
pub enum Ticket {
    /// First factor passed; waiting for a two-factor code.
    SecondFactor { user_id: i64, next: String, client: String },
    /// An OAuth authorization in progress.
    OAuth { provider: String, verifier: String, next: String, link_user: Option<i64> },
}

/// What happened after a first factor.
#[derive(Debug, Clone)]
pub enum SignIn {
    Session { token: String, next: String },
    NeedCode { ticket: String, next: String },
    Linked { next: String },
}

fn random_handle() -> String {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).expect("OS random number generator");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

/// Profile fields from a provider's user info.
#[derive(Debug, Clone, Default)]
struct Profile {
    subject: String,
    login: String,
    name: String,
    email: Option<String>,
    email_verified: bool,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

/// A valid account name from a provider login or email.
fn suggest_name(p: &Profile) -> String {
    let raw = if !p.login.is_empty() {
        p.login.clone()
    } else {
        p.email.as_deref().and_then(|e| e.split('@').next()).unwrap_or("user").to_string()
    };
    let mut s: String = raw.chars().map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '-' }).collect();
    s = s.trim_matches(|c| c == '-' || c == '.').to_string();
    s.truncate(36);
    if s.is_empty() { "user".into() } else { s }
}

impl Flux {
    pub fn oauth_provider(&self, name: &str) -> FluxResult<&OAuthProvider> {
        self.settings
            .oauth
            .iter()
            .find(|p| p.name == name)
            .ok_or_else(|| FluxError::NotFound(format!("sign-in with {name} is not configured")))
    }

    fn redirect_uri(&self, base: &str, provider: &str) -> String {
        format!("{}/-/oauth/{provider}/callback", self.settings.public_url.as_deref().unwrap_or(base).trim_end_matches('/'))
    }

    /// The provider authorization URL to send the browser to.
    pub fn oauth_start(&self, provider: &str, next: &str, link_user: Option<i64>, base: &str) -> FluxResult<String> {
        let p = self.oauth_provider(provider)?;
        let state = random_handle();
        let verifier = random_handle();
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier.as_bytes()));
        self.tickets.insert(state.clone(), Ticket::OAuth { provider: provider.into(), verifier, next: next.into(), link_user });
        let mut u = url::Url::parse(&p.auth_url).map_err(|e| FluxError::Internal(anyhow::anyhow!("bad auth URL: {e}")))?;
        u.query_pairs_mut()
            .append_pair("client_id", &p.client_id)
            .append_pair("redirect_uri", &self.redirect_uri(base, provider))
            .append_pair("response_type", "code")
            .append_pair("scope", &p.scopes)
            .append_pair("state", &state)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256");
        Ok(u.to_string())
    }

    async fn fetch_profile(&self, p: &OAuthProvider, access_token: &str) -> FluxResult<Profile> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .user_agent("flux")
            .build()
            .map_err(|e| FluxError::Internal(e.into()))?;
        let get = |url: String| {
            let http = http.clone();
            let token = access_token.to_string();
            async move {
                let r = http.get(&url).bearer_auth(token).header("accept", "application/json").send().await;
                match r {
                    Ok(r) if r.status().is_success() => {
                        r.json::<serde_json::Value>().await.map_err(|e| FluxError::Internal(e.into()))
                    }
                    Ok(r) => {
                        Err(FluxError::Unauthenticated(format!("the provider refused the user info request ({})", r.status())))
                    }
                    Err(e) => Err(FluxError::Internal(e.into())),
                }
            }
        };
        let v = get(p.userinfo_url.clone()).await?;
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or_default().to_string();
        let mut prof = if p.name == "github" {
            Profile {
                subject: v.get("id").and_then(|x| x.as_i64()).map(|i| i.to_string()).unwrap_or_default(),
                login: s("login"),
                name: s("name"),
                email: None,
                email_verified: false,
            }
        } else {
            Profile {
                subject: s("sub"),
                login: v.get("preferred_username").and_then(|x| x.as_str()).unwrap_or_default().to_string(),
                name: s("name"),
                email: Some(s("email")).filter(|e| !e.is_empty()),
                email_verified: v.get("email_verified").and_then(|x| x.as_bool()).unwrap_or(false),
            }
        };
        if p.name == "github" {
            // Only a primary, verified address counts.
            let base = p.userinfo_url.trim_end_matches("/user");
            if let Ok(emails) = get(format!("{base}/user/emails")).await
                && let Some(primary) =
                    emails.as_array().into_iter().flatten().find(|e| e["primary"] == true && e["verified"] == true)
            {
                prof.email = primary["email"].as_str().map(str::to_string);
                prof.email_verified = true;
            }
        }
        if prof.subject.is_empty() {
            return Err(FluxError::Unauthenticated("the provider did not identify the user".into()));
        }
        Ok(prof)
    }

    /// Finish an OAuth sign-in (the callback).
    pub async fn oauth_finish(&self, provider: &str, code: &str, state: &str, base: &str, client: &str) -> FluxResult<SignIn> {
        let Some(Ticket::OAuth { provider: tp, verifier, next, link_user }) = self.tickets.remove(state) else {
            return Err(FluxError::Unauthenticated("this sign-in link expired; start again".into()));
        };
        if tp != provider {
            return Err(FluxError::Unauthenticated("provider mismatch".into()));
        }
        let p = self.oauth_provider(provider)?.clone();
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .map_err(|e| FluxError::Internal(e.into()))?;
        let resp = http
            .post(&p.token_url)
            .header("accept", "application/json")
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", &self.redirect_uri(base, provider)),
                ("client_id", &p.client_id),
                ("client_secret", &p.client_secret),
                ("code_verifier", &verifier),
            ])
            .send()
            .await
            .map_err(|e| FluxError::Internal(e.into()))?;
        let tok: TokenResponse = resp.json().await.map_err(|e| FluxError::Internal(e.into()))?;
        let Some(access) = tok.access_token else {
            return Err(FluxError::Unauthenticated(format!(
                "sign-in failed: {}",
                tok.error_description.or(tok.error).unwrap_or_else(|| "no access token".into())
            )));
        };
        let prof = self.fetch_profile(&p, &access).await?;
        let user = self.resolve_identity(&p, &prof, link_user).await?;
        if link_user.is_some() {
            return Ok(SignIn::Linked { next });
        }
        self.after_first_factor(user, next, client).await
    }

    async fn resolve_identity(&self, p: &OAuthProvider, prof: &Profile, link_user: Option<i64>) -> FluxResult<User> {
        if let Some(id) = self.meta.identity(&p.name, &prof.subject).await? {
            if link_user.is_some_and(|u| u != id.user_id) {
                return Err(FluxError::AlreadyExists(format!("that {} account is linked to another user", p.label)));
            }
            let acct = self.meta.get_account_by_id(id.user_id).await?.ok_or_else(|| FluxError::NotFound("account".into()))?;
            if acct.disabled {
                return Err(FluxError::PermissionDenied("this account is disabled".into()));
            }
            if self.meta.user_kind(acct.user.id).await? == "placeholder" {
                self.meta.set_user_kind(acct.user.id, "user").await?;
                if acct.email.is_none() && prof.email_verified {
                    let _ = self
                        .meta
                        .update_account(
                            acct.user.id,
                            &flux_store::AccountUpdate { email: Some(prof.email.clone()), ..Default::default() },
                        )
                        .await;
                }
                self.meta.audit_append(&acct.user.name, "user.claimed", &acct.user.name, &p.name).await?;
            }
            return Ok(acct.user);
        }
        if let Some(uid) = link_user {
            let acct = self.meta.get_account_by_id(uid).await?.ok_or_else(|| FluxError::NotFound("account".into()))?;
            self.meta.link_identity(&p.name, &prof.subject, uid, &prof.login).await?;
            self.meta.audit_append(&acct.user.name, "user.identity_linked", &acct.user.name, &p.name).await?;
            return Ok(acct.user);
        }
        if prof.email_verified
            && let Some(email) = &prof.email
            && let Some((acct, _)) = self.meta.get_account(email).await?
            && acct.email.as_deref().is_some_and(|e| e.eq_ignore_ascii_case(email))
        {
            if acct.disabled {
                return Err(FluxError::PermissionDenied("this account is disabled".into()));
            }
            self.meta.link_identity(&p.name, &prof.subject, acct.user.id, &prof.login).await?;
            self.meta.audit_append(&acct.user.name, "user.identity_linked", &acct.user.name, &p.name).await?;
            return Ok(acct.user);
        }
        let first = self.meta.user_count().await? == 0;
        let public_idp = p.name == "github" || p.name == "google";
        let may_sign_up = p.allow_signup && (!public_idp || first || self.signup == crate::identity::SignupPolicy::Open);
        if !may_sign_up {
            return Err(FluxError::PermissionDenied(format!(
                "no account is linked to this {} account; ask an administrator to create one",
                p.label
            )));
        }
        let base = suggest_name(prof);
        let mut created = None;
        for i in 0..50 {
            let name = if i == 0 { base.clone() } else { format!("{base}-{}", i + 1) };
            if !crate::identity::valid_name(&name) {
                continue;
            }
            let email = prof.email.clone().filter(|_| prof.email_verified);
            match self
                .meta
                .create_account(&NewAccount {
                    name,
                    email,
                    display_name: prof.name.clone(),
                    password_hash: None,
                    is_admin: first,
                })
                .await
            {
                Ok(u) => {
                    created = Some(u);
                    break;
                }
                Err(flux_store::StoreError::Conflict(m)) if m.contains("email") => {
                    return Err(FluxError::AlreadyExists(
                        "an account with that email exists; sign in and link from settings".into(),
                    ));
                }
                Err(flux_store::StoreError::Conflict(_)) => continue,
                Err(e) => return Err(e.into()),
            }
        }
        let user = created.ok_or_else(|| FluxError::Internal(anyhow::anyhow!("could not pick an account name")))?;
        self.meta.link_identity(&p.name, &prof.subject, user.id, &prof.login).await?;
        self.meta.audit_append(&user.name, "user.signup", &user.name, &p.name).await?;
        Ok(user)
    }

    /// After a first factor: a session, or a ticket for the two-factor step.
    pub async fn after_first_factor(&self, user: User, next: String, client: &str) -> FluxResult<SignIn> {
        if self.two_factor_enabled(user.id).await? {
            let ticket = random_handle();
            self.tickets
                .insert(ticket.clone(), Ticket::SecondFactor { user_id: user.id, next: next.clone(), client: client.into() });
            return Ok(SignIn::NeedCode { ticket, next });
        }
        let token = self.open_session(&user, client).await?;
        Ok(SignIn::Session { token, next })
    }

    /// The second step: a TOTP or recovery code for a pending sign-in.
    pub async fn finish_second_factor(&self, ticket: &str, code: &str) -> FluxResult<(String, String)> {
        let Some(Ticket::SecondFactor { user_id, next, client }) = self.tickets.get(ticket) else {
            return Err(FluxError::Unauthenticated("this sign-in expired; start again".into()));
        };
        let key = format!("2fa:{user_id}");
        if self.login_failures.get(&key).unwrap_or(0) >= 10 {
            self.tickets.invalidate(ticket);
            return Err(FluxError::PermissionDenied("too many wrong codes; try again in 15 minutes".into()));
        }
        if !self.check_second_factor(user_id, code).await? {
            self.login_failures.insert(key.clone(), self.login_failures.get(&key).unwrap_or(0) + 1);
            return Err(FluxError::Unauthenticated("that code is not right".into()));
        }
        self.tickets.invalidate(ticket);
        let acct = self.meta.get_account_by_id(user_id).await?.ok_or_else(|| FluxError::NotFound("account".into()))?;
        let token = self.open_session(&acct.user, &client).await?;
        Ok((token, next))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_from_profiles() {
        let p = |login: &str, email: Option<&str>| Profile {
            login: login.into(),
            email: email.map(str::to_string),
            ..Default::default()
        };
        assert_eq!(suggest_name(&p("octocat", None)), "octocat");
        assert_eq!(suggest_name(&p("", Some("jane.doe@x.com"))), "jane.doe");
        assert_eq!(suggest_name(&p("--weird name!", None)), "weird-name");
        assert_eq!(suggest_name(&p("", None)), "user");
    }
}
