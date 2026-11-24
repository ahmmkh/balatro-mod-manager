//! Umami analytics client.
//!
//! All errors are logged and swallowed — analytics must never affect the user experience.
//! Events are posted to a self-hosted Umami instance configured at build time via the
//! `BMM_UMAMI_URL` (full `.../api/send` URL) and `BMM_UMAMI_WEBSITE_ID` environment variables.
//! If either is unset, analytics are a no-op and nothing is ever sent.

use serde_json::{Value, json};

const UMAMI_URL: Option<&str> = option_env!("BMM_UMAMI_URL");
const WEBSITE_ID_CONFIG: Option<&str> = option_env!("BMM_UMAMI_WEBSITE_ID");

/// Both values must be present and non-empty, otherwise analytics stay disabled.
fn config() -> Option<(&'static str, &'static str)> {
    match (UMAMI_URL, WEBSITE_ID_CONFIG) {
        (Some(url), Some(id)) if !url.is_empty() && !id.is_empty() => Some((url, id)),
        _ => None,
    }
}

/// Send a named event to Umami. Fails silently — never returns an error.
pub async fn send_event(name: &str, props: Value, app_version: &str) {
    let Some((url, website)) = config() else {
        return;
    };
    // Umami requires flat event data — merge app_version into props directly
    let mut data = match props {
        Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    data.insert(
        "app_version".to_string(),
        Value::String(app_version.to_string()),
    );

    let payload = json!({
        "type": "event",
        "payload": {
            "website": website,
            "hostname": "balatro-mod-manager",
            "language": "en",
            "url": "/",
            "name": name,
            "data": data,
        }
    });

    post(url, payload).await;
}

/// Send a page view to Umami so the Overview dashboard counts Visitors/Visits/Views.
/// Must be called once per session (i.e. on app startup). Fails silently.
pub async fn send_pageview() {
    let Some((url, website)) = config() else {
        return;
    };
    let payload = json!({
        "type": "event",
        "payload": {
            "website": website,
            "hostname": "balatro-mod-manager",
            "language": "en",
            "url": "/",
            "title": "Balatro Mod Manager"
        }
    });

    post(url, payload).await;
}

async fn post(url: &str, payload: Value) {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            log::warn!("Analytics: failed to build HTTP client: {e}");
            return;
        }
    };

    match client
        .post(url)
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            log::debug!("Analytics payload sent OK");
        }
        Ok(resp) => {
            log::warn!("Analytics payload failed (status {})", resp.status());
        }
        Err(e) => {
            log::warn!("Analytics request error: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn disabled_without_build_time_config() {
        // CI/dev builds don't set BMM_UMAMI_*, so nothing may ever be sent.
        if option_env!("BMM_UMAMI_URL").is_none() || option_env!("BMM_UMAMI_WEBSITE_ID").is_none() {
            assert!(config().is_none());
        }
    }

    const WEBSITE_ID: &str = "test-website-id";

    #[test]
    fn test_event_payload_structure() {
        let props = json!({ "mod": "TestMod" });
        let payload = json!({
            "type": "event",
            "payload": {
                "website": WEBSITE_ID,
                "hostname": "balatro-mod-manager",
                "language": "en",
                "url": "/",
                "name": "mod_installed",
                "data": {
                    "app_version": "0.4.1",
                    "props": props,
                }
            }
        });

        assert_eq!(payload["type"], "event");
        assert_eq!(payload["payload"]["website"], WEBSITE_ID);
        assert_eq!(payload["payload"]["name"], "mod_installed");
        assert_eq!(payload["payload"]["data"]["props"]["mod"], "TestMod");
    }

    #[test]
    fn test_website_id_is_set() {
        assert!(!WEBSITE_ID.is_empty());
    }
}
