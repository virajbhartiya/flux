//! Server-wide settings beyond storage and auth mode.

/// An external sign-in provider (OAuth 2 / OpenID Connect).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthProvider {
    /// `github`, `google`, or any name for a generic OIDC provider.
    pub name: String,
    /// Shown on the sign-in button.
    pub label: String,
    pub client_id: String,
    pub client_secret: String,
    pub auth_url: String,
    pub token_url: String,
    /// User info endpoint (GitHub `/user`, OIDC `userinfo`).
    pub userinfo_url: String,
    pub scopes: String,
    /// Create accounts on first sign-in (otherwise only linked accounts
    /// and claimed placeholders may sign in).
    pub allow_signup: bool,
}

impl OAuthProvider {
    pub fn github(client_id: String, client_secret: String) -> Self {
        Self {
            name: "github".into(),
            label: "GitHub".into(),
            client_id,
            client_secret,
            auth_url: "https://github.com/login/oauth/authorize".into(),
            token_url: "https://github.com/login/oauth/access_token".into(),
            userinfo_url: "https://api.github.com/user".into(),
            scopes: "read:user user:email".into(),
            allow_signup: true,
        }
    }

    pub fn google(client_id: String, client_secret: String) -> Self {
        Self {
            name: "google".into(),
            label: "Google".into(),
            client_id,
            client_secret,
            auth_url: "https://accounts.google.com/o/oauth2/v2/auth".into(),
            token_url: "https://oauth2.googleapis.com/token".into(),
            userinfo_url: "https://openidconnect.googleapis.com/v1/userinfo".into(),
            scopes: "openid email profile".into(),
            allow_signup: true,
        }
    }

    /// A generic OpenID Connect provider from its issuer's endpoints.
    pub fn oidc(issuer: &str, client_id: String, client_secret: String) -> Self {
        let base = issuer.trim_end_matches('/');
        Self {
            name: "oidc".into(),
            label: "SSO".into(),
            client_id,
            client_secret,
            auth_url: format!("{base}/authorize"),
            token_url: format!("{base}/oauth/token"),
            userinfo_url: format!("{base}/userinfo"),
            scopes: "openid email profile".into(),
            allow_signup: true,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SmtpConfig {
    /// `smtp://user:pass@host:587` (STARTTLS) or `smtps://...:465`.
    pub url: String,
    pub from: String,
}

#[derive(Debug, Clone, Default)]
pub struct Settings {
    /// The externally visible base URL (`https://flux.example.com`), used
    /// in webhook payloads, emails and OAuth redirects. Falls back to the
    /// request's host where one is available.
    pub public_url: Option<String>,
    /// Let webhooks target loopback and private addresses (tests, single
    /// machine setups). Off by default to prevent server-side request
    /// forgery.
    pub webhook_allow_private: bool,
    /// Key for encrypting CI secrets and two-factor secrets at rest.
    pub secret_key: Option<[u8; 32]>,
    pub smtp: Option<SmtpConfig>,
    pub oauth: Vec<OAuthProvider>,
    /// `host:port` of the SSH server, for clone URLs.
    pub ssh_host: Option<String>,
    /// Require two-factor auth for every account with a password.
    pub require_2fa: bool,
    /// Where the HTTP listener is, for links made outside a request (SSH,
    /// webhooks) when no public URL is set.
    pub listen_url: Option<String>,
}

impl Settings {
    pub fn base_url(&self) -> String {
        self.public_url
            .clone()
            .or_else(|| self.listen_url.clone())
            .unwrap_or_else(|| "http://localhost:7878".into())
            .trim_end_matches('/')
            .to_string()
    }
}
