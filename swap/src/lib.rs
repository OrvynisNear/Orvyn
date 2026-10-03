//! Orvyn swap router (`swap.orvyn.near`).
//!
//! Trades Nearly tokens against native NEAR in a single transaction, on the token's Rhea DCL pool,
//! and takes Orvyn's fee on chain. The router does the wrapping and unwrapping, so traders only
//! ever send and receive NEAR:
//!
//! - **Buy**: `buy(token, pool_id, min_out)` with NEAR attached. The router takes the fee, registers
//!   the buyer on the token if needed, wraps the rest and swaps it with the output sent straight to
//!   the buyer (`swap_out_recipient`). Whatever the swap doesn't use (all of it if the price moved
//!   past `min_out`) is unwrapped and refunded, with the fee on it.
//! - **Sell**: `ft_transfer_call` the token to the router with `msg` = `{"pool_id","min_out"}`. The
//!   router reads the token's sell tax, asks the pool for the exact NEAR out, and swaps with that as
//!   the minimum, so it knows what it receives. It unwraps it and sends the seller the NEAR less the
//!   fee. If the pool moved, the swap is refunded and so are the seller's tokens.
//!
//! Fees go to the Orvyn registry in the same transaction (`pay_client_fee` under this router's
//! client id). The router holds no trader funds between transactions. Only Nearly tokens paired
//! with NEAR are accepted.
//!
//! Upgradeable by the owner (`upgrade`, then `migrate` on the new code), keeping all state.

use near_sdk::json_types::{U128, U64};
use near_sdk::serde_json::{self, json, Value};
use near_sdk::{env, log, near, require, AccountId, Gas, GasWeight, NearToken, PanicOnDefault, Promise, PromiseError, PromiseOrValue};

const BPS: u128 = 10_000;
/// Hard cap on the fee the owner can set: 1%.
const MAX_FEE_BPS: u16 = 100;
/// Kept above the router's storage cost, so withdrawals never leave it short.
const STORAGE_MARGIN: u128 = 50_000_000_000_000_000_000_000;
/// Sells through the router (see ft_on_transfer).
const SELLS_ENABLED: bool = false;
/// Smallest trade: 0.001 NEAR in (buys) or out (sells).
const MIN_TRADE: u128 = 1_000_000_000_000_000_000_000;
/// Storage registration for a trader on a Nearly token (NEP-145; any excess comes back to the router).
const TOKEN_STORAGE: u128 = 1_300_000_000_000_000_000_000;
const ONE_YOCTO: NearToken = NearToken::from_yoctonear(1);
const NO_DEPOSIT: NearToken = NearToken::from_yoctonear(0);

const GAS_VIEW: Gas = Gas::from_tgas(5);
/// DCL's `quote` walks the pool's liquidity; it needs far more than a plain view.
const GAS_QUOTE: Gas = Gas::from_tgas(30);
const GAS_STORAGE: Gas = Gas::from_tgas(5);
const GAS_WRAP: Gas = Gas::from_tgas(5);
const GAS_SWAP: Gas = Gas::from_tgas(120);
const GAS_UNWRAP: Gas = Gas::from_tgas(5);
const GAS_FEE: Gas = Gas::from_tgas(10);
const GAS_CALLBACK: Gas = Gas::from_tgas(40);
const GAS_FOR_MIGRATE: Gas = Gas::from_tgas(20);

fn emit(event: &str, data: Value) {
    log!("EVENT_JSON:{}", json!({ "standard": "orvyn_swap", "version": "1.0.0", "event": event, "data": [data] }));
}

/// The fee on `amount` at `bps`, rounded down.
pub fn fee_of(amount: u128, bps: u16) -> u128 {
    amount * bps as u128 / BPS
}

/// `amount` less a tax of `bps`, as the pool receives it.
pub fn after_tax(amount: u128, bps: u16) -> u128 {
    amount * (BPS - bps.min(BPS as u16) as u128) / BPS
}

/// A DCL pool id is `<token_a>|<token_b>|<fee>`. It must pair `token` with `wnear`.
pub fn pool_pairs(pool_id: &str, token: &str, wnear: &str) -> bool {
    let parts: Vec<&str> = pool_id.split('|').collect();
    parts.len() == 3 && parts[2].parse::<u32>().is_ok() && ((parts[0] == token && parts[1] == wnear) || (parts[0] == wnear && parts[1] == token))
}

/// The sell tax (bps) from a Nearly token's `get_tax` answer. Untaxed tokens don't have the method.
pub fn sell_tax_bps(answer: &Result<Value, PromiseError>) -> u16 {
    answer
        .as_ref()
        .ok()
        .and_then(|v| v.get("tax").and_then(|t| t.get("sell_bps")).and_then(|b| b.as_u64()))
        .map(|b| b.min(BPS as u64) as u16)
        .unwrap_or(0)
}

/// The output amount from a DCL `quote` answer (`{"amount":"…","tag":…}`), or 0.
pub fn quoted_amount(answer: &Result<Value, PromiseError>) -> u128 {
    answer
        .as_ref()
        .ok()
        .and_then(|v| v.get("amount").and_then(|a| a.as_str().map(str::to_owned)))
        .and_then(|a| a.parse::<u128>().ok())
        .unwrap_or(0)
}

#[near(serializers = [json])]
pub struct SellMsg {
    pub pool_id: String,
    pub min_out: U128,
}

#[near(serializers = [json])]
pub struct Config {
    pub owner: AccountId,
    pub wnear: AccountId,
    pub dcl: AccountId,
    pub registry: AccountId,
    pub client_id: String,
    pub fee_bps: u16,
    pub token_suffix: String,
    pub paused: bool,
}

#[near(serializers = [json])]
pub struct Stats {
    pub buys: U64,
    pub sells: U64,
    /// NEAR traded (spent on buys, received on sells), in yocto.
    pub volume_near: U128,
    /// Fees paid into the registry, in yocto.
    pub fees_near: U128,
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct Router {
    owner: AccountId,
    wnear: AccountId,
    dcl: AccountId,
    registry: AccountId,
    client_id: String,
    fee_bps: u16,
    /// Accepted tokens end with this (Nearly launches: `.nearlytrade.near`).
    token_suffix: String,
    paused: bool,
    buys: u64,
    sells: u64,
    volume_near: u128,
    fees_near: u128,
}

#[near]
impl Router {
    #[init]
    pub fn new(owner: AccountId, wnear: AccountId, dcl: AccountId, registry: AccountId, client_id: String, fee_bps: u16, token_suffix: String) -> Self {
        require!(fee_bps <= MAX_FEE_BPS, "fee is capped at 1%");
        Self { owner, wnear, dcl, registry, client_id, fee_bps, token_suffix, paused: false, buys: 0, sells: 0, volume_near: 0, fees_near: 0 }
    }

    // ---------- Buy ----------

    /// Buy `token` with the attached NEAR, on `pool_id`, receiving at least `min_out` tokens
    /// (before the token's buy tax). One transaction; see the module docs.
    #[payable]
    pub fn buy(&mut self, token: AccountId, pool_id: String, min_out: U128) -> Promise {
        self.assert_open(&token, &pool_id);
        let attached = env::attached_deposit().as_yoctonear();
        require!(attached >= MIN_TRADE + TOKEN_STORAGE, "attach at least 0.0023 NEAR");
        let buyer = env::predecessor_account_id();
        // Is the buyer registered on the token? Then wrap and swap.
        Promise::new(token.clone())
            .function_call("storage_balance_of".to_string(), json!({ "account_id": buyer }).to_string().into_bytes(), NO_DEPOSIT, GAS_VIEW)
            .then(Self::ext(env::current_account_id()).with_static_gas(GAS_SWAP.saturating_add(Gas::from_tgas(120))).buy_registered(buyer, token, pool_id, min_out, U128(attached)))
    }

    #[private]
    pub fn buy_registered(
        &mut self,
        buyer: AccountId,
        token: AccountId,
        pool_id: String,
        min_out: U128,
        attached: U128,
        #[callback_result] registered: Result<Value, PromiseError>,
    ) -> Promise {
        let needs_storage = !matches!(registered, Ok(ref v) if !v.is_null());
        let storage = if needs_storage { TOKEN_STORAGE } else { 0 };
        let swap_in = attached.0 - storage;
        let fee = fee_of(swap_in, self.fee_bps);
        let amount = swap_in - fee;
        let msg = json!({ "Swap": { "pool_ids": [pool_id], "output_token": token, "min_output_amount": min_out, "swap_out_recipient": buyer } }).to_string();
        let swap = Promise::new(self.wnear.clone())
            .function_call("near_deposit".to_string(), b"{}".to_vec(), NearToken::from_yoctonear(amount), GAS_WRAP)
            .function_call(
                "ft_transfer_call".to_string(),
                json!({ "receiver_id": self.dcl, "amount": U128(amount), "msg": msg }).to_string().into_bytes(),
                ONE_YOCTO,
                GAS_SWAP,
            );
        let first = if needs_storage {
            Promise::new(token.clone())
                .function_call(
                    "storage_deposit".to_string(),
                    json!({ "account_id": buyer, "registration_only": true }).to_string().into_bytes(),
                    NearToken::from_yoctonear(storage),
                    GAS_STORAGE,
                )
                .then(swap)
        } else {
            swap
        };
        first.then(Self::ext(env::current_account_id()).with_static_gas(GAS_CALLBACK).on_bought(buyer, token, U128(amount), U128(fee)))
    }

    /// After the swap: `used` wNEAR went into the pool. Refund the rest (and its share of the fee),
    /// pay the fee on what was used.
    #[private]
    pub fn on_bought(&mut self, buyer: AccountId, token: AccountId, amount: U128, fee: U128, #[callback_result] used: Result<U128, PromiseError>) -> U128 {
        let used = used.map(|u| u.0.min(amount.0)).unwrap_or(0);
        let unused = amount.0 - used;
        // The whole fee if the swap used everything; otherwise the fee on what it used (never more).
        let fee_charged = if used == amount.0 { fee.0 } else { fee_of(used, self.fee_bps).min(fee.0) };
        let refund = unused + (fee.0 - fee_charged);
        if unused > 0 {
            Promise::new(self.wnear.clone())
                .function_call("near_withdraw".to_string(), json!({ "amount": U128(unused) }).to_string().into_bytes(), ONE_YOCTO, GAS_UNWRAP)
                .then(Promise::new(buyer.clone()).transfer(NearToken::from_yoctonear(refund)))
                .detach();
        } else if refund > 0 {
            Promise::new(buyer.clone()).transfer(NearToken::from_yoctonear(refund)).detach();
        }
        if used > 0 {
            self.buys += 1;
            self.volume_near += used;
            self.pay_fee(fee_charged);
            emit("bought", json!({ "buyer": buyer, "token": token, "near_in": U128(used), "fee": U128(fee_charged) }));
        } else {
            emit("refunded", json!({ "trader": buyer, "token": token, "side": "buy", "near": U128(refund) }));
        }
        U128(used)
    }

    // ---------- Sell ----------

    /// NEP-141 receiver: a sell. The token is the caller; `msg` = `{"pool_id","min_out"}` with
    /// `min_out` the least NEAR (yocto) to receive before the fee. Returns the tokens not used.
    pub fn ft_on_transfer(&mut self, sender_id: AccountId, amount: U128, msg: String) -> PromiseOrValue<U128> {
        // Sells are off: Rhea's DCL pays a sell's proceeds out as native NEAR in receipts that land
        // after this router's callback, so the router can't forward them to the seller. Every
        // token sent here is returned to its sender untouched until sells are redesigned.
        if !SELLS_ENABLED {
            emit("refunded", json!({ "trader": sender_id, "token": env::predecessor_account_id(), "side": "sell", "tokens": amount, "reason": "sells are disabled" }));
            return PromiseOrValue::Value(amount);
        }
        let token = env::predecessor_account_id();
        let sell: SellMsg = serde_json::from_str(&msg).unwrap_or_else(|_| env::panic_str(r#"msg must be {"pool_id","min_out"}"#));
        self.assert_open(&token, &sell.pool_id);
        require!(amount.0 > 0, "nothing to sell");
        PromiseOrValue::Promise(
            Promise::new(token.clone())
                .function_call("get_tax".to_string(), b"{}".to_vec(), NO_DEPOSIT, GAS_VIEW)
                .then(Self::ext(env::current_account_id()).with_static_gas(Gas::from_tgas(210)).sell_taxed(sender_id, token, amount, sell.pool_id, sell.min_out)),
        )
    }

    /// With the sell tax known: quote exactly what the pool pays for the tokens it will receive.
    #[private]
    pub fn sell_taxed(
        &mut self,
        seller: AccountId,
        token: AccountId,
        amount: U128,
        pool_id: String,
        min_out: U128,
        #[callback_result] tax: Result<Value, PromiseError>,
    ) -> PromiseOrValue<U128> {
        let into_pool = after_tax(amount.0, sell_tax_bps(&tax));
        PromiseOrValue::Promise(
            Promise::new(self.dcl.clone())
                .function_call(
                    "quote".to_string(),
                    json!({ "pool_ids": [pool_id], "input_token": token, "output_token": self.wnear, "input_amount": U128(into_pool), "tag": Value::Null }).to_string().into_bytes(),
                    NO_DEPOSIT,
                    GAS_QUOTE,
                )
                .then(Self::ext(env::current_account_id()).with_static_gas(Gas::from_tgas(170)).sell_quoted(seller, token, amount, pool_id, min_out)),
        )
    }

    /// Swap for at least the quoted NEAR, so the router knows what it receives. If the pool moved
    /// in between, the swap is refunded (and so are the seller's tokens).
    #[private]
    pub fn sell_quoted(
        &mut self,
        seller: AccountId,
        token: AccountId,
        amount: U128,
        pool_id: String,
        min_out: U128,
        #[callback_result] quote: Result<Value, PromiseError>,
    ) -> PromiseOrValue<U128> {
        let out = quoted_amount(&quote);
        if out < MIN_TRADE || out < min_out.0 {
            emit("refunded", json!({ "trader": seller, "token": token, "side": "sell", "tokens": amount, "reason": "price below min_out" }));
            return PromiseOrValue::Value(amount);
        }
        let msg = json!({ "Swap": { "pool_ids": [pool_id], "output_token": self.wnear, "min_output_amount": U128(out) } }).to_string();
        PromiseOrValue::Promise(
            Promise::new(token.clone())
                .function_call(
                    "ft_transfer_call".to_string(),
                    json!({ "receiver_id": self.dcl, "amount": amount, "msg": msg }).to_string().into_bytes(),
                    ONE_YOCTO,
                    GAS_SWAP,
                )
                .then(Self::ext(env::current_account_id()).with_static_gas(GAS_CALLBACK).on_sold(seller, token, amount, U128(out))),
        )
    }

    /// After the swap: if the pool took the tokens, the router holds at least `out` wNEAR. Unwrap
    /// it, send the seller the NEAR less the fee, and pay the fee. Returns the tokens not used.
    #[private]
    pub fn on_sold(&mut self, seller: AccountId, token: AccountId, amount: U128, out: U128, #[callback_result] used: Result<U128, PromiseError>) -> U128 {
        let used = used.map(|u| u.0.min(amount.0)).unwrap_or(0);
        if used == 0 {
            emit("refunded", json!({ "trader": seller, "token": token, "side": "sell", "tokens": amount, "reason": "swap refunded" }));
            return amount;
        }
        let fee = fee_of(out.0, self.fee_bps);
        let pay = out.0 - fee;
        Promise::new(self.wnear.clone())
            .function_call("near_withdraw".to_string(), json!({ "amount": out }).to_string().into_bytes(), ONE_YOCTO, GAS_UNWRAP)
            .then(Promise::new(seller.clone()).transfer(NearToken::from_yoctonear(pay)))
            .detach();
        self.sells += 1;
        self.volume_near += out.0;
        self.pay_fee(fee);
        emit("sold", json!({ "seller": seller, "token": token, "tokens_in": U128(used), "near_out": U128(pay), "fee": U128(fee) }));
        U128(amount.0 - used)
    }

    // ---------- Admin ----------

    /// One-time: register the router on wNEAR (attach the storage deposit, e.g. 0.00125 NEAR).
    #[payable]
    pub fn setup(&mut self) -> Promise {
        self.assert_owner();
        Promise::new(self.wnear.clone()).function_call(
            "storage_deposit".to_string(),
            json!({ "account_id": env::current_account_id(), "registration_only": true }).to_string().into_bytes(),
            env::attached_deposit(),
            GAS_STORAGE,
        )
    }

    pub fn set_fee_bps(&mut self, fee_bps: u16) {
        self.assert_owner();
        require!(fee_bps <= MAX_FEE_BPS, "fee is capped at 1%");
        self.fee_bps = fee_bps;
        emit("config_changed", json!({ "fee_bps": fee_bps }));
    }

    pub fn set_paused(&mut self, paused: bool) {
        self.assert_owner();
        self.paused = paused;
        emit("config_changed", json!({ "paused": paused }));
    }

    pub fn set_client_id(&mut self, client_id: String) {
        self.assert_owner();
        self.client_id = client_id;
    }

    /// Unwrap wNEAR the router holds beyond any trade (a sell that paid out more than its quote
    /// leaves the difference here) and pay it into the registry as fees.
    #[payable]
    pub fn sweep(&mut self, amount: U128) -> Promise {
        self.assert_owner();
        near_sdk::assert_one_yocto();
        Promise::new(self.wnear.clone())
            .function_call("near_withdraw".to_string(), json!({ "amount": amount }).to_string().into_bytes(), ONE_YOCTO, GAS_UNWRAP)
            .then(
                Promise::new(self.registry.clone()).function_call(
                    "pay_client_fee".to_string(),
                    json!({ "client_id": self.client_id }).to_string().into_bytes(),
                    NearToken::from_yoctonear(amount.0),
                    GAS_FEE,
                ),
            )
    }

    /// Owner: send NEAR the router holds beyond its own storage (and a small margin) to
    /// `receiver_id`. All of it when `amount` is omitted. The router's storage is never touched.
    pub fn withdraw_near(&mut self, receiver_id: AccountId, amount: Option<U128>) -> Promise {
        self.assert_owner();
        let free = self.free_balance();
        let amount = amount.map(|a| a.0).unwrap_or(free);
        require!(amount > 0 && amount <= free, "more than the router's free balance");
        emit("withdrawn", json!({ "receiver_id": receiver_id, "amount": U128(amount) }));
        Promise::new(receiver_id).transfer(NearToken::from_yoctonear(amount))
    }

    /// Owner: shut the router down for good. It must be paused first (so no trade is mid-way).
    /// Deletes this account, which sends its whole balance, storage included, to `beneficiary_id`.
    pub fn delete_router(&mut self, beneficiary_id: AccountId) -> Promise {
        self.assert_owner();
        require!(self.paused, "pause the router first");
        emit("deleted", json!({ "beneficiary_id": beneficiary_id, "balance": U128(env::account_balance().as_yoctonear()) }));
        Promise::new(env::current_account_id()).delete_account(beneficiary_id)
    }

    pub fn transfer_ownership(&mut self, new_owner: AccountId) {
        self.assert_owner();
        self.owner = new_owner;
    }

    /// Deploy new code (the call's raw input) and run its `migrate`, in one batch.
    pub fn upgrade(&self) -> Promise {
        self.assert_owner();
        let code = env::input().unwrap_or_default();
        require!(code.starts_with(b"\0asm"), "input must be the new contract WASM");
        Promise::new(env::current_account_id())
            .deploy_contract(code)
            .function_call_weight("migrate".to_string(), vec![], NO_DEPOSIT, GAS_FOR_MIGRATE, GasWeight(1))
    }

    #[private]
    #[init(ignore_state)]
    pub fn migrate() -> Self {
        let state: Self = env::state_read().unwrap_or_else(|| env::panic_str("no state to migrate"));
        emit("upgraded", json!({ "version": env!("CARGO_PKG_VERSION") }));
        state
    }

    // ---------- Views ----------

    pub fn version(&self) -> String {
        env!("CARGO_PKG_VERSION").to_string()
    }

    pub fn get_config(&self) -> Config {
        Config {
            owner: self.owner.clone(),
            wnear: self.wnear.clone(),
            dcl: self.dcl.clone(),
            registry: self.registry.clone(),
            client_id: self.client_id.clone(),
            fee_bps: self.fee_bps,
            token_suffix: self.token_suffix.clone(),
            paused: self.paused,
        }
    }

    pub fn get_stats(&self) -> Stats {
        Stats { buys: U64(self.buys), sells: U64(self.sells), volume_near: U128(self.volume_near), fees_near: U128(self.fees_near) }
    }

    /// NEAR the router holds beyond what its storage needs, less a small margin.
    pub fn get_free_balance(&self) -> U128 {
        U128(self.free_balance())
    }

    // ---------- Internal ----------

    fn free_balance(&self) -> u128 {
        let locked = env::storage_byte_cost().as_yoctonear() * env::storage_usage() as u128 + STORAGE_MARGIN;
        env::account_balance().as_yoctonear().saturating_sub(locked)
    }

    fn pay_fee(&mut self, fee: u128) {
        if fee == 0 {
            return;
        }
        self.fees_near += fee;
        Promise::new(self.registry.clone())
            .function_call("pay_client_fee".to_string(), json!({ "client_id": self.client_id }).to_string().into_bytes(), NearToken::from_yoctonear(fee), GAS_FEE)
            .detach();
    }

    fn assert_owner(&self) {
        require!(env::predecessor_account_id() == self.owner, "only the owner");
    }

    fn assert_open(&self, token: &AccountId, pool_id: &str) {
        require!(!self.paused, "trading is paused");
        require!(token.as_str().ends_with(&self.token_suffix) && token != &self.wnear, "only Nearly tokens");
        require!(pool_pairs(pool_id, token.as_str(), self.wnear.as_str()), "pool must pair the token with wNEAR");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use near_sdk::test_utils::VMContextBuilder;
    use near_sdk::testing_env;

    fn acc(s: &str) -> AccountId {
        s.parse().unwrap()
    }

    fn router() -> Router {
        testing_env!(VMContextBuilder::new().predecessor_account_id(acc("owner.near")).build());
        Router::new(acc("owner.near"), acc("wrap.near"), acc("dclv2.ref-labs.near"), acc("registry.orvyn.near"), "orvyn-swap".into(), 30, ".nearlytrade.near".into())
    }

    #[test]
    fn fees_and_tax() {
        assert_eq!(fee_of(1_000_000, 30), 3_000);
        assert_eq!(fee_of(999, 30), 2);
        assert_eq!(after_tax(10_000, 300), 9_700);
        assert_eq!(after_tax(10_000, 0), 10_000);
        assert_eq!(after_tax(10_000, 20_000), 0);
    }

    #[test]
    fn pools() {
        assert!(pool_pairs("ninu-4.nearlytrade.near|wrap.near|10000", "ninu-4.nearlytrade.near", "wrap.near"));
        assert!(pool_pairs("wrap.near|abc.nearlytrade.near|2000", "abc.nearlytrade.near", "wrap.near"));
        assert!(!pool_pairs("ninu-4.nearlytrade.near|usdc.near|10000", "ninu-4.nearlytrade.near", "wrap.near"));
        assert!(!pool_pairs("other.nearlytrade.near|wrap.near|10000", "ninu-4.nearlytrade.near", "wrap.near"));
        assert!(!pool_pairs("ninu-4.nearlytrade.near|wrap.near", "ninu-4.nearlytrade.near", "wrap.near"));
        assert!(!pool_pairs("ninu-4.nearlytrade.near|wrap.near|x", "ninu-4.nearlytrade.near", "wrap.near"));
    }

    #[test]
    fn reads_tax_and_quote() {
        let j = |s: &str| -> Result<Value, PromiseError> { Ok(serde_json::from_str(s).unwrap()) };
        assert_eq!(sell_tax_bps(&j(r#"{"tax":{"buy_bps":200,"sell_bps":300,"pairs":["dclv2.ref-labs.near"]},"pending":"1"}"#)), 300);
        assert_eq!(sell_tax_bps(&j("null")), 0);
        assert_eq!(sell_tax_bps(&Err(PromiseError::Failed)), 0);
        assert_eq!(quoted_amount(&j(r#"{"amount":"12345","tag":null}"#)), 12_345);
        assert_eq!(quoted_amount(&j("null")), 0);
        assert_eq!(quoted_amount(&Err(PromiseError::Failed)), 0);
    }

    #[test]
    #[should_panic(expected = "only Nearly tokens")]
    fn rejects_other_tokens() {
        let r = router();
        r.assert_open(&acc("usdc.near"), "usdc.near|wrap.near|100");
    }

    #[test]
    #[should_panic(expected = "trading is paused")]
    fn pause_stops_trading() {
        let mut r = router();
        r.set_paused(true);
        r.assert_open(&acc("a.nearlytrade.near"), "a.nearlytrade.near|wrap.near|100");
    }

    #[test]
    #[should_panic(expected = "fee is capped")]
    fn fee_cap() {
        let mut r = router();
        r.set_fee_bps(101);
    }

    fn with_balance(predecessor: &str, balance: u128, storage: u64) {
        testing_env!(VMContextBuilder::new()
            .predecessor_account_id(acc(predecessor))
            .account_balance(NearToken::from_yoctonear(balance))
            .storage_usage(storage)
            .build());
    }

    #[test]
    fn free_balance_keeps_storage_and_margin() {
        let r = router();
        // 4.7 NEAR held, 183,953 bytes of storage (1.83953 NEAR), 0.05 NEAR margin.
        with_balance("owner.near", 4_700_000_000_000_000_000_000_000, 183_953);
        assert_eq!(r.get_free_balance().0, 4_700_000_000_000_000_000_000_000 - 1_839_530_000_000_000_000_000_000 - STORAGE_MARGIN);
        with_balance("owner.near", 1_000_000_000_000_000_000_000_000, 183_953);
        assert_eq!(r.get_free_balance().0, 0);
    }

    #[test]
    #[should_panic(expected = "more than the router's free balance")]
    fn withdraw_capped_at_free_balance() {
        let mut r = router();
        with_balance("owner.near", 4_700_000_000_000_000_000_000_000, 183_953);
        let _ = r.withdraw_near(acc("orvyn.near"), Some(U128(4_000_000_000_000_000_000_000_000)));
    }

    #[test]
    #[should_panic(expected = "only the owner")]
    fn withdraw_owner_only() {
        let mut r = router();
        with_balance("mallory.near", 4_700_000_000_000_000_000_000_000, 183_953);
        let _ = r.withdraw_near(acc("mallory.near"), None);
    }

    #[test]
    fn sells_are_refunded() {
        let mut r = router();
        testing_env!(VMContextBuilder::new().predecessor_account_id(acc("a.nearlytrade.near")).build());
        match r.ft_on_transfer(acc("seller.near"), U128(500), r#"{"pool_id":"a.nearlytrade.near|wrap.near|10000","min_out":"1"}"#.into()) {
            PromiseOrValue::Value(v) => assert_eq!(v.0, 500),
            _ => panic!("a sell must be refunded"),
        }
    }

    #[test]
    #[should_panic(expected = "pause the router first")]
    fn delete_needs_pause() {
        let mut r = router();
        let _ = r.delete_router(acc("owner.near"));
    }

    #[test]
    #[should_panic(expected = "only the owner")]
    fn delete_owner_only() {
        let mut r = router();
        r.set_paused(true);
        testing_env!(VMContextBuilder::new().predecessor_account_id(acc("mallory.near")).build());
        let _ = r.delete_router(acc("mallory.near"));
    }

    #[test]
    fn delete_when_paused() {
        let mut r = router();
        r.set_paused(true);
        let _ = r.delete_router(acc("owner.near"));
    }

    #[test]
    #[should_panic(expected = "only the owner")]
    fn owner_only() {
        let mut r = router();
        testing_env!(VMContextBuilder::new().predecessor_account_id(acc("mallory.near")).build());
        r.set_fee_bps(10);
    }

    #[test]
    fn buy_refund_math_no_overflow() {
        // Real NEAR amounts: 5 NEAR in, 0.3% fee. The fee math must not overflow u128.
        let amount: u128 = 4_985_000_000_000_000_000_000_000;
        let fee = fee_of(amount, 30);
        assert_eq!(fee_of(amount, 30).min(fee), fee);
        assert!(fee_of(u128::MAX / 10_000, 30) > 0);
    }

    #[test]
    fn buy_refund_math() {
        // on_bought's arithmetic: a full refund returns the fee too; a fill pays the whole fee.
        let (amount, fee) = (997u128, 3u128);
        let refund = |used: u128| {
            let unused = amount - used;
            let fee_charged = if used == amount { fee } else { fee_of(used, 30).min(fee) };
            (unused + (fee - fee_charged), fee_charged)
        };
        assert_eq!(refund(0), (1000, 0));
        assert_eq!(refund(997), (0, 3));
    }
}
