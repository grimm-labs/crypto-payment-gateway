//! Turning what a caller sent (`destination` or `address_book_entry_id`)
//! into a validated, normalized destination — with the checks that need the
//! database or an RPC: address-book entries, gateway-owned addresses
//! (invariant 7), Solana account kinds, Lightning-address DNS.

use chain::destination::{lightning_destination_kind, normalize_destination, DestinationRules};
use chain::solana_tx::{associated_token_account, SolanaAccount, SolanaRpc};
use chrono::Utc;
use db::AddressBookEntryRow;
use domain::{DestinationKind, Network, Owner, Token};
use wallet::{KeyRef, Signer};

use super::WithdrawalError;
use crate::AppState;

/// What a caller targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DestinationInput {
    Raw(String),
    BookEntry(String),
}

/// How a Solana withdrawal reaches its destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SolanaRoute {
    /// SOL to a wallet.
    Wallet,
    /// USDT to this token account (existing, or created for its owner).
    TokenAccount { account: String, create_for_owner: Option<String> },
}

/// A validated destination.
#[derive(Debug, Clone)]
pub struct ResolvedDestination {
    pub kind: DestinationKind,
    /// Normalized: what is stored, hashed and signed for.
    pub destination: String,
    /// The active book entry it is (targeted, or matching the raw input).
    pub book_entry: Option<AddressBookEntryRow>,
    /// Solana only.
    pub solana: Option<SolanaRoute>,
}

impl ResolvedDestination {
    pub fn uses_book_entry(&self) -> bool {
        self.book_entry.is_some()
    }
}

/// The destination rules of the current chain mode.
pub fn rules(state: &AppState) -> DestinationRules<'_> {
    DestinationRules {
        testnet: state.config.chain.is_testnet(),
        usdt_eth_contract: &state.config.chain.usdt_eth,
        now_unix: u64::try_from(Utc::now().timestamp()).unwrap_or(0),
    }
}

/// Validate and normalize the destination of a withdrawal of `amount_raw`
/// by `owner`. Address-book entries are the owner's own (spec §8.2): another
/// owner's entry is unknown, like a missing one.
pub async fn resolve_destination(
    state: &AppState,
    owner: &Owner,
    network: Network,
    token: Token,
    amount_raw: u128,
    input: &DestinationInput,
) -> Result<ResolvedDestination, WithdrawalError> {
    let (kind, destination, book_entry) = match input {
        DestinationInput::BookEntry(id) => {
            let entry = state
                .db
                .find_address_book_entry(id)
                .await?
                .filter(|entry| entry.deleted_at.is_none() && entry.owner() == *owner)
                .ok_or_else(|| WithdrawalError::BadRequest("Unknown address-book entry".into()))?;
            if entry.network()? != network {
                return Err(WithdrawalError::BadRequest(
                    "This address-book entry belongs to another network".into(),
                ));
            }
            if !entry.is_active(Utc::now()) {
                return Err(WithdrawalError::Rule(super::RuleError::Refused {
                    code: "address_book_entry_not_active",
                    message: format!(
                        "This address-book entry becomes usable at {}",
                        entry.active_at.to_rfc3339()
                    ),
                }));
            }
            // Book entries were validated when created; the amount rules
            // (dust, invoice amount) still apply.
            let kind = entry.destination_kind()?;
            let destination = normalize_destination(network, kind, &entry.destination, amount_raw, &rules(state))
                .map_err(|e| WithdrawalError::BadRequest(e.to_string()))?;
            (kind, destination, Some(entry))
        }
        DestinationInput::Raw(raw) => {
            let raw = raw.trim();
            let kind = if network == Network::Lightning {
                lightning_destination_kind(raw)
            } else {
                DestinationKind::Address
            };
            let destination = normalize_destination(network, kind, raw, amount_raw, &rules(state))
                .map_err(|e| WithdrawalError::BadRequest(e.to_string()))?;
            // A raw destination that is one of the owner's active book
            // entries counts as one.
            let entry = state
                .db
                .find_address_book_entry_by_destination(owner, network, &destination)
                .await?
                .filter(|entry| entry.is_active(Utc::now()));
            (kind, destination, entry)
        }
    };

    if is_gateway_destination(state, network, &destination).await? {
        return Err(WithdrawalError::BadRequest(
            "Destinations never point at the gateway's own addresses".into(),
        ));
    }
    if kind == DestinationKind::LnAddress {
        ensure_public_ln_domain(state, &destination).await?;
    }
    let solana = if network == Network::Solana {
        Some(solana_route(state, &state.chain, token, &destination).await?)
    } else {
        None
    };
    Ok(ResolvedDestination {
        kind,
        destination,
        book_entry,
        solana,
    })
}

/// SOL goes to wallets only; USDT to a USDT token account — the one given,
/// or the wallet's associated account (created by the withdrawal when
/// missing). Another mint's token account is refused.
pub async fn solana_route(
    state: &AppState,
    rpc: &dyn SolanaRpc,
    token: Token,
    destination: &str,
) -> Result<SolanaRoute, WithdrawalError> {
    let mint = &state.config.chain.usdt_solana_mint;
    let account = rpc.account(destination).await.map_err(WithdrawalError::Provider)?;
    match (token, account) {
        (Token::Sol, SolanaAccount::Token { .. }) => Err(WithdrawalError::BadRequest(
            "SOL withdrawals go to a wallet address, not a token account".into(),
        )),
        (Token::Sol, _) => Ok(SolanaRoute::Wallet),
        (Token::Usdt, SolanaAccount::Token { mint: account_mint, owner }) => {
            if &account_mint != mint {
                return Err(WithdrawalError::BadRequest(
                    "This token account holds another token than USDT".into(),
                ));
            }
            if is_gateway_destination(state, Network::Solana, &owner).await? {
                return Err(WithdrawalError::BadRequest(
                    "Destinations never point at the gateway's own addresses".into(),
                ));
            }
            Ok(SolanaRoute::TokenAccount { account: destination.to_string(), create_for_owner: None })
        }
        (Token::Usdt, account) => {
            // Not a token account: a wallet — a System account (or nothing
            // yet) whose key someone holds. A mint, a program-owned account
            // or a program-derived address would lose the funds.
            if !account.is_wallet()
                || !chain::solana_tx::is_on_curve(destination).map_err(|e| WithdrawalError::BadRequest(e.to_string()))?
            {
                return Err(WithdrawalError::BadRequest(
                    "Neither a USDT token account nor a wallet address (off-curve)".into(),
                ));
            }
            let ata = associated_token_account(destination, mint)
                .map_err(|e| WithdrawalError::BadRequest(e.to_string()))?;
            if is_gateway_destination(state, Network::Solana, &ata).await? {
                return Err(WithdrawalError::BadRequest(
                    "Destinations never point at the gateway's own addresses".into(),
                ));
            }
            let missing = matches!(rpc.account(&ata).await.map_err(WithdrawalError::Provider)?, SolanaAccount::Missing);
            Ok(SolanaRoute::TokenAccount {
                account: ata,
                create_for_owner: missing.then(|| destination.to_string()),
            })
        }
        _ => Err(WithdrawalError::BadRequest(format!(
            "{} cannot be withdrawn on solana",
            token.as_api()
        ))),
    }
}

/// Invariant 7: a deposit address, a treasury, or (Solana) a deposit's or
/// the treasury's owner / token account. On Lightning, one of the gateway's
/// own invoices: paying it would fake a payment with treasury funds.
pub async fn is_gateway_destination(state: &AppState, network: Network, destination: &str) -> anyhow::Result<bool> {
    if network == Network::Lightning {
        return state.db.is_deposit_address(Network::Lightning, destination).await;
    }
    if state.wallet.treasury_address(network)?.eq_ignore_ascii_case(destination) {
        return Ok(true);
    }
    if state.db.is_deposit_address(network, destination).await? {
        return Ok(true);
    }
    if network == Network::Solana {
        if state.wallet.solana_usdt_token_account(KeyRef::Treasury(Network::Solana))? == destination {
            return Ok(true);
        }
        // Deposit owners are not stored (the addresses are their token
        // accounts): derive them.
        let max_index = state.db.max_derivation_index(Network::Solana).await?;
        for index in 0..=u32::try_from(max_index.max(0)).unwrap_or(0) {
            let owner = state
                .wallet
                .public_key(KeyRef::Deposit { network: Network::Solana, index })?;
            if bs58::encode(owner).into_string() == destination {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// The SDK fetches `https://<domain>/.well-known/lnurlp/<user>`: a public
/// name that resolves to an internal address would make the gateway probe
/// its own network. Best effort (the SDK resolves again), skipped in
/// development.
pub async fn ensure_public_ln_domain(state: &AppState, ln_address: &str) -> Result<(), WithdrawalError> {
    if state.config.is_development() {
        return Ok(());
    }
    let host = ln_address.rsplit_once('@').map(|(_, d)| d).unwrap_or_default();
    let resolved: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host, 443))
        .await
        .map_err(|_| WithdrawalError::BadRequest("Lightning address domain does not resolve".into()))?
        .collect();
    if resolved.is_empty() || !resolved.iter().all(|addr| domain::is_public_ip(addr.ip())) {
        return Err(WithdrawalError::BadRequest(
            "Lightning address domain must resolve to public addresses only".into(),
        ));
    }
    Ok(())
}
