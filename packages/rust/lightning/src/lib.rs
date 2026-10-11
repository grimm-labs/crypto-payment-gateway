//! Thin wrapper over the Breez SDK Spark so `jobs`/`api` never touch SDK
//! types: per-payment BOLT11 invoices, Lightning-address payouts, the wallet
//! event stream (received payments, finished sends), reconciliation and
//! catch-up listings, the wallet balance and its payment history.
//!
//! The SDK calls go through [`LightningOps`] (implemented for `BreezSdk`,
//! mocked in tests); [`LightningClient`] holds the request mapping and the
//! payment filtering on top of it.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use breez_sdk_spark::{
    connect, default_config, AssetFilter, BreezSdk, ConnectRequest, EventListener, GetInfoRequest,
    GetInfoResponse, GetPaymentRequest, GetPaymentResponse, InputType, ListPaymentsRequest,
    ListPaymentsResponse, LnurlPayRequest, LnurlPayResponse, Network, Payment, PaymentDetails,
    PaymentMethod, PaymentStatus, PaymentType, PrepareLnurlPayRequest, PrepareLnurlPayResponse,
    ReceivePaymentMethod, ReceivePaymentRequest, ReceivePaymentResponse, SdkError, SdkEvent, Seed,
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
    /// Resolves a Lightning address over HTTP (`/.well-known/lnurlp/…`).
    async fn parse(&self, input: &str) -> Result<InputType, SdkError>;
    /// Fetches the recipient's invoice and the exact fee, without paying.
    async fn prepare_lnurl_pay(
        &self,
        request: PrepareLnurlPayRequest,
    ) -> Result<PrepareLnurlPayResponse, SdkError>;
    /// Sends; returns as soon as the payment is accepted (usually Pending).
    async fn lnurl_pay(&self, request: LnurlPayRequest) -> Result<LnurlPayResponse, SdkError>;
    async fn get_payment(&self, request: GetPaymentRequest)
        -> Result<GetPaymentResponse, SdkError>;
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

    async fn parse(&self, input: &str) -> Result<InputType, SdkError> {
        BreezSdk::parse(self, input).await
    }

    async fn prepare_lnurl_pay(
        &self,
        request: PrepareLnurlPayRequest,
    ) -> Result<PrepareLnurlPayResponse, SdkError> {
        BreezSdk::prepare_lnurl_pay(self, request).await
    }

    async fn lnurl_pay(&self, request: LnurlPayRequest) -> Result<LnurlPayResponse, SdkError> {
        BreezSdk::lnurl_pay(self, request).await
    }

    async fn get_payment(
        &self,
        request: GetPaymentRequest,
    ) -> Result<GetPaymentResponse, SdkError> {
        BreezSdk::get_payment(self, request).await
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaymentDirection {
    Send,
    Receive,
}

impl PaymentDirection {
    pub fn as_api(&self) -> &'static str {
        match self {
            Self::Send => "send",
            Self::Receive => "receive",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalletPaymentStatus {
    Completed,
    Pending,
    Failed,
}

impl WalletPaymentStatus {
    pub fn as_api(&self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Pending => "pending",
            Self::Failed => "failed",
        }
    }
}

/// What the wallet listener forwards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LightningEvent {
    /// A settled incoming Lightning payment (invoice detection).
    Received(ReceivedPayment),
    /// An outgoing payment reached its final status (Completed or Failed) —
    /// whoever sent it: the wallet may be shared with another client.
    SendFinished(WalletPayment),
}

/// One entry of the wallet's payment history (any direction, any status).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalletPayment {
    /// SDK `Payment.id` — `deposits.txHash` for gateway invoices.
    pub id: String,
    pub direction: PaymentDirection,
    pub status: WalletPaymentStatus,
    pub amount_sats: u128,
    pub fees_sats: u128,
    /// Unix seconds.
    pub timestamp: u64,
    /// `lightning`, `spark`, `token`, `deposit`, `withdraw` or `unknown`.
    pub method: String,
    /// BOLT11, for Lightning payments.
    pub invoice: Option<String>,
    /// BOLT11 memo, for Lightning payments.
    pub description: Option<String>,
    /// On-chain txid, for deposits into / withdrawals from the wallet.
    pub tx_id: Option<String>,
    /// Lightning address, for payments made through LNURL-pay.
    pub ln_address: Option<String>,
    /// LNURL-pay comment sent with the payment.
    pub comment: Option<String>,
}

impl From<Payment> for WalletPayment {
    fn from(payment: Payment) -> Self {
        let (mut ln_address, mut comment) = (None, None);
        let (invoice, description, tx_id) = match payment.details {
            Some(PaymentDetails::Lightning {
                invoice,
                description,
                lnurl_pay_info,
                ..
            }) => {
                if let Some(info) = lnurl_pay_info {
                    ln_address = info.ln_address;
                    comment = info.comment;
                }
                (Some(invoice), description, None)
            }
            Some(PaymentDetails::Deposit { tx_id, .. } | PaymentDetails::Withdraw { tx_id }) => {
                (None, None, Some(tx_id))
            }
            _ => (None, None, None),
        };
        Self {
            id: payment.id,
            direction: match payment.payment_type {
                PaymentType::Send => PaymentDirection::Send,
                PaymentType::Receive => PaymentDirection::Receive,
            },
            status: match payment.status {
                PaymentStatus::Completed => WalletPaymentStatus::Completed,
                PaymentStatus::Pending => WalletPaymentStatus::Pending,
                PaymentStatus::Failed => WalletPaymentStatus::Failed,
            },
            amount_sats: payment.amount,
            fees_sats: payment.fees,
            timestamp: payment.timestamp,
            method: payment.method.to_string(),
            invoice,
            description,
            tx_id,
            ln_address,
            comment,
        }
    }
}

/// What a payout would cost, from the recipient's actual invoice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PayoutQuote {
    /// What the recipient receives.
    pub amount_sats: u64,
    /// Network fee, paid on top.
    pub fees_sats: u64,
}

/// A page of the wallet history, newest first. The SDK reports no total, so
/// paging is driven by `has_more`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalletPaymentsPage {
    pub payments: Vec<WalletPayment>,
    pub has_more: bool,
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

    /// Register `handler` for settled incoming Lightning payments and for
    /// outgoing payments reaching their final status. The handler runs on
    /// the SDK's event dispatch: keep it non-blocking (hand the work to a
    /// task or a channel). Returns the listener id.
    pub async fn subscribe<F>(&self, handler: F) -> String
    where
        F: Fn(LightningEvent) + Send + Sync + 'static,
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
            received.extend(page.into_iter().filter_map(settled_lightning_receive));
            if page_len < LIST_PAGE_SIZE as usize {
                return Ok(received);
            }
            offset += LIST_PAGE_SIZE;
        }
    }

    /// The wallet's bitcoin payment history (every direction and status —
    /// including funds unrelated to gateway invoices), newest first. Token
    /// payments are excluded: their amounts are not sats.
    pub async fn list_payments(&self, offset: u32, limit: u32) -> Result<WalletPaymentsPage> {
        let mut payments = self
            .ops
            .list_payments(ListPaymentsRequest {
                type_filter: None,
                status_filter: None,
                asset_filter: Some(AssetFilter::Bitcoin),
                payment_details_filter: None,
                from_timestamp: None,
                to_timestamp: None,
                offset: Some(offset),
                // One extra row tells whether another page exists.
                limit: Some(limit.saturating_add(1)),
                sort_ascending: Some(false),
            })
            .await
            .context("Breez list_payments failed")?
            .payments;
        let has_more = payments.len() > limit as usize;
        payments.truncate(limit as usize);
        Ok(WalletPaymentsPage {
            payments: payments.into_iter().map(WalletPayment::from).collect(),
            has_more,
        })
    }

    /// Pay a Lightning address (`user@domain`) `amount_sats`, fees on top.
    /// Anything else the SDK can parse (LNURL strings, invoices…) is
    /// refused. Returns as soon as the send is accepted — usually Pending;
    /// the final status arrives as a `SendFinished` event. A reused
    /// `idempotency_key` (UUID) returns the original payment, never a
    /// second send.
    pub async fn pay_lightning_address(
        &self,
        address: &str,
        amount_sats: u64,
        comment: Option<String>,
        idempotency_key: Option<String>,
    ) -> Result<WalletPayment> {
        // The SDK stores a keyed send under the key itself. Answer a replay
        // from storage before resolving the address again: the recipient may
        // be unreachable by now, and a failure here would invite the caller
        // to retry with a fresh key — paying twice.
        if let Some(key) = &idempotency_key {
            if let Some(existing) = self.get_payment(key).await? {
                // Keys share the wallet's payment id space: a key naming a
                // receive (or someone else's payment) is not a replay.
                if existing.direction != PaymentDirection::Send {
                    anyhow::bail!("Idempotency key collides with a non-send payment");
                }
                return Ok(existing);
            }
        }
        let prepare_response = self
            .prepare_lightning_address(address, amount_sats, comment)
            .await?;
        let response = self
            .ops
            .lnurl_pay(LnurlPayRequest {
                prepare_response,
                idempotency_key,
            })
            .await
            .context("Breez lnurl_pay failed")?;
        Ok(WalletPayment::from(response.payment))
    }

    /// Resolve the address and fetch the recipient's invoice for
    /// `amount_sats`, without paying: the fee the payout would cost.
    pub async fn quote_lightning_address(
        &self,
        address: &str,
        amount_sats: u64,
        comment: Option<String>,
    ) -> Result<PayoutQuote> {
        let prepared = self
            .prepare_lightning_address(address, amount_sats, comment)
            .await?;
        Ok(PayoutQuote {
            amount_sats: prepared.amount_sats,
            fees_sats: prepared.fee_sats,
        })
    }

    /// `user@domain` only (anything else the SDK parses is refused), fees on
    /// top of `amount_sats`.
    async fn prepare_lightning_address(
        &self,
        address: &str,
        amount_sats: u64,
        comment: Option<String>,
    ) -> Result<PrepareLnurlPayResponse> {
        let pay_request = match self
            .ops
            .parse(address)
            .await
            .context("Breez parse failed")?
        {
            InputType::LightningAddress(details) => details.pay_request,
            _ => anyhow::bail!("Input is not a Lightning address"),
        };
        self.ops
            .prepare_lnurl_pay(PrepareLnurlPayRequest {
                amount: u128::from(amount_sats),
                pay_request,
                comment,
                validate_success_action_url: None,
                token_identifier: None,
                conversion_options: None,
                // Default: fees on top, the recipient receives the amount.
                fee_policy: None,
            })
            .await
            .context("Breez prepare_lnurl_pay failed")
    }

    /// One wallet payment by SDK id; `None` when the wallet has no such
    /// payment.
    pub async fn get_payment(&self, payment_id: &str) -> Result<Option<WalletPayment>> {
        match self
            .ops
            .get_payment(GetPaymentRequest {
                payment_id: payment_id.to_string(),
            })
            .await
        {
            Ok(response) => Ok(Some(WalletPayment::from(response.payment))),
            // Unknown ids: the storage layer reports "no rows" (verified on
            // the pinned 0.26.1 SQLite backend), other backends "Not found".
            Err(SdkError::InvalidInput(_)) => Ok(None),
            Err(SdkError::StorageError(message)) if message.contains("no rows") => Ok(None),
            Err(err) => Err(anyhow::Error::new(err).context("Breez get_payment failed")),
        }
    }

    /// Outgoing bitcoin payments created at or after `unix_ts` that reached
    /// their final status, oldest first — the boot-time payout catch-up.
    pub async fn list_sends_since(&self, unix_ts: u64) -> Result<Vec<WalletPayment>> {
        let mut sends = Vec::new();
        let mut offset = 0u32;
        loop {
            let page = self
                .ops
                .list_payments(ListPaymentsRequest {
                    type_filter: Some(vec![PaymentType::Send]),
                    status_filter: Some(vec![PaymentStatus::Completed, PaymentStatus::Failed]),
                    asset_filter: Some(AssetFilter::Bitcoin),
                    payment_details_filter: None,
                    from_timestamp: Some(unix_ts),
                    to_timestamp: None,
                    offset: Some(offset),
                    limit: Some(LIST_PAGE_SIZE),
                    sort_ascending: Some(true),
                })
                .await
                .context("Breez list_payments failed")?
                .payments;
            let page_len = page.len();
            sends.extend(page.into_iter().map(WalletPayment::from));
            if page_len < LIST_PAGE_SIZE as usize {
                return Ok(sends);
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
        SdkEvent::PaymentSucceeded { payment } => settled_lightning_receive(payment),
        _ => None,
    }
}

/// What the gateway listens to: received Lightning payments, and outgoing
/// payments reaching a final status. Pending sends, failed receives and
/// token payments (not denominated in sats) are dropped.
pub fn lightning_event(event: SdkEvent) -> Option<LightningEvent> {
    match event {
        SdkEvent::PaymentSucceeded { payment } if payment.payment_type == PaymentType::Send => {
            finished_send(payment)
        }
        SdkEvent::PaymentFailed { payment } if payment.payment_type == PaymentType::Send => {
            finished_send(payment)
        }
        event => received_lightning_payment(event).map(LightningEvent::Received),
    }
}

fn finished_send(payment: Payment) -> Option<LightningEvent> {
    let is_final = matches!(payment.status, PaymentStatus::Completed | PaymentStatus::Failed);
    let is_token = payment.method == PaymentMethod::Token
        || matches!(payment.details, Some(PaymentDetails::Token { .. }));
    (is_final && !is_token).then(|| LightningEvent::SendFinished(WalletPayment::from(payment)))
}

/// A completed incoming payment made over Lightning, else `None`.
fn settled_lightning_receive(payment: Payment) -> Option<ReceivedPayment> {
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
    F: Fn(LightningEvent) + Send + Sync + 'static,
{
    async fn on_event(&self, event: SdkEvent) {
        if let Some(event) = lightning_event(event) {
            (self.handler)(event);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use breez_sdk_spark::{
        BitcoinNetwork, Bolt11Invoice, Bolt11InvoiceDetails, FeePolicy, LightningAddressDetails,
        LnurlPayInfo, LnurlPayRequestDetails, PaymentMethod, PaymentRequestSource,
        PaymentStatus, PaymentType, SparkHtlcDetails, SparkHtlcStatus,
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
    async fn subscribe_forwards_received_payments_and_finished_sends() {
        let registered: Arc<Mutex<Option<Box<dyn EventListener>>>> = Arc::default();
        let mut ops = MockLightningOps::new();
        let slot = registered.clone();
        ops.expect_add_event_listener().times(1).returning(move |listener| {
            *slot.lock().unwrap() = Some(listener);
            "listener-1".into()
        });

        let seen: Arc<Mutex<Vec<LightningEvent>>> = Arc::default();
        let sink = seen.clone();
        let client = LightningClient::new(Arc::new(ops));
        let id = client
            .subscribe(move |event| sink.lock().unwrap().push(event))
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
        assert_eq!(seen.len(), 2);
        match &seen[0] {
            LightningEvent::SendFinished(sent) => {
                assert_eq!(sent.id, "out");
                assert_eq!(sent.status, WalletPaymentStatus::Completed);
            }
            other => panic!("expected a finished send, got {other:?}"),
        }
        match &seen[1] {
            LightningEvent::Received(received) => {
                assert_eq!(received.payment_id, "in");
                assert_eq!(received.invoice, "lnbc1in");
            }
            other => panic!("expected a received payment, got {other:?}"),
        }
    }

    #[test]
    fn send_events_report_only_final_bitcoin_outcomes() {
        let sent = |status| {
            with_status(
                payment("out", PaymentType::Send, Some(lightning_details("lnbc1out"))),
                status,
            )
        };
        let finished = |event| match lightning_event(event) {
            Some(LightningEvent::SendFinished(payment)) => Some(payment.status),
            _ => None,
        };
        assert_eq!(
            finished(SdkEvent::PaymentSucceeded { payment: sent(PaymentStatus::Completed) }),
            Some(WalletPaymentStatus::Completed)
        );
        assert_eq!(
            finished(SdkEvent::PaymentFailed { payment: sent(PaymentStatus::Failed) }),
            Some(WalletPaymentStatus::Failed)
        );
        // Not final yet.
        assert_eq!(
            finished(SdkEvent::PaymentPending { payment: sent(PaymentStatus::Pending) }),
            None
        );
        // A failed *receive* is not a payout.
        assert!(lightning_event(SdkEvent::PaymentFailed {
            payment: with_status(received("in", "lnbc1in"), PaymentStatus::Failed),
        })
        .is_none());
        // Token sends are not denominated in sats.
        let mut token_send = sent(PaymentStatus::Completed);
        token_send.method = PaymentMethod::Token;
        assert!(lightning_event(SdkEvent::PaymentSucceeded { payment: token_send }).is_none());
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

    fn with_status(mut payment: Payment, status: PaymentStatus) -> Payment {
        payment.status = status;
        payment
    }

    #[tokio::test]
    async fn history_lists_bitcoin_payments_newest_first_with_paging() {
        let mut ops = MockLightningOps::new();
        ops.expect_list_payments()
            .withf(|request| {
                request.offset == Some(40)
                    // One extra row tells whether another page exists.
                    && request.limit == Some(21)
                    && request.sort_ascending == Some(false)
                    && matches!(request.asset_filter, Some(AssetFilter::Bitcoin))
                    // The whole wallet: every direction and status.
                    && request.type_filter.is_none()
                    && request.status_filter.is_none()
                    && request.from_timestamp.is_none()
            })
            .times(1)
            .returning(|_| {
                Ok(ListPaymentsResponse {
                    payments: (0..21).map(|i| received(&format!("p{i}"), "lnbc1")).collect(),
                })
            });
        let client = LightningClient::new(Arc::new(ops));
        let page = client.list_payments(40, 20).await.unwrap();
        assert_eq!(page.payments.len(), 20);
        assert!(page.has_more);
        assert_eq!(page.payments[0].id, "p0");
    }

    #[tokio::test]
    async fn history_last_page_has_no_more() {
        let mut ops = MockLightningOps::new();
        ops.expect_list_payments()
            .returning(|_| Ok(ListPaymentsResponse { payments: vec![received("only", "lnbc1")] }));
        let client = LightningClient::new(Arc::new(ops));
        let page = client.list_payments(0, 20).await.unwrap();
        assert_eq!(page.payments.len(), 1);
        assert!(!page.has_more);
    }

    #[test]
    fn history_entry_keeps_direction_status_amounts_and_references() {
        let mut lightning = received("pay-1", "lnbc1500u1invoice");
        lightning.fees = 3;
        lightning.details = match lightning.details {
            Some(PaymentDetails::Lightning { invoice, destination_pubkey, htlc_details, .. }) => {
                Some(PaymentDetails::Lightning {
                    description: Some("order #123".into()),
                    invoice,
                    destination_pubkey,
                    htlc_details,
                    lnurl_pay_info: None,
                    lnurl_withdraw_info: None,
                    lnurl_receive_metadata: None,
                    conversion_info: None,
                })
            }
            other => other,
        };
        assert_eq!(
            WalletPayment::from(lightning),
            WalletPayment {
                id: "pay-1".into(),
                direction: PaymentDirection::Receive,
                status: WalletPaymentStatus::Completed,
                amount_sats: 150_000,
                fees_sats: 3,
                timestamp: 1_700_000_000,
                method: "lightning".into(),
                invoice: Some("lnbc1500u1invoice".into()),
                description: Some("order #123".into()),
                tx_id: None,
                ln_address: None,
                comment: None,
            }
        );

        let sent = with_status(
            payment("pay-2", PaymentType::Send, Some(lightning_details("lnbc1out"))),
            PaymentStatus::Failed,
        );
        let sent = WalletPayment::from(sent);
        assert_eq!(sent.direction, PaymentDirection::Send);
        assert_eq!(sent.status, WalletPaymentStatus::Failed);
        assert_eq!(sent.invoice.as_deref(), Some("lnbc1out"));

        let mut deposit = with_status(
            payment(
                "pay-3",
                PaymentType::Receive,
                Some(PaymentDetails::Deposit { tx_id: "btc-tx".into(), vout: 1 }),
            ),
            PaymentStatus::Pending,
        );
        deposit.method = PaymentMethod::Deposit;
        let deposit = WalletPayment::from(deposit);
        assert_eq!(deposit.status, WalletPaymentStatus::Pending);
        assert_eq!(deposit.method, "deposit");
        assert_eq!(deposit.tx_id.as_deref(), Some("btc-tx"));
        assert_eq!(deposit.invoice, None);

        let bare = WalletPayment::from(payment("pay-4", PaymentType::Receive, None));
        assert_eq!((bare.invoice, bare.description, bare.tx_id), (None, None, None));
    }

    fn pay_request_details() -> LnurlPayRequestDetails {
        LnurlPayRequestDetails {
            callback: "https://example.com/lnurlp/alice/callback".into(),
            min_sendable: 1_000,
            max_sendable: 1_000_000_000,
            metadata_str: "[]".into(),
            comment_allowed: 120,
            domain: "example.com".into(),
            url: "https://example.com/.well-known/lnurlp/alice".into(),
            address: Some("alice@example.com".into()),
            allows_nostr: None,
            nostr_pubkey: None,
        }
    }

    fn prepared(amount_sats: u64, fee_sats: u64, comment: Option<String>) -> PrepareLnurlPayResponse {
        PrepareLnurlPayResponse {
            amount_sats,
            comment,
            pay_request: pay_request_details(),
            fee_sats,
            invoice_details: Bolt11InvoiceDetails {
                amount_msat: Some(amount_sats * 1_000),
                description: None,
                description_hash: None,
                expiry: 600,
                invoice: Bolt11Invoice {
                    bolt11: "lnbc1500u1recipient".into(),
                    source: PaymentRequestSource {
                        bip_21_uri: None,
                        bip_353_address: None,
                    },
                },
                min_final_cltv_expiry_delta: 18,
                network: BitcoinNetwork::Bitcoin,
                payee_pubkey: "02ab".into(),
                payment_hash: "hash".into(),
                payment_secret: "secret".into(),
                routing_hints: vec![],
                timestamp: 1_700_000_000,
            },
            success_action: None,
            conversion_estimate: None,
            fee_policy: FeePolicy::FeesExcluded,
        }
    }

    fn lightning_address_input() -> InputType {
        InputType::LightningAddress(LightningAddressDetails {
            address: "alice@example.com".into(),
            pay_request: pay_request_details(),
        })
    }

    fn unknown_payment() -> SdkError {
        SdkError::StorageError("Underlying implementation error: Query returned no rows".into())
    }

    #[tokio::test]
    async fn pays_a_lightning_address_with_fees_on_top() {
        let mut ops = MockLightningOps::new();
        ops.expect_get_payment()
            .withf(|request| request.payment_id == "6f1c7a8e-2f43-4d55-9a8b-1c2d3e4f5a6b")
            .times(1)
            .returning(|_| Err(unknown_payment()));
        ops.expect_parse()
            .withf(|input| input == "alice@example.com")
            .times(1)
            .returning(|_| Ok(lightning_address_input()));
        ops.expect_prepare_lnurl_pay()
            .withf(|request| {
                request.amount == 150_000
                    && request.comment.as_deref() == Some("thanks")
                    && request.pay_request.address.as_deref() == Some("alice@example.com")
                    // Default policy: fees on top, the recipient gets the amount.
                    && request.fee_policy.is_none()
                    && request.token_identifier.is_none()
                    && request.conversion_options.is_none()
            })
            .times(1)
            .returning(|request| Ok(prepared(150_000, 3, request.comment)));
        ops.expect_lnurl_pay()
            .withf(|request| {
                request.idempotency_key.as_deref() == Some("6f1c7a8e-2f43-4d55-9a8b-1c2d3e4f5a6b")
                    && request.prepare_response.amount_sats == 150_000
                    && request.prepare_response.fee_sats == 3
            })
            .times(1)
            .returning(|_| {
                Ok(LnurlPayResponse {
                    payment: with_status(
                        payment("sent-1", PaymentType::Send, Some(lightning_details("lnbc1500u1recipient"))),
                        PaymentStatus::Pending,
                    ),
                    success_action: None,
                })
            });

        let client = LightningClient::new(Arc::new(ops));
        let sent = client
            .pay_lightning_address(
                "alice@example.com",
                150_000,
                Some("thanks".into()),
                Some("6f1c7a8e-2f43-4d55-9a8b-1c2d3e4f5a6b".into()),
            )
            .await
            .unwrap();
        assert_eq!(sent.id, "sent-1");
        assert_eq!(sent.direction, PaymentDirection::Send);
        assert_eq!(sent.status, WalletPaymentStatus::Pending);
    }

    #[tokio::test]
    async fn a_replayed_idempotency_key_returns_the_original_send_without_contacting_anyone() {
        let mut ops = MockLightningOps::new();
        // The SDK stores a keyed send under the key itself.
        ops.expect_get_payment()
            .withf(|request| request.payment_id == "6f1c7a8e-2f43-4d55-9a8b-1c2d3e4f5a6b")
            .times(1)
            .returning(|_| {
                Ok(GetPaymentResponse {
                    payment: payment(
                        "6f1c7a8e-2f43-4d55-9a8b-1c2d3e4f5a6b",
                        PaymentType::Send,
                        Some(lightning_details("lnbc1original")),
                    ),
                })
            });
        // No parse / prepare / pay expectations: a replay must not resolve
        // the address again (it may be down) nor send anything.
        let client = LightningClient::new(Arc::new(ops));
        let replayed = client
            .pay_lightning_address(
                "alice@example.com",
                150_000,
                None,
                Some("6f1c7a8e-2f43-4d55-9a8b-1c2d3e4f5a6b".into()),
            )
            .await
            .unwrap();
        assert_eq!(replayed.id, "6f1c7a8e-2f43-4d55-9a8b-1c2d3e4f5a6b");
        assert_eq!(replayed.status, WalletPaymentStatus::Completed);
    }

    #[tokio::test]
    async fn an_idempotency_key_naming_a_receive_is_not_a_replay() {
        let mut ops = MockLightningOps::new();
        ops.expect_get_payment().times(1).returning(|_| {
            Ok(GetPaymentResponse {
                payment: received("6f1c7a8e-2f43-4d55-9a8b-1c2d3e4f5a6b", "lnbc1incoming"),
            })
        });
        // Nothing may be resolved, prepared or sent.
        let client = LightningClient::new(Arc::new(ops));
        let err = client
            .pay_lightning_address(
                "alice@example.com",
                1_000,
                None,
                Some("6f1c7a8e-2f43-4d55-9a8b-1c2d3e4f5a6b".into()),
            )
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("non-send"));
    }

    #[tokio::test]
    async fn a_quote_prepares_without_paying() {
        let mut ops = MockLightningOps::new();
        ops.expect_parse().times(1).returning(|_| Ok(lightning_address_input()));
        ops.expect_prepare_lnurl_pay()
            .withf(|request| request.amount == 150_000 && request.fee_policy.is_none())
            .times(1)
            .returning(|request| Ok(prepared(150_000, 7, request.comment)));
        // No `lnurl_pay` expectation: a quote never sends.
        let client = LightningClient::new(Arc::new(ops));
        let quote = client
            .quote_lightning_address("alice@example.com", 150_000, None)
            .await
            .unwrap();
        assert_eq!(quote, PayoutQuote { amount_sats: 150_000, fees_sats: 7 });
    }

    #[tokio::test]
    async fn only_lightning_addresses_are_paid() {
        let mut ops = MockLightningOps::new();
        // A bare LNURL-pay string resolves to the same pay request, but the
        // gateway only pays `user@domain` addresses: nothing is prepared.
        ops.expect_parse()
            .returning(|_| Ok(InputType::LnurlPay(pay_request_details())));
        let client = LightningClient::new(Arc::new(ops));
        let err = client
            .pay_lightning_address("lnurl1dp68gurn8ghj7", 1_000, None, None)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("not a Lightning address"));
    }

    #[tokio::test]
    async fn a_failed_quote_sends_nothing() {
        let mut ops = MockLightningOps::new();
        ops.expect_parse().returning(|_| Ok(lightning_address_input()));
        ops.expect_prepare_lnurl_pay()
            .returning(|_| Err(SdkError::InvalidInput("Amount below minimum".into())));
        // No `lnurl_pay` expectation: calling it would fail the test.
        let client = LightningClient::new(Arc::new(ops));
        assert!(client
            .pay_lightning_address("alice@example.com", 1, None, None)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn get_payment_maps_found_missing_and_errors() {
        let mut ops = MockLightningOps::new();
        ops.expect_get_payment()
            .withf(|request| request.payment_id == "known")
            .returning(|_| Ok(GetPaymentResponse { payment: received("known", "lnbc1") }));
        ops.expect_get_payment()
            .withf(|request| request.payment_id == "missing")
            .returning(|_| Err(SdkError::InvalidInput("Not found".into())));
        // What 0.26.1's SQLite storage actually returns for an unknown id.
        ops.expect_get_payment()
            .withf(|request| request.payment_id == "unknown")
            .returning(|_| {
                Err(SdkError::StorageError(
                    "Underlying implementation error: Query returned no rows".into(),
                ))
            });
        ops.expect_get_payment()
            .withf(|request| request.payment_id == "boom")
            .returning(|_| Err(SdkError::Generic("storage unavailable".into())));
        let client = LightningClient::new(Arc::new(ops));
        assert_eq!(client.get_payment("known").await.unwrap().unwrap().id, "known");
        assert!(client.get_payment("missing").await.unwrap().is_none());
        assert!(client.get_payment("unknown").await.unwrap().is_none());
        assert!(client.get_payment("boom").await.is_err());
    }

    #[tokio::test]
    async fn catch_up_lists_final_sends_since_a_timestamp() {
        let mut ops = MockLightningOps::new();
        ops.expect_list_payments()
            .withf(|request| {
                request.from_timestamp == Some(1_700_000_000)
                    && matches!(request.type_filter.as_deref(), Some([PaymentType::Send]))
                    && matches!(
                        request.status_filter.as_deref(),
                        Some([PaymentStatus::Completed, PaymentStatus::Failed])
                    )
                    && matches!(request.asset_filter, Some(AssetFilter::Bitcoin))
                    && request.sort_ascending == Some(true)
                    && request.offset == Some(0)
            })
            .times(1)
            .returning(|_| {
                Ok(ListPaymentsResponse {
                    payments: vec![payment("s1", PaymentType::Send, Some(lightning_details("lnbc1")))],
                })
            });
        let client = LightningClient::new(Arc::new(ops));
        let sends = client.list_sends_since(1_700_000_000).await.unwrap();
        assert_eq!(sends.len(), 1);
        assert_eq!(sends[0].id, "s1");
        assert_eq!(sends[0].direction, PaymentDirection::Send);
    }

    #[test]
    fn history_entry_exposes_the_paid_lightning_address_and_comment() {
        let mut sent = payment("s1", PaymentType::Send, Some(lightning_details("lnbc1")));
        if let Some(PaymentDetails::Lightning { lnurl_pay_info, .. }) = &mut sent.details {
            *lnurl_pay_info = Some(LnurlPayInfo {
                ln_address: Some("alice@example.com".into()),
                comment: Some("thanks".into()),
                domain: Some("example.com".into()),
                metadata: None,
                processed_success_action: None,
                raw_success_action: None,
            });
        }
        let entry = WalletPayment::from(sent);
        assert_eq!(entry.ln_address.as_deref(), Some("alice@example.com"));
        assert_eq!(entry.comment.as_deref(), Some("thanks"));
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
