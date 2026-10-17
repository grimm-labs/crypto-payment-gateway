//! Operational alerts (spec §7.10, §7.12): recorded once per dedup key per
//! day, logged at `warn`, and fanned out to the webhook endpoints
//! subscribed to their event.

use anyhow::Result;
use config::ALERT_DEDUP_SECS;
use db::{AlertRow, NewAlert};
use domain::{AlertType, Network, Token, WebhookEventType};
use serde_json::Value;
use tracing::warn;

use crate::{enqueue_webhook, webhooks, AppState};

/// The deduplication key: conditions (gas reserve low, cap exceeded) per
/// type/network/token — one alert a day while it holds; events per subject
/// (each address-book entry, each stuck withdrawal is reported).
pub fn dedup_key(alert: AlertType, network: Option<Network>, token: Option<Token>, subject: Option<&str>) -> String {
    match subject {
        Some(subject) => format!("{}:{subject}", alert.as_db()),
        None => format!(
            "{}:{}:{}",
            alert.as_db(),
            network.map(|n| n.as_db()).unwrap_or("-"),
            token.map(|t| t.as_db()).unwrap_or("-")
        ),
    }
}

/// Record and notify an alert; `None` when an identical one was raised
/// within the dedup window.
pub async fn raise(
    state: &AppState,
    alert: AlertType,
    network: Option<Network>,
    token: Option<Token>,
    subject: Option<&str>,
    details: Value,
) -> Result<Option<AlertRow>> {
    let key = dedup_key(alert, network, token, subject);
    let Some(row) = state
        .db
        .record_alert(
            &NewAlert {
                alert_type: alert,
                network,
                token,
                dedup_key: &key,
                details,
            },
            ALERT_DEDUP_SECS,
        )
        .await?
    else {
        return Ok(None);
    };
    warn!(
        alert = alert.as_api(),
        network = network.map(|n| n.as_api()),
        token = token.map(|t| t.as_api()),
        details = %row.details,
        "ALERT"
    );
    // Alerts are the merchant's (spec §10.1): merchant endpoints only.
    let event = WebhookEventType::for_alert(alert);
    for endpoint in webhooks::endpoints_for(state, event, &webhooks::SubjectOwners::merchant()).await? {
        enqueue_webhook(state, event, &row.id, &endpoint.id).await?;
    }
    Ok(Some(row))
}

/// Webhook body of an alert.
pub fn alert_payload(row: &AlertRow) -> Result<Value> {
    let alert = row.alert_type()?;
    let api = |value: &Option<String>, parse: fn(&str) -> Option<&'static str>| value.as_deref().and_then(parse);
    Ok(serde_json::json!({
        "event_type": WebhookEventType::for_alert(alert).payload_event_type(),
        "data": {
            "alert_id": row.id,
            "type": alert.as_api(),
            "network": api(&row.network, |n| Network::from_db(n).ok().map(|n| n.as_api())),
            "token": api(&row.token, |t| Token::from_db(t).ok().map(|t| t.as_api())),
            "details": row.details,
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conditions_dedup_per_asset_and_events_per_subject() {
        assert_eq!(
            dedup_key(AlertType::GasReserveLow, Some(Network::Tron), Some(Token::Trx), None),
            "GAS_RESERVE_LOW:TRON:TRX"
        );
        assert_ne!(
            dedup_key(AlertType::GasReserveLow, Some(Network::Tron), Some(Token::Trx), None),
            dedup_key(AlertType::GasReserveLow, Some(Network::Ethereum), Some(Token::Eth), None)
        );
        assert_eq!(dedup_key(AlertType::WithdrawalStuck, Some(Network::Bitcoin), None, Some("w1")), "WITHDRAWAL_STUCK:w1");
        assert_ne!(
            dedup_key(AlertType::AddressBookEntryAdded, None, None, Some("e1")),
            dedup_key(AlertType::AddressBookEntryAdded, None, None, Some("e2"))
        );
    }

    #[test]
    fn alert_payload_shape() {
        let row = AlertRow {
            id: "a1".into(),
            alert_type: "GAS_RESERVE_LOW".into(),
            network: Some("TRON".into()),
            token: Some("TRX".into()),
            dedup_key: "k".into(),
            details: serde_json::json!({ "balance": "40", "alert_below": "200" }),
            created_at: chrono::Utc::now(),
        };
        assert_eq!(
            alert_payload(&row).unwrap(),
            serde_json::json!({
                "event_type": "gas_reserve_low",
                "data": {
                    "alert_id": "a1", "type": "gas_reserve_low", "network": "tron", "token": "TRX",
                    "details": { "balance": "40", "alert_below": "200" },
                },
            })
        );
    }
}
