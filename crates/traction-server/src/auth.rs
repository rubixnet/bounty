//! Accounts: the `AuthProvider` seam, the S1 dev provider, and the pure
//! pieces of the Pairing approval page (HTML, CSRF).

use crate::token::secure_eq;

/// Who the provider says the browser user is. Users are keyed by `sub`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub sub: String,
    pub display_name: String,
}

/// What a provider does with a Pairing login request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthStep {
    /// Authenticated: show the approval page.
    Identified(Identity),
    /// Send the browser elsewhere first (OIDC, from S2).
    Redirect(String),
}

pub trait AuthProvider: Send + Sync {
    fn id(&self) -> &'static str;
    /// Begins authentication for a Pairing (`pair` is the pairing code).
    fn begin(&self, pair: &str) -> AuthStep;
}

/// Logs everyone in as one configured user. For development and tests only;
/// S2 adds Hack Club OIDC beside it.
pub struct DevAuth {
    pub sub: String,
    pub display_name: String,
}

impl AuthProvider for DevAuth {
    fn id(&self) -> &'static str {
        "dev"
    }

    fn begin(&self, _pair: &str) -> AuthStep {
        AuthStep::Identified(Identity {
            sub: self.sub.clone(),
            display_name: self.display_name.clone(),
        })
    }
}

pub const CSRF_COOKIE: &str = "traction_csrf";

/// Double-submit CSRF: the cookie and the form field must both exist and
/// match. A cross-site page can neither read nor set the SameSite cookie.
pub fn csrf_ok(cookie: Option<&str>, field: Option<&str>) -> bool {
    match (cookie, field) {
        (Some(cookie), Some(field)) if cookie.len() >= 32 => {
            secure_eq(cookie.as_bytes(), field.as_bytes())
        }
        _ => false,
    }
}

/// A foreign `Origin` is refused; a missing one is allowed (non-browser).
pub fn origin_ok(origin: Option<&str>, host: Option<&str>) -> bool {
    let Some(origin) = origin else {
        return true;
    };
    let authority = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"));
    matches!((authority, host), (Some(a), Some(h)) if a == h)
}

/// The value of cookie `name` in a `Cookie` header.
pub fn cookie_value<'a>(header: Option<&'a str>, name: &str) -> Option<&'a str> {
    header?.split(';').find_map(|pair| {
        let (key, value) = pair.trim().split_once('=')?;
        (key == name).then_some(value)
    })
}

fn escape(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '&' => "&amp;".to_owned(),
            '<' => "&lt;".to_owned(),
            '>' => "&gt;".to_owned(),
            '"' => "&quot;".to_owned(),
            '\'' => "&#39;".to_owned(),
            c => c.to_string(),
        })
        .collect()
}

fn page(body: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>Traction</title></head><body>{body}</body></html>"
    )
}

/// The one plain HTML page of Pairing: the code, the device and a
/// CSRF-protected **Approve device** button. Viewing it approves nothing.
pub fn approval_page(pair_code: &str, device_label: &str, user: &str, csrf: &str) -> String {
    page(&format!(
        "<h1>Approve device</h1>\
         <p>Signed in as <strong>{user}</strong>.</p>\
         <p>Pairing code: <strong>{code}</strong></p>\
         <p>Device: {label}</p>\
         <p>Only approve if this code matches the one shown by <code>traction login</code>.</p>\
         <form method=\"post\" action=\"/auth/approve\">\
         <input type=\"hidden\" name=\"pair\" value=\"{code}\">\
         <input type=\"hidden\" name=\"csrf\" value=\"{csrf}\">\
         <button type=\"submit\">Approve device</button></form>",
        user = escape(user),
        code = escape(pair_code),
        label = escape(device_label),
        csrf = escape(csrf),
    ))
}

pub fn message_page(message: &str) -> String {
    page(&format!("<h1>Traction</h1><p>{}</p>", escape(message)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dev_provider_identifies_the_configured_user() {
        let provider = DevAuth {
            sub: "u1".into(),
            display_name: "U".into(),
        };
        assert_eq!(
            provider.begin("CODE"),
            AuthStep::Identified(Identity {
                sub: "u1".into(),
                display_name: "U".into()
            })
        );
    }

    #[test]
    fn csrf_needs_a_matching_cookie_and_field() {
        let token = "a".repeat(64);
        assert!(csrf_ok(Some(&token), Some(&token)));
        assert!(!csrf_ok(None, Some(&token)));
        assert!(!csrf_ok(Some(&token), None));
        assert!(!csrf_ok(Some(&token), Some(&"b".repeat(64))));
        // A short or empty value never passes, even when both match.
        assert!(!csrf_ok(Some(""), Some("")));
        assert!(!csrf_ok(Some("abc"), Some("abc")));
    }

    #[test]
    fn a_foreign_origin_is_refused() {
        assert!(origin_ok(None, Some("srv:8080")));
        assert!(origin_ok(Some("http://srv:8080"), Some("srv:8080")));
        assert!(origin_ok(Some("https://srv"), Some("srv")));
        assert!(!origin_ok(Some("http://evil.example"), Some("srv:8080")));
        assert!(!origin_ok(Some("null"), Some("srv:8080")));
    }

    #[test]
    fn cookies_are_picked_by_name() {
        assert_eq!(
            cookie_value(Some("a=1; traction_csrf=xyz; b=2"), CSRF_COOKIE),
            Some("xyz")
        );
        assert_eq!(cookie_value(Some("a=1"), CSRF_COOKIE), None);
        assert_eq!(cookie_value(None, CSRF_COOKIE), None);
    }

    #[test]
    fn the_page_escapes_what_it_shows_and_posts_to_approve() {
        let html = approval_page("AB-CD", "<script>x</script>", "me&you", "tok");
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("me&amp;you"));
        assert!(html.contains("Approve device"));
        assert!(html.contains("method=\"post\""));
        assert!(html.contains("name=\"csrf\" value=\"tok\""));
    }
}
