use subtle::ConstantTimeEq;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Denied,
    Phone,
    Webhook,
    PairingClaim,
}

/// Same decision as `Lan.AuthorizeRequest`. A shared phone/webhook secret is not a phone key.
pub fn authorize(
    enabled: bool,
    phone_token: &str,
    webhook_token: &str,
    method: &str,
    path: &str,
    origin: &str,
    authorization: Option<&str>,
    query_token: Option<&str>,
) -> Access {
    if !enabled || !origin.is_empty() || !allowed_over_lan(path) {
        return Access::Denied;
    }
    let shared = !phone_token.is_empty() && phone_token == webhook_token;
    if !shared && !phone_token.is_empty() && bearer_matches(authorization, phone_token) {
        return Access::Phone;
    }
    if is_webhook(path)
        && !webhook_token.is_empty()
        && (bearer_matches(authorization, webhook_token) || query_matches(query_token, webhook_token))
    {
        return Access::Webhook;
    }
    if method.eq_ignore_ascii_case("POST") && path == "/api/pair/claim" {
        return Access::PairingClaim;
    }
    Access::Denied
}

fn bearer_matches(header: Option<&str>, token: &str) -> bool {
    let Some(header) = header else { return false };
    let Some(given) = header.strip_prefix("Bearer ") else { return false };
    fixed_eq(given.as_bytes(), token.as_bytes())
}

fn query_matches(given: Option<&str>, token: &str) -> bool {
    let Some(given) = given else { return false };
    !given.is_empty() && fixed_eq(given.as_bytes(), token.as_bytes())
}

fn fixed_eq(given: &[u8], expected: &[u8]) -> bool {
    if given.len() != expected.len() {
        return false;
    }
    given.ct_eq(expected).into()
}

pub fn allowed_over_lan(path: &str) -> bool {
    const EXACT: &[&str] = &[
        "/api/controls/spotify/state",
        "/api/controls/plex/state",
        "/api/controls/state",
        "/api/controls/listen",
        "/api/lyrics",
        "/api/dictate",
        "/api/steam/library",
        "/api/steam/cover",
        "/api/steam/launch",
        "/api/camera",
        "/api/spectrum",
        "/api/steam",
        "/api/steam/avatar",
        "/api/pair/claim",
        "/api/wake/macs",
    ];
    const PREFIX: &[&str] = &[
        "/api/controls/spotify/action",
        "/api/controls/plex/action",
        "/api/controls/radio-logo",
        "/api/controls/command",
        "/api/controls/obs",
        "/api/state",
        "/api/config",
        "/api/art",
        "/api/media",
        "/api/volume",
        "/api/quotas",
        "/api/work",
        "/api/procs",
        "/api/run",
        "/api/apps",
        "/api/shortcuts/icon",
        "/api/console",
        "/api/widget",
        "/api/calendar",
        "/api/feeds",
        "/api/photos",
        "/api/docker",
        "/api/obs",
        "/api/discord",
        "/api/batteries",
        "/api/ha/states",
        "/api/ha/toggle",
        "/api/approvals",
        "/api/rules/pause",
        "/api/phone/battery",
        "/api/stream",
        "/api/trackpad",
        "/api/push",
        "/api/notify",
        "/api/goto",
        "/api/refresh",
    ];
    EXACT.contains(&path) || PREFIX.iter().any(|prefix| child(path, prefix))
}

fn child(path: &str, prefix: &str) -> bool {
    path == prefix || path.strip_prefix(prefix).is_some_and(|rest| rest.starts_with('/'))
}

pub fn is_webhook(path: &str) -> bool {
    child(path, "/api/push")
        || child(path, "/api/notify")
        || child(path, "/api/goto")
        || child(path, "/api/refresh")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phone_key_opens_state_and_rejects_settings() {
        assert_eq!(
            authorize(true, "phone", "hook", "GET", "/api/state", "", Some("Bearer phone"), None),
            Access::Phone
        );
        assert_eq!(
            authorize(true, "phone", "hook", "GET", "/api/info", "", Some("Bearer phone"), None),
            Access::Denied
        );
        assert_eq!(
            authorize(true, "phone", "hook", "GET", "/api/state", "https://evil", Some("Bearer phone"), None),
            Access::Denied
        );
    }
}
