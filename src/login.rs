//! Password login for `/_admin`: one shared password (`RMAPPROXY_ADMIN_PASSWORD`), random session
//! tokens kept in memory (single admin pod; a restart logs everybody out).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use parking_lot::Mutex;

pub const COOKIE: &str = "rmapproxy_admin";
const SESSION: Duration = Duration::from_secs(12 * 3600);
/// Sessions beyond this drop the oldest.
const SESSIONS_MAX: usize = 1000;
/// Delay of a wrong password's answer.
const FAIL_DELAY: Duration = Duration::from_secs(1);

pub struct Login {
    password: String,
    /// Token -> expiry.
    sessions: Mutex<HashMap<String, Instant>>,
}

/// Equality without an early exit, so response timing doesn't reveal how much of a guessed token
/// matched (only its length). black_box keeps the optimiser from short-circuiting the fold.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && std::hint::black_box(a.iter().zip(b).fold(0u8, |acc, (x, y)| std::hint::black_box(acc | (x ^ y)))) == 0
}

impl Login {
    pub fn new(password: String) -> Login {
        Login { password, sessions: Mutex::default() }
    }

    /// `RMAPPROXY_ADMIN_PASSWORD`: unset = no login. Set but empty is an error, not an open admin.
    pub fn from_env() -> Result<Option<Login>> {
        match std::env::var("RMAPPROXY_ADMIN_PASSWORD") {
            Ok(p) if p.trim().is_empty() => bail!("RMAPPROXY_ADMIN_PASSWORD is set but empty"),
            // Secrets created with `echo` end in a newline.
            Ok(p) => Ok(Some(Login::new(p.trim().to_string()))),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(e) => bail!("RMAPPROXY_ADMIN_PASSWORD: {e}"),
        }
    }

    pub fn logged_in(&self, h: &HeaderMap) -> bool {
        let Some(token) = session_cookie(h) else { return false };
        self.sessions.lock().get(token).is_some_and(|exp| *exp > Instant::now())
    }

    pub async fn login(&self, h: &HeaderMap, password: &str) -> Response {
        if !ct_eq(password.as_bytes(), self.password.as_bytes()) {
            tracing::warn!("admin: wrong password");
            tokio::time::sleep(FAIL_DELAY).await;
            return page(StatusCode::UNAUTHORIZED, true);
        }
        let token = format!("{:032x}{:032x}", rand::random::<u128>(), rand::random::<u128>());
        let now = Instant::now();
        {
            let mut s = self.sessions.lock();
            s.retain(|_, exp| *exp > now);
            if s.len() >= SESSIONS_MAX
                && let Some(oldest) = s.iter().min_by_key(|(_, exp)| **exp).map(|(t, _)| t.clone())
            {
                s.remove(&oldest);
            }
            s.insert(token.clone(), now + SESSION);
        }
        redirect("/_admin", Some(cookie(h, &token, SESSION.as_secs())))
    }

    pub fn logout(&self, h: &HeaderMap) -> Response {
        if let Some(t) = session_cookie(h) {
            self.sessions.lock().remove(t);
        }
        redirect("/_admin/login", Some(cookie(h, "", 0)))
    }
}

fn session_cookie(h: &HeaderMap) -> Option<&str> {
    h.get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|c| c.trim().strip_prefix(COOKIE)?.strip_prefix('='))
}

/// Secure behind the TLS-terminating ingress only: plain http on localhost still works.
fn cookie(h: &HeaderMap, token: &str, max_age: u64) -> String {
    let https = h.get("x-forwarded-proto").is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"https"));
    let secure = if https { "; Secure" } else { "" };
    format!("{COOKIE}={token}; Path=/_admin; Max-Age={max_age}; HttpOnly; SameSite=Lax{secure}")
}

pub fn redirect(to: &'static str, set_cookie: Option<String>) -> Response {
    let mut r = (StatusCode::SEE_OTHER, [(header::LOCATION, to), (header::CACHE_CONTROL, "no-store")]).into_response();
    if let Some(v) = set_cookie.and_then(|c| HeaderValue::from_str(&c).ok()) {
        r.headers_mut().insert(header::SET_COOKIE, v);
    }
    r
}

/// The login form (styled by the GUI's admin.css, which is served without a session).
pub fn page(status: StatusCode, failed: bool) -> Response {
    let error = if failed { r#"<p id="login-error" class="login-error" role="alert">Feil passord.</p>"# } else { "" };
    let described = if failed { r#" aria-invalid="true" aria-describedby="login-error""# } else { "" };
    let html = format!(
        r#"<!DOCTYPE html>
<html lang="no">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Logg inn – Cache-admin</title>
  <link rel="stylesheet" href="/_admin/admin.css">
</head>
<body class="login">
  <main>
    <h1>Cache-admin <span class="sub">rMapproxy</span></h1>
    <form method="post" action="/_admin/login">
      <input type="text" name="username" value="admin" autocomplete="username" hidden>
      <div class="field">
        <label for="password">Passord</label>
        <input id="password" type="password" name="password" autocomplete="current-password" required autofocus{described}>
        {error}
      </div>
      <button type="submit" class="primary">Logg inn</button>
    </form>
  </main>
</body>
</html>
"#
    );
    (status, [(header::CACHE_CONTROL, "no-store")], Html(html)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_compare_is_exact() {
        assert!(ct_eq(b"s3cret-token", b"s3cret-token"));
        assert!(!ct_eq(b"s3cret-token", b"s3cret-tokeN"));
        assert!(!ct_eq(b"s3cret-token", b"s3cret"));
        assert!(!ct_eq(b"", b"x"));
        assert!(ct_eq(b"", b""));
    }

    #[test]
    fn sessions_expire_and_cookies_are_secure_behind_tls() {
        let l = Login::new("p".into());
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, format!("a=b; {COOKIE}=tok").parse().unwrap());
        assert!(!l.logged_in(&h));
        l.sessions.lock().insert("tok".into(), Instant::now() + SESSION);
        assert!(l.logged_in(&h));
        l.sessions.lock().insert("tok".into(), Instant::now());
        assert!(!l.logged_in(&h));
        assert!(!cookie(&h, "t", 1).contains("Secure"));
        h.insert("x-forwarded-proto", "https".parse().unwrap());
        assert!(cookie(&h, "t", 1).ends_with("; Secure"));
    }
}
