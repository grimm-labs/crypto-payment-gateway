//! Thin wrapper over the Breez SDK Spark so `jobs`/`api` never touch SDK
//! types. Receive-only: per-payment BOLT11 invoices, the "payment received"
//! event stream, a reconciliation listing and the wallet balance.
//!
//! The SDK calls go through [`LightningOps`] (implemented for `BreezSdk`,
//! mocked in tests); [`LightningClient`] holds the request mapping and the
//! payment filtering on top of it.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use breez_sdk_spark::{
    connect, default_config, BreezSdk, ConnectRequest, EventListener, GetInfoRequest,
    GetInfoResponse, ListPaymentsRequest, ListPaymentsResponse, Network, Payment,
    PaymentDetails, PaymentStatus, PaymentType, ReceivePaymentMethod, ReceivePaymentRequest,
    ReceivePaymentResponse, SdkError, SdkEvent, Seed,
};
use tracing::info;

/// Page size of the reconciliation listing.
const LIST_PAGE_SIZE: u32 = 100;

/// The Breez SDK operations the gateway relies on.
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait LightningOps: Send + Sync {
    async fn receive_payment(
        &self,
        request: ReceivePaymentRequest,
    ) -> Result<ReceivePaymentResponse, SdkError>;
    async fn list_payments(
        &self,
        request: ListPaymentsRequest,
    ) -> Result<ListPaymentsResponse, SdkError>;
    async fn get_info(&self, request: GetInfoRequest) -> Result<GetInfoResponse, SdkError>;
    /// Returns the listener id.
    async fn add_event_listener(&self, listener: Box<dyn EventListener>) -> String;
    async fn disconnect(&self) -> Result<(), SdkError>;
}

#[async_trait]
impl LightningOps for BreezSdk {
    async fn receive_payment(
        &self,
        request: ReceivePaymentRequest,
    ) -> Result<ReceivePaymentResponse, SdkError> {
        BreezSdk::receive_payment(self, request).await
    }

    async fn list_payments(
        &self,
        request: ListPaymentsRequest,
    ) -> Result<ListPaymentsResponse, SdkError> {
        BreezSdk::list_payments(self, request).await
    }

    async fn get_info(&self, request: GetInfoRequest) -> Result<GetInfoResponse, SdkError> {
        BreezSdk::get_info(self, request).await
    }

    async fn add_event_listener(&self, listener: Box<dyn EventListener>) -> String {
        BreezSdk::add_event_listener(self, listener).await
    }

    async fn disconnect(&self) -> Result<(), SdkError> {
        BreezSdk::disconnect(self).await
    }
}

/// What `connect` needs. `mnemonic` is `WALLET_MNEMONIC` — never log it.
pub struct LightningConfig {
    pub api_key: String,
    pub mnemonic: String,
    /// Absolute path; must persist across restarts.
    pub storage_dir: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedInvoice {
    /// The BOLT11 payment request.
    pub payment_request: String,
    pub fee_sats: u128,
}

/// A settled incoming Lightning payment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceivedPayment {
    /// The BOLT11 that was paid — matches `deposit_addresses.address`.
    pub invoice: String,
    /// SDK `Payment.id`: stable, resolvable later through `get_payment`.
    pub payment_id: String,
    pub amount_sats: u128,
}

#[derive(Clone)]
pub struct LightningClient {
    ops: Arc<dyn LightningOps>,
}

impl LightningClient {
    pub fn new(ops: Arc<dyn LightningOps>) -> Self {
        Self { ops }
    }

    /// Connect the mainnet Breez SDK, seeded with the gateway mnemonic.
    pub async fn connect(config: LightningConfig) -> Result<Self> {
        if !Path::new(&config.storage_dir).is_absolute() {
            anyhow::bail!("Breez storage dir must be an absolute path");
        }
        std::fs::create_dir_all(&config.storage_dir)
            .with_context(|| format!("Cannot create Breez storage dir {}", config.storage_dir))?;

        let mut sdk_config = default_config(Network::Mainnet);
        sdk_config.api_key = Some(config.api_key);
        let sdk = connect(ConnectRequest {
            config: sdk_config,
            seed: Seed::Mnemonic {
                mnemonic: config.mnemonic,
                passphrase: None,
            },
            storage_dir: config.storage_dir,
        })
        .await
        .context("Breez SDK connect failed")?;
        info!("Breez SDK connected");
        Ok(Self::new(Arc::new(sdk)))
    }

    /// Fixed-amount BOLT11 invoice expiring after `expiry_secs`.
    pub async fn create_invoice(
        &self,
        amount_sats: u64,
        description: &str,
        expiry_secs: u32,
    ) -> Result<CreatedInvoice> {
        let response = self
            .ops
            .receive_payment(ReceivePaymentRequest {
                payment_method: ReceivePaymentMethod::Bolt11Invoice {
                    description: description.to_string(),
                    amount_sats: Some(amount_sats),
                    expiry_secs: Some(expiry_secs),
                    payment_hash: None,
                    receiver_identity_public_key: None,
                },
            })
            .await
            .context("Breez receive_payment failed")?;
        Ok(CreatedInvoice {
            payment_request: response.payment_request,
            fee_sats: response.fee,
        })
    }

    /// Register `handler` for every settled incoming Lightning payment.
    /// The handler runs on the SDK's event dispatch: keep it non-blocking
    /// (hand the work to a task or a channel). Returns the listener id.
    pub async fn subscribe<F>(&self, handler: F) -> String
    where
        F: Fn(ReceivedPayment) + Send + Sync + 'static,
    {
        self.ops
            .add_event_listener(Box::new(PaymentForwarder { handler }))
            .await
    }

    /// Settled incoming Lightning payments created at or after `unix_ts`,
    /// oldest first. Always filtered and paginated — never a full scan.
    pub async fn list_received_since(&self, unix_ts: u64) -> Result<Vec<ReceivedPayment>> {
        let mut received = Vec::new();
        let mut offset = 0u32;
        loop {
            let page = self
                .ops
                .list_payments(ListPaymentsRequest {
                    type_filter: Some(vec![PaymentType::Receive]),
                    status_filter: Some(vec![PaymentStatus::Completed]),
                    asset_filter: None,
                    payment_details_filter: None,
                    from_timestamp: Some(unix_ts),
                    to_timestamp: None,
                    offset: Some(offset),
                    limit: Some(LIST_PAGE_SIZE),
                    // Ascending keeps offsets stable while new payments land.
                    sort_ascending: Some(true),
                })
                .await
                .context("Breez list_payments failed")?
                .payments;
            let page_len = page.len();
            received.extend(page.into_iter().filter_map(received_lightning));
            if page_len < LIST_PAGE_SIZE as usize {
                return Ok(received);
            }
            offset += LIST_PAGE_SIZE;
        }
    }

    /// Wallet balance. Pass `ensure_synced = false` on hot paths.
    pub async fn balance_sats(&self, ensure_synced: bool) -> Result<u64> {
        let info = self
            .ops
            .get_info(GetInfoRequest {
                ensure_synced: Some(ensure_synced),
            })
            .await
            .context("Breez get_info failed")?;
        Ok(info.balance_sats)
    }

    /// Close the SDK (and its SQLite storage) cleanly.
    pub async fn disconnect(&self) -> Result<()> {
        self.ops.disconnect().await.context("Breez disconnect failed")
    }
}

/// `PaymentSucceeded` for an incoming Lightning payment, else `None`: the
/// event also fires for outgoing payments, and the wallet may receive funds
/// through other methods (Spark transfers, on-chain deposits).
pub fn received_lightning_payment(event: SdkEvent) -> Option<ReceivedPayment> {
    match event {
        SdkEvent::PaymentSucceeded { payment } => received_lightning(payment),
        _ => None,
    }
}

fn received_lightning(payment: Payment) -> Option<ReceivedPayment> {
    if payment.payment_type != PaymentType::Receive || payment.status != PaymentStatus::Completed
    {
        return None;
    }
    match payment.details {
        Some(PaymentDetails::Lightning { invoice, .. }) => Some(ReceivedPayment {
            invoice,
            payment_id: payment.id,
            amount_sats: payment.amount,
        }),
        _ => None,
    }
}

struct PaymentForwarder<F> {
    handler: F,
}

#[async_trait]
impl<F> EventListener for PaymentForwarder<F>
where
    F: Fn(ReceivedPayment) + Send + Sync + 'static,
{
    async fn on_event(&self, event: SdkEvent) {
        if let Some(payment) = received_lightning_payment(event) {
            (self.handler)(payment);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use breez_sdk_spark::{
        PaymentMethod, PaymentStatus, PaymentType, SparkHtlcDetails, SparkHtlcStatus,
    };

    fn lightning_details(invoice: &str) -> PaymentDetails {
        PaymentDetails::Lightning {
            description: None,
            invoice: invoice.to_string(),
            destination_pubkey: "02ab".into(),
            htlc_details: SparkHtlcDetails {
                payment_hash: "hash".into(),
                preimage: Some("preimage".into()),
                expiry_time: 0,
                status: SparkHtlcStatus::PreimageShared,
            },
            lnurl_pay_info: None,
            lnurl_withdraw_info: None,
            lnurl_receive_metadata: None,
            conversion_info: None,
        }
    }

    fn payment(id: &str, payment_type: PaymentType, details: Option<PaymentDetails>) -> Payment {
        Payment {
            id: id.into(),
            payment_type,
            status: PaymentStatus::Completed,
            amount: 150_000,
            fees: 0,
            timestamp: 1_700_000_000,
            method: PaymentMethod::Lightning,
            details,
            conversion_details: None,
        }
    }

    fn received(id: &str, invoice: &str) -> Payment {
        payment(id, PaymentType::Receive, Some(lightning_details(invoice)))
    }

    #[tokio::test]
    async fn create_invoice_maps_amount_description_and_expiry() {
        let mut ops = MockLightningOps::new();
        ops.expect_receive_payment()
            .withf(|request| {
                matches!(
                    &request.payment_method,
                    ReceivePaymentMethod::Bolt11Invoice {
                        description,
                        amount_sats: Some(150_000),
                        expiry_secs: Some(3600),
                        payment_hash: None,
                        receiver_identity_public_key: None,
                    } if description == "order #123"
                )
            })
            .times(1)
            .returning(|_| {
                Ok(ReceivePaymentResponse {
                    payment_request: "lnbc1500u1invoice".into(),
                    fee: 2,
                    cross_chain_info: None,
                })
            });

        let client = LightningClient::new(Arc::new(ops));
        let invoice = client.create_invoice(150_000, "order #123", 3600).await.unwrap();
        assert_eq!(
            invoice,
            CreatedInvoice {
                payment_request: "lnbc1500u1invoice".into(),
                fee_sats: 2,
            }
        );
    }

    #[tokio::test]
    async fn create_invoice_surfaces_sdk_errors() {
        let mut ops = MockLightningOps::new();
        ops.expect_receive_payment()
            .returning(|_| Err(SdkError::Generic("spark operator unreachable".into())));
        let client = LightningClient::new(Arc::new(ops));
        assert!(client.create_invoice(1, "", 60).await.is_err());
    }

    #[test]
    fn event_filter_keeps_only_received_lightning_payments() {
        let ok = received_lightning_payment(SdkEvent::PaymentSucceeded {
            payment: received("pay-1", "lnbc1abc"),
        });
        assert_eq!(
            ok,
            Some(ReceivedPayment {
                invoice: "lnbc1abc".into(),
                payment_id: "pay-1".into(),
                amount_sats: 150_000,
            })
        );
    }

    #[test]
    fn event_filter_drops_send_side_payments() {
        let sent = payment("pay-2", PaymentType::Send, Some(lightning_details("lnbc1out")));
        assert_eq!(
            received_lightning_payment(SdkEvent::PaymentSucceeded { payment: sent }),
            None
        );
    }

    #[test]
    fn event_filter_drops_non_lightning_details() {
        let deposit = payment(
            "pay-3",
            PaymentType::Receive,
            Some(PaymentDetails::Deposit { tx_id: "txid".into(), vout: 0 }),
        );
        let no_details = payment("pay-4", PaymentType::Receive, None);
        for payment in [deposit, no_details] {
            assert_eq!(
                received_lightning_payment(SdkEvent::PaymentSucceeded { payment }),
                None
            );
        }
    }

    #[test]
    fn event_filter_ignores_other_events() {
        assert_eq!(received_lightning_payment(SdkEvent::Synced), None);
        assert_eq!(
            received_lightning_payment(SdkEvent::PaymentPending {
                payment: received("pay-5", "lnbc1pending"),
            }),
            None
        );
        assert_eq!(
            received_lightning_payment(SdkEvent::PaymentFailed {
                payment: received("pay-6", "lnbc1failed"),
            }),
            None
        );
    }

    #[tokio::test]
    async fn subscribe_forwards_only_received_lightning_payments() {
        let registered: Arc<Mutex<Option<Box<dyn EventListener>>>> = Arc::default();
        let mut ops = MockLightningOps::new();
        let slot = registered.clone();
        ops.expect_add_event_listener().times(1).returning(move |listener| {
            *slot.lock().unwrap() = Some(listener);
            "listener-1".into()
        });

        let seen: Arc<Mutex<Vec<ReceivedPayment>>> = Arc::default();
        let sink = seen.clone();
        let client = LightningClient::new(Arc::new(ops));
        let id = client
            .subscribe(move |payment| sink.lock().unwrap().push(payment))
            .await;
        assert_eq!(id, "listener-1");

        let listener = registered.lock().unwrap().take().expect("listener registered");
        listener.on_event(SdkEvent::Synced).await;
        listener
            .on_event(SdkEvent::PaymentSucceeded {
                payment: payment("out", PaymentType::Send, Some(lightning_details("lnbc1out"))),
            })
            .await;
        listener
            .on_event(SdkEvent::PaymentSucceeded { payment: received("in", "lnbc1in") })
            .await;

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].payment_id, "in");
        assert_eq!(seen[0].invoice, "lnbc1in");
    }

    #[tokio::test]
    async fn reconciliation_passes_timestamp_and_receive_filters() {
        let mut ops = MockLightningOps::new();
        ops.expect_list_payments()
            .withf(|request| {
                request.from_timestamp == Some(1_700_000_000)
                    && matches!(request.type_filter.as_deref(), Some([PaymentType::Receive]))
                    && matches!(request.status_filter.as_deref(), Some([PaymentStatus::Completed]))
                    && request.limit == Some(LIST_PAGE_SIZE)
                    && request.offset == Some(0)
            })
            .times(1)
            .returning(|_| {
                Ok(ListPaymentsResponse {
                    payments: vec![
                        received("pay-1", "lnbc1one"),
                        payment(
                            "spark",
                            PaymentType::Receive,
                            Some(PaymentDetails::Deposit { tx_id: "t".into(), vout: 1 }),
                        ),
                    ],
                })
            });

        let client = LightningClient::new(Arc::new(ops));
        let payments = client.list_received_since(1_700_000_000).await.unwrap();
        assert_eq!(
            payments,
            vec![ReceivedPayment {
                invoice: "lnbc1one".into(),
                payment_id: "pay-1".into(),
                amount_sats: 150_000,
            }]
        );
    }

    #[tokio::test]
    async fn reconciliation_paginates_until_a_short_page() {
        let mut ops = MockLightningOps::new();
        ops.expect_list_payments()
            .withf(|request| request.offset == Some(0))
            .times(1)
            .returning(|_| {
                Ok(ListPaymentsResponse {
                    payments: (0..LIST_PAGE_SIZE)
                        .map(|i| received(&format!("p{i}"), &format!("lnbc{i}")))
                        .collect(),
                })
            });
        ops.expect_list_payments()
            .withf(|request| request.offset == Some(LIST_PAGE_SIZE) && request.from_timestamp == Some(42))
            .times(1)
            .returning(|_| Ok(ListPaymentsResponse { payments: vec![received("last", "lnbclast")] }));

        let client = LightningClient::new(Arc::new(ops));
        let payments = client.list_received_since(42).await.unwrap();
        assert_eq!(payments.len(), LIST_PAGE_SIZE as usize + 1);
        assert_eq!(payments.last().unwrap().payment_id, "last");
    }

    #[tokio::test]
    async fn balance_reports_wallet_sats_with_requested_sync_mode() {
        let mut ops = MockLightningOps::new();
        ops.expect_get_info()
            .withf(|request| request.ensure_synced == Some(false))
            .times(1)
            .returning(|_| {
                Ok(GetInfoResponse {
                    identity_pubkey: "02ab".into(),
                    balance_sats: 21_000,
                    token_balances: Default::default(),
                })
            });
        let client = LightningClient::new(Arc::new(ops));
        assert_eq!(client.balance_sats(false).await.unwrap(), 21_000);
    }
}
