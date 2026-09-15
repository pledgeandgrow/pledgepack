// Build event webhooks (#105)
//
// POST build results to external services on completion.
// Supports Slack/Discord notification format.

use crate::config::WebhookConfig;
use hmac::Mac;
use serde::{Deserialize, Serialize};
use tracing::info;
use thiserror::Error;

/// Build event payload sent to webhook endpoints
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildEvent {
    pub event: String,
    pub success: bool,
    pub duration_ms: u128,
    pub modules_built: usize,
    pub modules_cached: usize,
    pub bundle_size: usize,
    pub chunk_count: usize,
    pub timestamp: u64,
    pub error: Option<String>,
}

/// Errors that can occur when sending a webhook.
#[derive(Debug, Error)]
pub enum WebhookError {
    #[error("serialization failed: {0}")]
    Serialization(String),
    #[error("crypto error: {0}")]
    Crypto(String),
    #[error("delivery failed: {0}")]
    Delivery(String),
    #[error("HTTP {0}")]
    HttpStatus(u16),
}

/// Send build event webhook
pub async fn send_webhook(
    config: &WebhookConfig,
    event: BuildEvent,
) -> Result<(), WebhookError> {
    if !config.enabled {
        return Ok(());
    }

    let url = if event.success {
        config.on_build.as_ref()
    } else {
        config.on_error.as_ref().or(config.on_build.as_ref())
    };

    let Some(url) = url else {
        return Ok(());
    };

    // Detect Slack/Discord webhook URL format
    let is_slack = url.contains("hooks.slack.com");
    let is_discord = url.contains("discord.com/api/webhooks");

    let body = if is_slack {
        format_slack_payload(&event)
    } else if is_discord {
        format_discord_payload(&event)
    } else {
        serde_json::to_string(&event).map_err(|e| WebhookError::Serialization(e.to_string()))?
    };

    // HMAC-SHA256 signing if a secret is configured
    let signature = if let Some(ref secret) = config.secret {
        let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes())
            .map_err(|e| WebhookError::Crypto(e.to_string()))?;
        mac.update(body.as_bytes());
        Some(hex::encode(mac.finalize().into_bytes()))
    } else {
        None
    };

    let url = url.clone();
    let headers = config.headers.clone();

    tokio::task::spawn_blocking(move || {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_secs(10)))
            .build()
            .new_agent();
        let mut req = agent
            .post(&url)
            .header("Content-Type", "application/json");

        if let Some(ref sig) = signature {
            req = req.header("X-Webhook-Signature", sig);
        }

        for (key, value) in &headers {
            req = req.header(key, value);
        }

        match req.send(&body) {
            Ok(resp) => {
                let status = resp.status().as_u16();
                if (200..300).contains(&status) {
                    info!("Webhook sent to {}", url);
                    Ok(())
                } else {
                    Err(WebhookError::HttpStatus(status))
                }
            }
            Err(e) => Err(WebhookError::Delivery(e.to_string())),
        }
    })
    .await
    .map_err(|e| WebhookError::Delivery(e.to_string()))??;

    Ok(())
}

/// Format event as Slack message
fn format_slack_payload(event: &BuildEvent) -> String {
    let status_emoji = if event.success {
        "white_check_mark"
    } else {
        "x"
    };
    let color = if event.success { "good" } else { "danger" };

    let error_field = event
        .error
        .as_ref()
        .map(|e| format!(r#",{{"title":"Error","value":"{}","short":false}}"#, e))
        .unwrap_or_default();

    format!(
        r#"{{"attachments":[{{"color":"{}","fields":[{{"title":"Status","value":":{}: {}","short":true}},{{"title":"Duration","value":"{}ms","short":true}},{{"title":"Modules","value":"{} built, {} cached","short":true}},{{"title":"Bundle Size","value":"{} bytes","short":true}}{}]}}]}}"#,
        color,
        status_emoji,
        if event.success {
            "Build succeeded"
        } else {
            "Build failed"
        },
        event.duration_ms,
        event.modules_built,
        event.modules_cached,
        event.bundle_size,
        error_field,
    )
}

/// Format event as Discord message
fn format_discord_payload(event: &BuildEvent) -> String {
    let color = if event.success { 3066993 } else { 15158332 };
    let title = if event.success {
        "Build Succeeded"
    } else {
        "Build Failed"
    };

    let error_field = event
        .error
        .as_ref()
        .map(|e| format!(r#","description":"{}""#, e))
        .unwrap_or_default();

    format!(
        r#"{{"embeds":[{{"title":"{}","color":{}{},"fields":[{{"name":"Duration","value":"{}ms","inline":true}},{{"name":"Modules","value":"{} built, {} cached","inline":true}},{{"name":"Bundle Size","value":"{} bytes","inline":true}}]}}]}}"#,
        title,
        color,
        error_field,
        event.duration_ms,
        event.modules_built,
        event.modules_cached,
        event.bundle_size,
    )
}
