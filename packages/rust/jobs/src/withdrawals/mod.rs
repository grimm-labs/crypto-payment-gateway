//! Withdrawals (spec §7): admission (validation, authorization, destination
//! rules, limits and funds, under per-asset locks — invariant 5), quotes,
//! approval, cancellation, the admin bump/abandon actions and webhooks.
//! The outbound engine (`crate::outbound`) executes admitted withdrawals.

use std::time::Duration;

use anyhow::Context;
use chrono::Utc;
use config::{WITHDRAWAL_APPROVAL_EXPIRY_SECS, WITHDRAWAL_SELF_APPROVAL_DELAY_SECS};
use db::{NewWithdrawal, WithdrawalRow, WithdrawalUpdate};
use domain::{
    format_amount, next_withdrawal_status, raw_u128_to_string, reserved_amount, CancelReason, DestinationKind,
    Network, Owner, ProjectBalance, Token, WebhookEventType, WithdrawalEvent, WithdrawalPolicy, WithdrawalStatus,
};
use serde_json::{json, Value};
use tracing::{error, info, warn};

use crate::audit::{self, Actor};
use crate::{enqueue_webhook, fees, ledger, treasury, webhooks, AppState};

pub mod resolve;
pub mod rules;

pub use resolve::{DestinationInput, ResolvedDestination, SolanaRoute};
pub use rules::{Balance, Requester, RuleError};

// `WithdrawalError` doubles as the transfer error (same HTTP mapping).

/// A withdrawal request that passed the HTTP-level checks (shapes,
/// amount syntax, Idempotency-Key format).
#[derive(Debug, Clone)]
pub struct WithdrawalInput {
    pub network: Network,
    pub token: Token,
    pub amount_raw: u128,
    pub destination: DestinationInput,
    pub reference: Option<String>,
    /// Lightning-address comment.
    pub comment: Option<String>,
    pub metadata: Value,
    /// The deprecated payout alias: the withdrawal id (its SDK idempotency
    /// key) is the `Idempotency-Key` itself, as in the former payout flow —
    /// a request racing between an old and a new container during a deploy
    /// reaches the wallet with one key, never two.
    pub id_is_idempotency_key: bool,
    /// Staff only: the owner the withdrawal is debited from (a project, or
    /// the merchant funds). A key's withdrawals always belong to its
    /// project; `None` for a staff member means the merchant funds.
    pub owner: Option<Owner>,
}

/// The owner of a withdrawal `requester` makes (spec §8.1): a key's project,
/// or the admin's explicit choice.
pub fn withdrawal_owner(requester: &Requester, input: &WithdrawalInput) -> Owner {
    match requester {
        Requester::ApiKey { project_id, .. } => Owner::from_project_id(project_id.as_deref()),
        Requester::Staff { .. } => input.owner.clone().unwrap_or(Owner::Merchant),
    }
}

#[derive(Debug)]
pub enum WithdrawalError {
    /// 403 / 422.
    Rule(RuleError),
    /// 400.
    BadRequest(String),
    NotFound,
    /// 409: the withdrawal is not in a status allowing the action.
    Conflict(String),
    /// 422: the Idempotency-Key was used by another caller or with other
    /// parameters.
    KeyReused,
    /// 503.
    Frozen,
    /// 503: the network has no provider (or Lightning is off).
    NetworkDisabled(String),
    /// 502: a provider failed (fee estimate, account lookup); the detail is
    /// logged.
    Provider(anyhow::Error),
    Internal(anyhow::Error),
}

impl From<anyhow::Error> for WithdrawalError {
    fn from(value: anyhow::Error) -> Self {
        Self::Internal(value)
    }
}

impl From<RuleError> for WithdrawalError {
    fn from(value: RuleError) -> Self {
        Self::Rule(value)
    }
}

impl From<domain::DomainError> for WithdrawalError {
    fn from(value: domain::DomainError) -> Self {
        Self::Internal(value.into())
    }
}

/// What a withdrawal would cost and whether this caller may make it.
#[derive(Debug, Clone)]
pub struct WithdrawalQuote {
    pub network: Network,
    pub token: Token,
    pub amount_raw: u128,
    pub fee_estimate_raw: u128,
    pub fee_token: Token,
    pub approval_required: bool,
    pub destination: String,
    pub destination_kind: DestinationKind,
    pub address_book_entry_id: Option<String>,
    /// `None` when the destination is acceptable for this caller.
    pub destination_refusal: Option<RuleError>,
    /// Of the withdrawn token on its network; `None` when unreadable, and
    /// always for a merchant key (a custody figure, staff only).
    pub available_raw: Option<u128>,
    /// Gas still available for the fee (when paid in another asset); staff
    /// only, like `available_raw`.
    pub fee_available_raw: Option<u128>,
    /// Merchant keys: the project's available balance of the asset (spec
    /// §8.3), signed, and whether the project check would pass. `None` for
    /// staff (admins may exceed the balance).
    pub project_available_raw: Option<i128>,
    pub project_check_ok: Option<bool>,
}

/// The project check of a key withdrawal (spec §8.3, invariant 3): what the
/// withdrawal reserves against the project balance — amount plus the fee
/// cap when the fee is paid in the same asset.
fn project_reservation(input: &WithdrawalInput, fee_estimate: u128) -> u128 {
    ProjectBalance::reservation_for(
        input.amount_raw,
        input.token,
        input.network.fee_token(),
        rules::fee_cap(fee_estimate),
    )
}

fn project_insufficient() -> RuleError {
    RuleError::Refused {
        code: "project_insufficient_funds",
        message: "Insufficient project balance for this withdrawal".into(),
    }
}

#[derive(Debug, Clone)]
pub struct CreatedWithdrawal {
    pub row: WithdrawalRow,
    /// Same key, caller and parameters: the stored withdrawal, nothing new.
    pub replayed: bool,
}

/// The fee a withdrawal is quoted (in its network's fee token).
pub async fn estimate_fee(
    state: &AppState,
    network: Network,
    token: Token,
    destination: &ResolvedDestination,
    amount_raw: u128,
    comment: Option<&str>,
) -> Result<u128, WithdrawalError> {
    let provider = WithdrawalError::Provider;
    match network {
        Network::Ethereum => {
            let treasury = state.wallet.treasury_address(network).map_err(anyhow::Error::from)?;
            let plan = fees::plan_eth(&state.chain, &treasury, token, &state.config.chain.usdt_eth, &destination.destination, amount_raw)
                .await
                .map_err(provider)?;
            Ok(plan.max_fee())
        }
        Network::Tron => {
            let treasury = state.wallet.treasury_address(network).map_err(anyhow::Error::from)?;
            let plan = fees::plan_tron(&state.chain, &treasury, token, &state.config.chain.usdt_tron, &destination.destination, amount_raw)
                .await
                .map_err(provider)?;
            Ok(plan.max_fee())
        }
        Network::Solana => {
            let treasury = state.wallet.treasury_address(network).map_err(anyhow::Error::from)?;
            let route = destination.solana.as_ref().context("unresolved Solana route")?;
            let plan = fees::plan_solana(&state.chain, &treasury, route).await.map_err(provider)?;
            Ok(plan.max_fee())
        }
        Network::Bitcoin => {
            let amount = u64::try_from(amount_raw).context("amount out of range")?;
            match fees::plan_btc(state, &state.chain, &destination.destination, amount).await {
                Ok((selection, _)) => Ok(u128::from(selection.fee)),
                // Not enough inputs: the funds check refuses it; quote a
                // typical two-input transaction meanwhile.
                Err(err) => {
                    warn!("Bitcoin fee estimate without a full coin selection: {err:#}");
                    let rate = chain::btc_tx::EsploraRpc::fee_rate(
                        &state.chain,
                        config::BTC_FEE_TARGET_BLOCKS,
                        config::BTC_MIN_FEE_RATE_SAT_VB,
                        config::BTC_MAX_FEE_RATE_SAT_VB,
                    )
                    .await
                    .map_err(provider)?;
                    Ok(u128::from(chain::btc_tx::estimate_vsize(2, &[34, 22]) * rate))
                }
            }
        }
        Network::Lightning => {
            let lightning = state
                .lightning
                .as_ref()
                .ok_or_else(|| WithdrawalError::NetworkDisabled(state.network_disabled_message(network).into()))?;
            let amount = u64::try_from(amount_raw).context("amount out of range")?;
            let quote = tokio::time::timeout(Duration::from_secs(config::PAYOUT_TIMEOUT_SECS), async {
                match destination.kind {
                    DestinationKind::Bolt11 => lightning.quote_bolt11(&destination.destination, amount).await,
                    _ => {
                        lightning
                            .quote_lightning_address(&destination.destination, amount, comment.map(str::to_string))
                            .await
                    }
                }
            })
            .await
            .map_err(|_| provider(anyhow::anyhow!("Lightning quote timed out")))?
            .map_err(provider)?;
            Ok(u128::from(quote.fees_sats))
        }
    }
}

/// The token and fee balances of (network, token), cached or `fresh`.
async fn balances(
    state: &AppState,
    admission: Option<&mut db::WithdrawalAdmission>,
    network: Network,
    token: Token,
    exclude_id: Option<&str>,
    fresh: bool,
) -> Result<(Balance, Option<Balance>), WithdrawalError> {
    let fee_token = network.fee_token();
    let token_confirmed = treasury::confirmed_balance(state, network, token, fresh)
        .await
        .map_err(WithdrawalError::Provider)?;
    let fee_confirmed = if fee_token == token {
        None
    } else {
        Some(
            treasury::confirmed_balance(state, network, fee_token, fresh)
                .await
                .map_err(WithdrawalError::Provider)?,
        )
    };
    let (token_res, fee_res) = match admission {
        Some(admission) => (
            admission.reservations(network, token, exclude_id).await?,
            admission.reservations(network, fee_token, exclude_id).await?,
        ),
        None => (
            state.db.withdrawal_reservations(network, token).await?,
            state.db.withdrawal_reservations(network, fee_token).await?,
        ),
    };
    let token_floor = treasury::gas_floor(state, network, token).await?;
    let token_balance = Balance {
        confirmed: token_confirmed,
        reserved: token_res.amount,
        // Fees paid in this same token are reserved from it too.
        reserved_fees: if fee_token == token { token_res.fees } else { 0 },
        floor: token_floor,
    };
    let fee_balance = match fee_confirmed {
        None => None,
        Some(confirmed) => Some(Balance {
            confirmed,
            reserved: fee_res.amount,
            reserved_fees: fee_res.fees,
            floor: treasury::gas_floor(state, network, fee_token).await?,
        }),
    };
    Ok((token_balance, fee_balance))
}

fn ensure_network_enabled(state: &AppState, network: Network) -> Result<(), WithdrawalError> {
    if state.is_network_enabled(network) {
        Ok(())
    } else {
        Err(WithdrawalError::NetworkDisabled(format!(
            "{} is not configured: {}",
            network.as_api(),
            state.network_disabled_message(network)
        )))
    }
}

/// Everything admission checks before taking the locks.
struct Prepared {
    owner: Owner,
    resolved: ResolvedDestination,
    approval_required: bool,
    fee_estimate: u128,
    settings: db::AssetSettingsRow,
}

async fn prepare(
    state: &AppState,
    requester: &Requester,
    input: &WithdrawalInput,
) -> Result<(Prepared, Option<RuleError>), WithdrawalError> {
    // The owner first (pure checks): gas assets are always merchant funds
    // (projects never hold TRX/SOL), and a named project must exist.
    let owner = withdrawal_owner(requester, input);
    if input.token.is_gas_only() && owner != Owner::Merchant {
        return Err(RuleError::Refused {
            code: "gas_asset_merchant_only",
            message: format!("{} withdrawals always draw on the merchant funds", input.token.as_api()),
        }
        .into());
    }
    if let Owner::Project(id) = &owner {
        if state.db.find_project(id).await?.is_none() {
            return Err(WithdrawalError::BadRequest("Unknown project".into()));
        }
    }
    ensure_network_enabled(state, input.network)?;
    rules::authorize(requester, input.network, input.token)?;
    let resolved = resolve::resolve_destination(state, &owner, input.network, input.token, input.amount_raw, &input.destination).await?;
    let settings = state.db.asset_settings(input.token).await?;
    let approval_required = rules::approval_required(input.amount_raw, settings.approval_threshold_raw.0);
    let refusal = rules::check_destination_rule(requester, approval_required, resolved.uses_book_entry(), resolved.kind).err();
    let fee_estimate = estimate_fee(
        state,
        input.network,
        input.token,
        &resolved,
        input.amount_raw,
        input.comment.as_deref(),
    )
    .await?;
    Ok((
        Prepared {
            owner,
            resolved,
            approval_required,
            fee_estimate,
            settings,
        },
        refusal,
    ))
}

/// What a withdrawal would cost — same validation as creation, nothing
/// reserved. Works while outflows are frozen.
pub async fn quote_withdrawal(
    state: &AppState,
    requester: &Requester,
    input: &WithdrawalInput,
) -> Result<WithdrawalQuote, WithdrawalError> {
    let (prepared, refusal) = prepare(state, requester, input).await?;
    // Custody figures are for staff (projects spec invariant 7).
    let (token_balance, fee_balance) = if rules::custody_figures_visible(requester) {
        match balances(state, None, input.network, input.token, None, false).await {
            Ok((token, fee)) => (Some(token), fee),
            Err(err) => {
                warn!("Quote without available balance: {err:?}");
                (None, None)
            }
        }
    } else {
        (None, None)
    };
    // Merchant keys: the project's own figure (never cached, spec §7.2).
    let (project_available_raw, project_check_ok) = match (requester, &prepared.owner) {
        (Requester::ApiKey { .. }, Owner::Project(project_id)) => {
            let balance = ledger::project_balance(state, project_id, input.network, input.token).await?;
            (
                Some(balance.available),
                Some(balance.covers(project_reservation(input, prepared.fee_estimate))),
            )
        }
        _ => (None, None),
    };
    Ok(WithdrawalQuote {
        network: input.network,
        token: input.token,
        amount_raw: input.amount_raw,
        fee_estimate_raw: prepared.fee_estimate,
        fee_token: input.network.fee_token(),
        approval_required: prepared.approval_required,
        destination: prepared.resolved.destination,
        destination_kind: prepared.resolved.kind,
        address_book_entry_id: prepared.resolved.book_entry.map(|e| e.id),
        destination_refusal: refusal,
        available_raw: token_balance.map(|b| b.available()),
        fee_available_raw: fee_balance.map(|b| b.available()),
        project_available_raw,
        project_check_ok,
    })
}

/// Admit a withdrawal under its Idempotency-Key: validated, authorized,
/// checked against every limit and the available balance inside the
/// per-asset locks, then reserved (`QUEUED`, or `PENDING_APPROVAL` above
/// the approval threshold). A replay (same caller and parameters) returns
/// the stored withdrawal.
pub async fn create_withdrawal(
    state: &AppState,
    actor: &Actor,
    requester: &Requester,
    idempotency_key: &str,
    input: &WithdrawalInput,
) -> Result<CreatedWithdrawal, WithdrawalError> {
    // Replays are answered from storage, whatever happened since (freeze,
    // provider down, book entry deleted) — no provider is contacted.
    if let Some(existing) = state.db.find_withdrawal_by_idempotency_key(idempotency_key).await? {
        return replay(&existing, requester, &replay_hash(state, input).await?);
    }
    let (prepared, refusal) = prepare(state, requester, input).await?;
    if let Some(refusal) = refusal {
        return Err(refusal.into());
    }
    if crate::freeze::outflows_frozen(state).await {
        return Err(WithdrawalError::Frozen);
    }
    let params_hash = rules::params_hash(
        input.network,
        input.token,
        &prepared.resolved.destination,
        input.amount_raw,
        input.comment.as_deref(),
    );

    let fee_token = input.network.fee_token();
    let key_limit = match requester {
        Requester::ApiKey { id, policy, .. } => {
            let limit = state
                .db
                .api_key_withdrawal_limits(id)
                .await?
                .into_iter()
                .find(|l| l.token == input.token.as_db());
            if limit.is_none() && *policy == WithdrawalPolicy::Open {
                return Err(RuleError::Refused {
                    code: "key_limit_missing",
                    message: format!("This API key has no {} withdrawal limit", input.token.as_api()),
                }
                .into());
            }
            limit
        }
        Requester::Staff { .. } => None,
    };
    // Single active admin: they may approve their own withdrawal after a
    // delay (spec §7.4).
    let self_approval_not_before = match requester {
        Requester::Staff { .. } if prepared.approval_required && state.db.count_active_admins().await? <= 1 => {
            Some(Utc::now() + chrono::Duration::seconds(WITHDRAWAL_SELF_APPROVAL_DELAY_SECS))
        }
        _ => None,
    };
    // Balances are read before the locks (provider calls), from the 30 s
    // cache; a shortfall re-reads them fresh before refusing.
    let (mut token_balance, mut fee_balance) =
        balances(state, None, input.network, input.token, None, false).await?;

    let mut admission = state
        .db
        .begin_withdrawal_admission(&[input.token, fee_token])
        .await?;
    if let Some(existing) = admission.find_by_idempotency_key(idempotency_key).await? {
        return replay(&existing, requester, &params_hash);
    }
    let used = admission.used_last_24h(input.token, None, None).await?;
    rules::check_limit(
        rules::LimitScope::Asset,
        input.token,
        input.amount_raw,
        prepared.settings.max_per_withdrawal_raw.0,
        prepared.settings.daily_max_raw.0,
        used,
    )?;
    if let (Some(limit), Some(key_id)) = (&key_limit, requester.api_key_id()) {
        let used = admission.used_last_24h(input.token, Some(key_id), None).await?;
        rules::check_limit(
            rules::LimitScope::ApiKey,
            input.token,
            input.amount_raw,
            limit.max_per_withdrawal_raw.0,
            limit.daily_max_raw.0,
            used,
        )?;
    }
    // Reservations as they stand under the locks.
    let token_res = admission.reservations(input.network, input.token, None).await?;
    let fee_res = admission.reservations(input.network, fee_token, None).await?;
    token_balance.reserved = token_res.amount;
    token_balance.reserved_fees = if fee_token == input.token { token_res.fees } else { 0 };
    if let Some(fee_balance) = fee_balance.as_mut() {
        fee_balance.reserved = fee_res.amount;
        fee_balance.reserved_fees = fee_res.fees;
    }
    if rules::check_funds(input.token, input.amount_raw, &token_balance, prepared.fee_estimate, fee_balance.as_ref()).is_err() {
        let (fresh_token, fresh_fee) =
            balances(state, Some(&mut admission), input.network, input.token, None, true).await?;
        token_balance = fresh_token;
        fee_balance = fresh_fee;
        rules::check_funds(input.token, input.amount_raw, &token_balance, prepared.fee_estimate, fee_balance.as_ref())?;
    }
    // Key withdrawals only: the project's available balance must cover the
    // amount plus the fee cap when paid in the same asset (spec §8.3,
    // invariant 3) — read on the locked admission connection, never cached.
    if let (Requester::ApiKey { .. }, Owner::Project(project_id)) = (requester, &prepared.owner) {
        let balance = ledger::project_balance_on(admission.conn(), project_id, input.network, input.token).await?;
        if !balance.covers(project_reservation(input, prepared.fee_estimate)) {
            return Err(project_insufficient().into());
        }
    }

    let now = Utc::now();
    let status = if prepared.approval_required {
        WithdrawalStatus::PendingApproval
    } else {
        WithdrawalStatus::Queued
    };
    let row = admission
        .insert(&NewWithdrawal {
            id: input.id_is_idempotency_key.then_some(idempotency_key),
            idempotency_key,
            api_key_id: requester.api_key_id(),
            staff_id: requester.staff_id(),
            owner: prepared.owner.clone(),
            params_hash: &params_hash,
            network: input.network,
            token: input.token,
            destination_kind: prepared.resolved.kind,
            destination: &prepared.resolved.destination,
            address_book_entry_id: prepared.resolved.book_entry.as_ref().map(|e| e.id.as_str()),
            solana_token_account: match &prepared.resolved.solana {
                Some(SolanaRoute::TokenAccount { account, .. }) => Some(account.as_str()),
                _ => None,
            },
            amount_raw: input.amount_raw,
            fee_estimate_raw: prepared.fee_estimate,
            fee_cap_raw: rules::fee_cap(prepared.fee_estimate),
            fee_token,
            status,
            approval_required: prepared.approval_required,
            self_approval_not_before,
            approval_expires_at: prepared
                .approval_required
                .then(|| now + chrono::Duration::seconds(WITHDRAWAL_APPROVAL_EXPIRY_SECS)),
            reference: input.reference.as_deref(),
            comment: input.comment.as_deref(),
            metadata: &input.metadata,
        })
        .await?;
    admission.commit().await?;
    ledger::invalidate_cache(state).await;

    info!(
        "Withdrawal {} admitted: {} {} on {} to {} ({})",
        row.id,
        raw_u128_to_string(input.amount_raw, input.token),
        input.token.as_api(),
        input.network.as_api(),
        row.destination,
        status.as_api()
    );
    audit::record(
        state,
        actor,
        "withdrawal.requested",
        Some(("withdrawal", &row.id)),
        json!({
            "network": input.network.as_api(),
            "token": input.token.as_api(),
            "amount_raw": input.amount_raw.to_string(),
            "project_id": row.project_id,
            "destination": row.destination,
            "address_book_entry_id": row.address_book_entry_id,
            "status": status.as_api(),
            "fee_estimate_raw": prepared.fee_estimate.to_string(),
        }),
    )
    .await;
    if status == WithdrawalStatus::PendingApproval {
        notify_withdrawal(state, &row, WebhookEventType::WithdrawalPendingApproval).await;
    }
    Ok(CreatedWithdrawal { row, replayed: false })
}

/// The stored withdrawal for a replayed key — only for the same caller and
/// parameters.
fn replay(existing: &WithdrawalRow, requester: &Requester, params_hash: &str) -> Result<CreatedWithdrawal, WithdrawalError> {
    if existing.api_key_id.as_deref() != requester.api_key_id()
        || existing.staff_id.as_deref() != requester.staff_id()
        || existing.params_hash != params_hash
    {
        return Err(WithdrawalError::KeyReused);
    }
    Ok(CreatedWithdrawal { row: existing.clone(), replayed: true })
}

/// The parameters hash of `input`, without any provider call (the
/// destination is normalized offline; a book entry is read even if deleted).
async fn replay_hash(state: &AppState, input: &WithdrawalInput) -> Result<String, WithdrawalError> {
    let destination = match &input.destination {
        DestinationInput::BookEntry(id) => state
            .db
            .find_address_book_entry(id)
            .await?
            .map(|entry| entry.destination)
            .unwrap_or_default(),
        DestinationInput::Raw(raw) => {
            let raw = raw.trim();
            let kind = if input.network == Network::Lightning {
                chain::destination::lightning_destination_kind(raw)
            } else {
                DestinationKind::Address
            };
            chain::destination::normalize_destination(input.network, kind, raw, input.amount_raw, &resolve::rules(state))
                // An invalid destination cannot match a stored one.
                .unwrap_or_else(|_| raw.to_string())
        }
    };
    Ok(rules::params_hash(input.network, input.token, &destination, input.amount_raw, input.comment.as_deref()))
}

/// Approve a pending withdrawal (an ADMIN with a fresh TOTP, checked by the
/// HTTP layer): approver rules, then the limits and funds are re-validated
/// under the locks (its reservation is not counted twice).
pub async fn approve_withdrawal(
    state: &AppState,
    actor: &Actor,
    approver_id: &str,
    id: &str,
) -> Result<WithdrawalRow, WithdrawalError> {
    let row = state.db.find_withdrawal(id).await?.ok_or(WithdrawalError::NotFound)?;
    if row.status()? != WithdrawalStatus::PendingApproval {
        return Err(WithdrawalError::Conflict("This withdrawal is not waiting for approval".into()));
    }
    rules::check_approver(
        row.staff_id.as_deref(),
        approver_id,
        row.self_approval_not_before,
        row.approval_expires_at,
        Utc::now(),
    )?;
    let network = row.network()?;
    let token = row.token()?;
    let amount = row.amount_raw.0;
    let fee = row.fee_estimate_raw.map(|f| f.0).unwrap_or(0);
    let settings = state.db.asset_settings(token).await?;
    // The destination rule holds for whoever asked: a raw destination that
    // matched a book entry at creation keeps that entry as information only,
    // so a deleted entry blocks the withdrawal only when its caller needs
    // the book (spec §7.4).
    let requester = requester_of(state, &row).await?.ok_or_else(|| RuleError::Refused {
        code: "requester_revoked",
        message: "The requesting key or staff member may no longer withdraw; reject this withdrawal".into(),
    })?;
    rules::check_destination_rule(&requester, row.approval_required, book_entry_active(state, &row).await?, row.destination_kind()?)?;

    let mut admission = state
        .db
        .begin_withdrawal_admission(&[token, network.fee_token()])
        .await?;
    let Some(locked) = admission.lock_withdrawal(id).await? else {
        return Err(WithdrawalError::NotFound);
    };
    if locked.status()? != WithdrawalStatus::PendingApproval {
        return Err(WithdrawalError::Conflict("This withdrawal is not waiting for approval".into()));
    }
    let used = admission.used_last_24h(token, None, Some(id)).await?;
    rules::check_limit(rules::LimitScope::Asset, token, amount, settings.max_per_withdrawal_raw.0, settings.daily_max_raw.0, used)?;
    if let Some(key_id) = &row.api_key_id {
        if let Some(limit) = state
            .db
            .api_key_withdrawal_limits(key_id)
            .await?
            .into_iter()
            .find(|l| l.token == token.as_db())
        {
            let used = admission.used_last_24h(token, Some(key_id), Some(id)).await?;
            rules::check_limit(rules::LimitScope::ApiKey, token, amount, limit.max_per_withdrawal_raw.0, limit.daily_max_raw.0, used)?;
        }
    }
    let (token_balance, fee_balance) = balances(state, Some(&mut admission), network, token, Some(id), true).await?;
    rules::check_funds(token, amount, &token_balance, fee, fee_balance.as_ref())?;
    // Key withdrawals: the project check again (spec §8.3), its own
    // reservation excluded (it is part of `reserved` already).
    if let (Some(_), Some(project_id)) = (&locked.api_key_id, &locked.project_id) {
        let balance = ledger::project_balance_on(admission.conn(), project_id, network, token).await?;
        let own = reserved_amount(
            amount,
            token,
            locked.fee_token()?,
            locked.fee_cap_raw.map(|c| c.0),
            locked.fee_estimate_raw.map(|f| f.0),
        );
        let available_without_own = balance.available.saturating_add(i128::try_from(own).unwrap_or(i128::MAX));
        if i128::try_from(own).map(|own| available_without_own < own).unwrap_or(true) {
            return Err(project_insufficient().into());
        }
    }
    let approved = admission
        .transition(
            id,
            &[WithdrawalStatus::PendingApproval],
            transition_target(&row, WithdrawalEvent::Approve)?,
            &WithdrawalUpdate {
                approved_by_id: Some(approver_id.to_string()),
                ..Default::default()
            },
        )
        .await?
        .ok_or_else(|| WithdrawalError::Conflict("This withdrawal is not waiting for approval".into()))?;
    admission.commit().await?;
    audit::record(state, actor, "withdrawal.approved", Some(("withdrawal", id)), json!({})).await;
    Ok(approved)
}

/// The status an API action (approve, reject, cancel, bump) leads to, from
/// the transition table (spec §7.3): a `Conflict` when the withdrawal is not
/// in a status that allows it, so that no caller ever writes a status the
/// machine refuses.
fn transition_target(row: &WithdrawalRow, event: WithdrawalEvent) -> Result<WithdrawalStatus, WithdrawalError> {
    let lightning = row.network().map(|n| n == Network::Lightning).unwrap_or(false);
    next_withdrawal_status(row.status()?, event, lightning)
        .map_err(|_| WithdrawalError::Conflict(format!("This withdrawal cannot be {} now", event_verb(event))))
}

fn event_verb(event: WithdrawalEvent) -> &'static str {
    match event {
        WithdrawalEvent::Approve => "approved",
        WithdrawalEvent::Reject => "rejected",
        WithdrawalEvent::Cancel => "cancelled",
        WithdrawalEvent::Replace => "bumped",
        WithdrawalEvent::Abandon => "abandoned",
        _ => "changed",
    }
}

/// Reject a pending withdrawal: an ADMIN other than its requester.
pub async fn reject_withdrawal(
    state: &AppState,
    actor: &Actor,
    admin_id: &str,
    id: &str,
) -> Result<WithdrawalRow, WithdrawalError> {
    let row = state.db.find_withdrawal(id).await?.ok_or(WithdrawalError::NotFound)?;
    if row.staff_id.as_deref() == Some(admin_id) {
        return Err(RuleError::Forbidden("Cancel your own withdrawal instead of rejecting it".into()).into());
    }
    let rejected = state
        .db
        .transition_withdrawal(
            id,
            &[WithdrawalStatus::PendingApproval],
            transition_target(&row, WithdrawalEvent::Reject)?,
            &WithdrawalUpdate {
                cancelled_by_id: Some(admin_id.to_string()),
                cancel_reason: Some(CancelReason::Rejected),
                ..Default::default()
            },
        )
        .await?
        .ok_or_else(|| WithdrawalError::Conflict("This withdrawal is not waiting for approval".into()))?;
    audit::record(state, actor, "withdrawal.rejected", Some(("withdrawal", id)), json!({})).await;
    notify_withdrawal(state, &rejected, WebhookEventType::WithdrawalCancelled).await;
    Ok(rejected)
}

/// Who cancels.
#[derive(Debug, Clone)]
pub enum Canceller {
    /// A merchant key: any key of the withdrawal's project (spec §8.5) —
    /// or, for a legacy key without a project, the requesting key itself.
    ApiKey {
        id: String,
        project_id: Option<String>,
    },
    /// An ADMIN (TOTP checked by the HTTP layer), any withdrawal.
    Admin(String),
}

/// Cancel a withdrawal before anything is signed (PENDING_APPROVAL or
/// QUEUED), releasing its reservation. `cancelledById` keeps who did it.
pub async fn cancel_withdrawal(
    state: &AppState,
    actor: &Actor,
    canceller: &Canceller,
    id: &str,
) -> Result<WithdrawalRow, WithdrawalError> {
    // Invariant 2: a merchant key reaches its project's withdrawal through
    // a query scoped on the project (a legacy key, its own); another
    // project's is as absent as a missing one.
    let (row, by) = match canceller {
        Canceller::ApiKey { id: key, project_id } => {
            let filter = db::WithdrawalFilter {
                api_key_id: project_id.is_none().then(|| key.clone()),
                owner: match project_id {
                    Some(project) => db::OwnerFilter::Project(project.clone()),
                    None => db::OwnerFilter::Any,
                },
                ..Default::default()
            };
            let row = state.db.find_withdrawal_visible(id, &filter).await?.ok_or(WithdrawalError::NotFound)?;
            (row, key)
        }
        Canceller::Admin(staff) => {
            let row = state.db.find_withdrawal(id).await?.ok_or(WithdrawalError::NotFound)?;
            (row, staff)
        }
    };
    let cancelled = state
        .db
        .transition_withdrawal(
            id,
            &[WithdrawalStatus::PendingApproval, WithdrawalStatus::Queued],
            transition_target(&row, WithdrawalEvent::Cancel)?,
            &WithdrawalUpdate {
                cancelled_by_id: Some(by.clone()),
                cancel_reason: Some(CancelReason::Cancelled),
                ..Default::default()
            },
        )
        .await?
        .ok_or_else(|| WithdrawalError::Conflict("Only a withdrawal not yet signed can be cancelled".into()))?;
    audit::record(state, actor, "withdrawal.cancelled", Some(("withdrawal", id)), json!({})).await;
    notify_withdrawal(state, &cancelled, WebhookEventType::WithdrawalCancelled).await;
    Ok(cancelled)
}

/// Admin bump of a STUCK withdrawal: its fee cap doubles and the engine
/// resumes replacing it (spec §7.3).
pub async fn bump_withdrawal(state: &AppState, actor: &Actor, id: &str) -> Result<WithdrawalRow, WithdrawalError> {
    let row = state.db.find_withdrawal(id).await?.ok_or(WithdrawalError::NotFound)?;
    if row.status()? != WithdrawalStatus::Stuck {
        return Err(WithdrawalError::Conflict("Only a stuck withdrawal can be bumped".into()));
    }
    let cap = row.fee_cap_raw.map(|c| c.0).unwrap_or(1).saturating_mul(2);
    let bumped = state
        .db
        .transition_withdrawal(
            id,
            &[WithdrawalStatus::Stuck],
            transition_target(&row, WithdrawalEvent::Replace)?,
            &WithdrawalUpdate {
                fee_cap_raw: Some(cap),
                ..Default::default()
            },
        )
        .await?
        .ok_or_else(|| WithdrawalError::Conflict("Only a stuck withdrawal can be bumped".into()))?;
    // A bumped cap raises the project's reservation (spec §8.4).
    ledger::invalidate_cache(state).await;
    audit::record(
        state,
        actor,
        "withdrawal.bumped",
        Some(("withdrawal", id)),
        json!({ "fee_cap_raw": cap.to_string() }),
    )
    .await;
    Ok(bumped)
}

/// Cancel every approval request older than 72 h, releasing their
/// reservations.
pub async fn approval_expiry_loop(state: AppState) {
    let mut interval = tokio::time::interval(Duration::from_secs(60));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        match state.db.expire_pending_approvals().await {
            Ok(rows) => {
                for row in rows {
                    info!("Withdrawal {} approval expired", row.id);
                    audit::record(&state, &Actor::system(), "withdrawal.cancelled", Some(("withdrawal", &row.id)), json!({ "reason": "expired" })).await;
                    notify_withdrawal(&state, &row, WebhookEventType::WithdrawalCancelled).await;
                }
            }
            Err(err) => error!("approval expiry error: {err:#}"),
        }
    }
}

/// The worker's last look before signing (invariant 6), from the database:
/// approval present, requesting key still allowed, destination rule and
/// book entry still valid, destination still not the gateway's.
/// `Err(code)`: the withdrawal must fail with that client-safe reason.
pub async fn recheck_before_signing(state: &AppState, row: &WithdrawalRow) -> anyhow::Result<Result<(), &'static str>> {
    if row.approval_required && row.approved_by_id.is_none() {
        return Ok(Err("approval_missing"));
    }
    let network = row.network()?;
    let token = row.token()?;
    let Some(requester) = requester_of(state, row).await? else {
        return Ok(Err("requester_revoked"));
    };
    if rules::authorize(&requester, network, token).is_err() {
        return Ok(Err("requester_revoked"));
    }
    // A raw destination that matched a book entry at creation keeps the
    // entry as information: once the entry is gone, the rule decides on the
    // destination alone (an OPEN key or an admin below the threshold may
    // still send there; a BOOK key or an approval-level withdrawal may not).
    let entry_active = book_entry_active(state, row).await?;
    if rules::check_destination_rule(&requester, row.approval_required, entry_active, row.destination_kind()?).is_err() {
        return Ok(Err("destination_not_allowed"));
    }
    if resolve::is_gateway_destination(state, network, &row.destination).await? {
        return Ok(Err("destination_not_allowed"));
    }
    Ok(Ok(()))
}

/// The withdrawal's requester as it stands in the database now, `None`
/// when it is revoked, expired, inactive or gone.
async fn requester_of(state: &AppState, row: &WithdrawalRow) -> anyhow::Result<Option<Requester>> {
    Ok(match (&row.api_key_id, &row.staff_id) {
        (Some(key_id), _) => {
            let Some(key) = state.db.find_api_key(key_id).await? else {
                return Ok(None);
            };
            let usable = key.status == domain::ApiKeyStatus::Active.as_db()
                && key.expires_at.is_none_or(|deadline| deadline > Utc::now());
            if !usable {
                return Ok(None);
            }
            Some(Requester::ApiKey {
                id: key.id.clone(),
                project_id: key.project_id.clone(),
                policy: key.withdrawal_policy(),
                tokens: key.withdrawal_tokens.iter().filter_map(|t| Token::from_db(t).ok()).collect(),
                networks: key
                    .withdrawal_networks
                    .as_ref()
                    .map(|n| n.iter().filter_map(|n| Network::from_db(n).ok()).collect()),
            })
        }
        (None, Some(staff_id)) => {
            let Some(staff) = state.db.find_staff_by_id(staff_id).await? else {
                return Ok(None);
            };
            if !staff.is_active {
                return Ok(None);
            }
            Some(Requester::Staff { id: staff.id, role: domain::StaffRole::from_db(&staff.role)? })
        }
        (None, None) => None,
    })
}

/// Whether the withdrawal's book entry (if any) is still active and still
/// says the same destination.
async fn book_entry_active(state: &AppState, row: &WithdrawalRow) -> anyhow::Result<bool> {
    Ok(match &row.address_book_entry_id {
        Some(entry_id) => state
            .db
            .find_address_book_entry(entry_id)
            .await?
            .is_some_and(|entry| entry.is_active(Utc::now()) && entry.destination == row.destination),
        None => false,
    })
}

/// Webhook body of a withdrawal event (its current state).
pub fn withdrawal_payload(event: WebhookEventType, row: &WithdrawalRow) -> anyhow::Result<Value> {
    let token = row.token()?;
    let fee_token = row.fee_token()?;
    let amount = |raw: u128, token: Token| -> anyhow::Result<String> {
        Ok(format_amount(domain::raw_to_decimal(&raw.to_string(), token)?, token))
    };
    Ok(json!({
        "event_type": event.payload_event_type(),
        "data": {
            "withdrawal_id": row.id,
            "project_id": row.project_id,
            "status": row.status()?.as_api(),
            "network": row.network()?.as_api(),
            "token": token.as_api(),
            "amount": amount(row.amount_raw.0, token)?,
            "amount_raw": row.amount_raw.0.to_string(),
            "fee": row.fee_raw.map(|f| amount(f.0, fee_token)).transpose()?,
            "fee_raw": row.fee_raw.map(|f| f.0.to_string()),
            "fee_token": fee_token.as_api(),
            "destination": row.destination,
            "address_book_entry_id": row.address_book_entry_id,
            "tx_hash": row.tx_hash,
            "confirmations": row.confirmations,
            "reference": row.reference,
            "requested_by": requested_by(row),
            "failure_reason": row.failure_reason,
        },
    }))
}

pub fn requested_by(row: &WithdrawalRow) -> Value {
    match (&row.api_key_id, &row.staff_id) {
        (Some(id), _) => json!({ "type": "api_key", "id": id }),
        (None, Some(id)) => json!({ "type": "staff", "id": id }),
        (None, None) => Value::Null,
    }
}

/// Queue `event` of a withdrawal for the subscribed endpoints of its owner
/// (its project, or the merchant — spec §10.1), once per (withdrawal,
/// event, endpoint). A queueing failure is logged: the withdrawal itself
/// already moved.
pub async fn notify_withdrawal(state: &AppState, row: &WithdrawalRow, event: WebhookEventType) {
    // Every notified move (broadcast, confirmed, failed, cancelled, pending
    // approval) changes what the project ledger shows.
    ledger::invalidate_cache(state).await;
    let result: anyhow::Result<()> = async {
        let owners = webhooks::SubjectOwners::One(row.owner());
        for endpoint in webhooks::endpoints_for(state, event, &owners).await? {
            if state
                .db
                .find_subject_webhook_delivery(event, &row.id, &endpoint.id)
                .await?
                .is_some()
            {
                continue;
            }
            enqueue_webhook(state, event, &row.id, &endpoint.id).await?;
        }
        Ok(())
    }
    .await;
    if let Err(err) = result {
        error!("Failed to queue {} webhook for withdrawal {}: {err:#}", event.as_api(), row.id);
    }
}
