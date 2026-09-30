//! Orvyn agent registry.
//!
//! Each Orvyn agent is registered here under its agent ID. Its hosting subscription is paid in
//! USDC or USDT (NEP-141) with a single `ft_transfer_call` to this contract whose `msg` is the
//! agent ID. Anyone can pay for an agent: its owner manually, or the agent itself (autopay).
//! Whole months are credited; any remainder is refunded by the token's `ft_resolve_transfer`.
//!
//! The Orvyn backend reads `get_agent` / `is_active` to suspend or resume the agent's service.
//!
//! It also collects Orvyn's NEAR-side fees, paid by the agents themselves with their agent ID:
//! the platform fee on their creator earnings and the fee on their NEAR swaps. Orvyn tokens
//! launch paired with NEAR, so creator fees arrive in NEAR and fees are paid in NEAR only
//! (`pay_fee`), recorded against the agent. The owner withdraws them (`withdraw_near`); no fee is
//! ever sent straight to a treasury account.
//!
//! Upgradeable by the owner (`upgrade`, then `migrate` on the new code), keeping all state.

use near_sdk::json_types::{U128, U64};
use near_sdk::store::{IterableMap, IterableSet, LookupMap};
use near_sdk::{
    env, log, near, require, AccountId, BorshStorageKey, Gas, GasWeight, NearToken, PanicOnDefault, Promise, PromiseOrValue,
};

const MONTH_MS: u64 = 30 * 24 * 60 * 60 * 1000;
const DAY_MS: u64 = 24 * 60 * 60 * 1000;
const MAX_MONTHS: u128 = 36;
const GAS_FOR_FT_TRANSFER: Gas = Gas::from_tgas(15);
/// Minimum gas for `migrate` on the new code; it also gets all gas left over after `upgrade`.
const GAS_FOR_MIGRATE: Gas = Gas::from_tgas(20);

#[derive(BorshStorageKey)]
#[near]
enum StorageKey {
    Prices,
    Operators,
    Agents,
    Revenue,
    Payments,
    Clients,
}

#[derive(Clone)]
#[near(serializers = [borsh, json])]
pub struct Agent {
    /// NEAR account of the human who created the agent.
    pub owner: AccountId,
    /// The agent's own NEAR account (autopay pays from here).
    pub agent_account: AccountId,
    pub paid_until_ms: U64,
    pub registered_at_ms: U64,
    /// Total paid, in token base units, summed across tokens (USDC and USDT are both 6 decimals).
    pub total_paid: U128,
    pub payments: u32,
    /// Orvyn fees this agent has paid in NEAR (platform fee and NEAR swap fees).
    pub fees_paid_near: U128,
}

/// An app using the Orvyn Swap API outside Orvyn (an API client), and the fees it has paid.
#[derive(Clone)]
#[near(serializers = [borsh, json])]
pub struct Client {
    pub registered_at_ms: U64,
    pub fees_paid_near: U128,
    pub payments: u32,
}

/// Fee kinds agents pay to the registry.
const FEE_KINDS: [&str; 2] = ["platform", "swap"];

#[near(serializers = [json])]
pub struct FeeRevenue {
    /// NEAR fees collected and not yet withdrawn.
    pub near_available: U128,
    /// NEAR fees collected since deployment.
    pub near_total: U128,
}

/// One subscription payment, kept on chain so invoices and receipts can be rebuilt from the
/// registry alone (no indexer needed).
#[derive(Clone)]
#[near(serializers = [borsh, json])]
pub struct Payment {
    pub payer: AccountId,
    pub token: AccountId,
    /// Amount credited, in token base units (any remainder was refunded).
    pub amount: U128,
    pub months: u32,
    /// The period this payment bought: [period_start_ms, paid_until_ms).
    pub period_start_ms: U64,
    pub paid_until_ms: U64,
    pub paid_at_ms: U64,
    pub block_height: U64,
}

#[near(serializers = [json])]
pub struct PaymentView {
    pub index: u32,
    #[serde(flatten)]
    pub payment: Payment,
}

#[near(serializers = [json])]
pub struct AgentView {
    pub agent_id: String,
    pub owner: AccountId,
    pub agent_account: AccountId,
    pub paid_until_ms: U64,
    pub registered_at_ms: U64,
    pub total_paid: U128,
    pub payments: u32,
    pub fees_paid_near: U128,
    pub active: bool,
}

#[near(serializers = [json])]
pub struct Config {
    pub owner: AccountId,
    pub trial_days: u32,
    pub prices: Vec<(AccountId, U128)>,
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct Registry {
    owner: AccountId,
    /// Accepted payment tokens → monthly price in token base units.
    prices: IterableMap<AccountId, U128>,
    /// Accounts allowed to register agents (the Orvyn backend).
    operators: IterableSet<AccountId>,
    agents: LookupMap<String, Agent>,
    /// Collected and not yet withdrawn, per token.
    revenue: LookupMap<AccountId, U128>,
    trial_days: u32,
    /// (agent_id, payment index) → payment. Index i is the agent's (i+1)-th payment.
    payments: LookupMap<(String, u32), Payment>,
    /// NEAR fees collected and not yet withdrawn, and collected in total.
    near_fees: u128,
    near_fees_total: u128,
    /// client_id → API client (apps paying the NEAR swap fee on the Orvyn Swap API).
    clients: LookupMap<String, Client>,
}

fn emit(event: &str, data: near_sdk::serde_json::Value) {
    log!(
        "EVENT_JSON:{}",
        near_sdk::serde_json::json!({ "standard": "orvyn_registry", "version": "1.0.0", "event": event, "data": [data] })
    );
}

fn valid_agent_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[near]
impl Registry {
    #[init]
    pub fn new(owner: AccountId, trial_days: u32) -> Self {
        Self {
            owner,
            prices: IterableMap::new(StorageKey::Prices),
            operators: IterableSet::new(StorageKey::Operators),
            agents: LookupMap::new(StorageKey::Agents),
            revenue: LookupMap::new(StorageKey::Revenue),
            trial_days,
            payments: LookupMap::new(StorageKey::Payments),
            near_fees: 0,
            near_fees_total: 0,
            clients: LookupMap::new(StorageKey::Clients),
        }
    }

    // ---------- Admin ----------

    fn assert_owner(&self) {
        require!(env::predecessor_account_id() == self.owner, "only the registry owner");
    }

    /// Accept `token` for payments at `monthly_price` base units per month.
    pub fn set_price(&mut self, token: AccountId, monthly_price: U128) {
        self.assert_owner();
        require!(monthly_price.0 > 0, "price must be positive");
        self.prices.insert(token, monthly_price);
    }

    pub fn remove_token(&mut self, token: AccountId) {
        self.assert_owner();
        self.prices.remove(&token);
    }

    pub fn set_trial_days(&mut self, trial_days: u32) {
        self.assert_owner();
        self.trial_days = trial_days;
    }

    pub fn add_operator(&mut self, account: AccountId) {
        self.assert_owner();
        self.operators.insert(account);
    }

    pub fn remove_operator(&mut self, account: AccountId) {
        self.assert_owner();
        self.operators.remove(&account);
    }

    pub fn transfer_ownership(&mut self, new_owner: AccountId) {
        self.assert_owner();
        self.owner = new_owner;
    }

    // ---------- Upgrades ----------

    /// Owner only. Upgrades the contract in place: the new WASM is the call's raw input (not
    /// JSON), deployed to this account together with a call to its `migrate` in one batch, so if
    /// `migrate` fails the deploy is reverted too and the old code keeps running. Attach 300 Tgas.
    pub fn upgrade(&self) -> Promise {
        self.assert_owner();
        let code = env::input().unwrap_or_default();
        require!(code.starts_with(b"\0asm"), "input must be the new contract WASM");
        Promise::new(env::current_account_id())
            .deploy_contract(code)
            .function_call_weight("migrate".to_string(), vec![], NearToken::from_yoctonear(0), GAS_FOR_MIGRATE, GasWeight(1))
    }

    /// Runs on the new code right after `upgrade` deploys it (the contract itself only). It
    /// reads the existing state as the new code's state. When a release changes the state's
    /// layout, it reads the previous layout here and converts it, and the release notes say so.
    #[private]
    #[init(ignore_state)]
    pub fn migrate() -> Self {
        let state: Self = env::state_read().unwrap_or_else(|| env::panic_str("no state to migrate"));
        emit("upgraded", near_sdk::serde_json::json!({ "version": env!("CARGO_PKG_VERSION") }));
        state
    }

    /// The deployed code's version (Cargo.toml), to confirm an upgrade.
    pub fn version(&self) -> String {
        env!("CARGO_PKG_VERSION").to_string()
    }

    /// Credit free days (promotions, support refunds).
    pub fn grant_days(&mut self, agent_id: String, days: u32) {
        self.assert_owner();
        let mut agent = self.agents.get(&agent_id).cloned().unwrap_or_else(|| env::panic_str("unknown agent"));
        let base = agent.paid_until_ms.0.max(env::block_timestamp_ms());
        agent.paid_until_ms = U64(base + days as u64 * DAY_MS);
        self.agents.insert(agent_id.clone(), agent.clone());
        emit("days_granted", near_sdk::serde_json::json!({ "agent_id": agent_id, "days": days, "paid_until_ms": agent.paid_until_ms }));
    }

    /// Send collected revenue in `token` to `receiver` (must be registered on the token).
    #[payable]
    pub fn withdraw(&mut self, token: AccountId, amount: U128, receiver: AccountId) -> Promise {
        self.assert_owner();
        near_sdk::assert_one_yocto();
        let available = self.revenue.get(&token).map(|v| v.0).unwrap_or(0);
        require!(amount.0 > 0 && amount.0 <= available, "amount exceeds collected revenue");
        self.revenue.insert(token.clone(), U128(available - amount.0));
        Promise::new(token).function_call(
            "ft_transfer".to_string(),
            near_sdk::serde_json::to_vec(&near_sdk::serde_json::json!({ "receiver_id": receiver, "amount": amount })).unwrap(),
            NearToken::from_yoctonear(1),
            GAS_FOR_FT_TRANSFER,
        )
    }

    /// Send collected NEAR fees to `receiver`. Only fee revenue can leave, never the contract's
    /// storage balance.
    #[payable]
    pub fn withdraw_near(&mut self, amount: U128, receiver: AccountId) -> Promise {
        self.assert_owner();
        near_sdk::assert_one_yocto();
        require!(amount.0 > 0 && amount.0 <= self.near_fees, "amount exceeds collected NEAR fees");
        self.near_fees -= amount.0;
        emit("fees_withdrawn", near_sdk::serde_json::json!({ "token": "near", "amount": amount, "receiver": receiver }));
        Promise::new(receiver).transfer(NearToken::from_yoctonear(amount.0))
    }

    // ---------- Fees (paid by agents) ----------

    /// An agent pays an Orvyn fee in NEAR (the attached deposit). `kind`: "platform" (the cut of
    /// its creator earnings) or "swap" (its NEAR swaps and buybacks).
    #[payable]
    pub fn pay_fee(&mut self, agent_id: String, kind: String) {
        let amount = env::attached_deposit().as_yoctonear();
        require!(amount > 0, "attach the fee as a deposit");
        require!(FEE_KINDS.contains(&kind.as_str()), "fee kind must be platform or swap");
        let mut agent = self.agents.get(&agent_id).cloned().unwrap_or_else(|| env::panic_str("unknown agent_id"));
        agent.fees_paid_near = U128(agent.fees_paid_near.0 + amount);
        self.agents.insert(agent_id.clone(), agent);
        self.near_fees += amount;
        self.near_fees_total += amount;
        emit(
            "fee_paid",
            near_sdk::serde_json::json!({ "agent_id": agent_id, "kind": kind, "token": "near", "amount": U128(amount), "payer": env::predecessor_account_id() }),
        );
    }

    /// An API client (an app using the Orvyn Swap API) pays the NEAR swap fee, attached.
    #[payable]
    pub fn pay_client_fee(&mut self, client_id: String) {
        let amount = env::attached_deposit().as_yoctonear();
        require!(amount > 0, "attach the fee as a deposit");
        let mut client = self.clients.get(&client_id).cloned().unwrap_or_else(|| env::panic_str("unknown client_id"));
        client.fees_paid_near = U128(client.fees_paid_near.0 + amount);
        client.payments += 1;
        self.clients.insert(client_id.clone(), client);
        self.near_fees += amount;
        self.near_fees_total += amount;
        emit(
            "fee_paid",
            near_sdk::serde_json::json!({ "client_id": client_id, "kind": "swap", "token": "near", "amount": U128(amount), "payer": env::predecessor_account_id() }),
        );
    }

    // ---------- Registration ----------

    /// Register an API client. Operators only (the Orvyn backend, when it issues an API key).
    pub fn register_client(&mut self, client_id: String) -> Client {
        require!(
            self.operators.contains(&env::predecessor_account_id()) || env::predecessor_account_id() == self.owner,
            "only an operator can register clients"
        );
        require!(valid_agent_id(&client_id), "client_id: 1-64 of [A-Za-z0-9_-]");
        require!(self.clients.get(&client_id).is_none(), "client already registered");
        let client = Client { registered_at_ms: U64(env::block_timestamp_ms()), fees_paid_near: U128(0), payments: 0 };
        self.clients.insert(client_id.clone(), client.clone());
        emit("client_registered", near_sdk::serde_json::json!({ "client_id": client_id }));
        client
    }

    pub fn get_client(&self, client_id: String) -> Option<Client> {
        self.clients.get(&client_id).cloned()
    }

    /// Register an agent. Operators only (the Orvyn backend, when an agent is created).
    pub fn register_agent(&mut self, agent_id: String, owner: AccountId, agent_account: AccountId) -> AgentView {
        require!(
            self.operators.contains(&env::predecessor_account_id()) || env::predecessor_account_id() == self.owner,
            "only an operator can register agents"
        );
        require!(valid_agent_id(&agent_id), "agent_id: 1-64 of [A-Za-z0-9_-]");
        require!(self.agents.get(&agent_id).is_none(), "agent already registered");
        let now = env::block_timestamp_ms();
        let agent = Agent {
            owner,
            agent_account,
            paid_until_ms: U64(now + self.trial_days as u64 * DAY_MS),
            registered_at_ms: U64(now),
            total_paid: U128(0),
            payments: 0,
            fees_paid_near: U128(0),
        };
        self.agents.insert(agent_id.clone(), agent.clone());
        emit("agent_registered", near_sdk::serde_json::json!({ "agent_id": agent_id, "owner": agent.owner, "paid_until_ms": agent.paid_until_ms }));
        self.view(agent_id, agent)
    }

    // ---------- Payments (NEP-141 receiver) ----------

    /// Pay with `ft_transfer_call(registry, amount, msg = "<agent_id>")`. Returns the unused
    /// amount, which the token refunds to the sender.
    pub fn ft_on_transfer(&mut self, sender_id: AccountId, amount: U128, msg: String) -> PromiseOrValue<U128> {
        let token = env::predecessor_account_id();
        let price = match self.prices.get(&token) {
            Some(p) => p.0,
            None => env::panic_str("token not accepted for payments"),
        };
        let agent_id = msg.trim().to_string();
        let mut agent = match self.agents.get(&agent_id) {
            Some(a) => a.clone(),
            None => env::panic_str("unknown agent_id in msg"),
        };
        let months = (amount.0 / price).min(MAX_MONTHS);
        require!(months >= 1, "amount is less than one month");
        let used = months * price;

        let now = env::block_timestamp_ms();
        // Paying after expiry starts a fresh period from now; paying early extends it.
        let base = agent.paid_until_ms.0.max(now);
        agent.paid_until_ms = U64(base + months as u64 * MONTH_MS);
        agent.total_paid = U128(agent.total_paid.0 + used);
        let index = agent.payments;
        agent.payments += 1;
        self.agents.insert(agent_id.clone(), agent.clone());
        self.payments.insert(
            (agent_id.clone(), index),
            Payment {
                payer: sender_id.clone(),
                token: token.clone(),
                amount: U128(used),
                months: months as u32,
                period_start_ms: U64(base),
                paid_until_ms: agent.paid_until_ms,
                paid_at_ms: U64(now),
                block_height: U64(env::block_height()),
            },
        );

        let collected = self.revenue.get(&token).map(|v| v.0).unwrap_or(0);
        self.revenue.insert(token.clone(), U128(collected + used));

        emit(
            "subscription_paid",
            near_sdk::serde_json::json!({
                "agent_id": agent_id,
                "payment_index": index,
                "payer": sender_id,
                "token": token,
                "amount": U128(used),
                "months": months as u32,
                "period_start_ms": U64(base),
                "paid_until_ms": agent.paid_until_ms,
            }),
        );
        PromiseOrValue::Value(U128(amount.0 - used))
    }

    // ---------- Views ----------

    fn view(&self, agent_id: String, agent: Agent) -> AgentView {
        AgentView {
            active: agent.paid_until_ms.0 > env::block_timestamp_ms(),
            agent_id,
            owner: agent.owner,
            agent_account: agent.agent_account,
            paid_until_ms: agent.paid_until_ms,
            registered_at_ms: agent.registered_at_ms,
            total_paid: agent.total_paid,
            payments: agent.payments,
            fees_paid_near: agent.fees_paid_near,
        }
    }

    pub fn get_agent(&self, agent_id: String) -> Option<AgentView> {
        self.agents.get(&agent_id).cloned().map(|a| self.view(agent_id, a))
    }

    /// An agent's payments, oldest first, from `from_index` (at most 100 per call).
    pub fn get_payments(&self, agent_id: String, from_index: Option<u32>, limit: Option<u32>) -> Vec<PaymentView> {
        let count = self.agents.get(&agent_id).map(|a| a.payments).unwrap_or(0);
        let from = from_index.unwrap_or(0);
        let end = count.min(from.saturating_add(limit.unwrap_or(50).min(100)));
        (from..end)
            .filter_map(|index| self.payments.get(&(agent_id.clone(), index)).cloned().map(|payment| PaymentView { index, payment }))
            .collect()
    }

    pub fn is_active(&self, agent_id: String) -> bool {
        self.agents.get(&agent_id).map(|a| a.paid_until_ms.0 > env::block_timestamp_ms()).unwrap_or(false)
    }

    pub fn get_config(&self) -> Config {
        Config {
            owner: self.owner.clone(),
            trial_days: self.trial_days,
            prices: self.prices.iter().map(|(k, v)| (k.clone(), *v)).collect(),
        }
    }

    pub fn get_revenue(&self, token: AccountId) -> U128 {
        self.revenue.get(&token).copied().unwrap_or(U128(0))
    }

    pub fn get_fee_revenue(&self) -> FeeRevenue {
        FeeRevenue { near_available: U128(self.near_fees), near_total: U128(self.near_fees_total) }
    }

    pub fn is_operator(&self, account: AccountId) -> bool {
        self.operators.contains(&account)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use near_sdk::test_utils::VMContextBuilder;
    use near_sdk::testing_env;

    const USDC_PRICE: u128 = 20_000_000; // 20 USDC

    fn acc(s: &str) -> AccountId {
        s.parse().unwrap()
    }

    fn ctx(predecessor: &str, ts_ms: u64) {
        let mut b = VMContextBuilder::new();
        b.predecessor_account_id(acc(predecessor)).block_timestamp(ts_ms * 1_000_000);
        testing_env!(b.build());
    }

    fn setup() -> Registry {
        ctx("owner.near", 1_000);
        let mut r = Registry::new(acc("owner.near"), 3);
        r.set_price(acc("usdc.near"), U128(USDC_PRICE));
        r.set_price(acc("usdt.near"), U128(USDC_PRICE));
        r.add_operator(acc("backend.near"));
        ctx("backend.near", 1_000);
        r.register_agent("agent-1".into(), acc("alice.near"), acc("agent1.near"));
        r
    }

    fn paid_until(r: &Registry) -> u64 {
        r.get_agent("agent-1".into()).unwrap().paid_until_ms.0
    }

    fn ctx_deposit(predecessor: &str, yocto: u128) {
        let mut b = VMContextBuilder::new();
        b.predecessor_account_id(acc(predecessor)).attached_deposit(NearToken::from_yoctonear(yocto)).block_timestamp(2_000_000_000);
        testing_env!(b.build());
    }

    #[test]
    fn agents_pay_near_fees_to_the_registry() {
        let mut r = setup();
        ctx_deposit("agent1.near", 1_400);
        r.pay_fee("agent-1".into(), "platform".into());
        ctx_deposit("agent1.near", 30);
        r.pay_fee("agent-1".into(), "swap".into());
        assert_eq!(r.get_agent("agent-1".into()).unwrap().fees_paid_near.0, 1_430);
        let f = r.get_fee_revenue();
        assert_eq!((f.near_available.0, f.near_total.0), (1_430, 1_430));
    }

    #[test]
    #[should_panic(expected = "attach the fee")]
    fn near_fee_needs_a_deposit() {
        let mut r = setup();
        ctx_deposit("agent1.near", 0);
        r.pay_fee("agent-1".into(), "platform".into());
    }

    #[test]
    #[should_panic(expected = "fee kind")]
    fn near_fee_kind_is_checked() {
        let mut r = setup();
        ctx_deposit("agent1.near", 10);
        r.pay_fee("agent-1".into(), "tip".into());
    }

    #[test]
    #[should_panic(expected = "unknown agent_id")]
    fn near_fee_needs_a_registered_agent() {
        let mut r = setup();
        ctx_deposit("agent1.near", 10);
        r.pay_fee("nobody".into(), "platform".into());
    }

    #[test]
    #[should_panic(expected = "token not accepted for payments")]
    fn fees_in_tokens_are_refused() {
        let mut r = setup();
        // Fees are NEAR only; a token transfer with a fee message is refunded.
        ctx("atlas.nearlytrade.near", 2_000);
        let _ = r.ft_on_transfer(acc("agent1.near"), U128(5), r#"{"fee":"platform","agent_id":"agent-1"}"#.into());
    }

    #[test]
    fn near_fees_do_not_touch_the_subscription() {
        let mut r = setup();
        ctx_deposit("agent1.near", 1_400);
        r.pay_fee("agent-1".into(), "platform".into());
        let agent = r.get_agent("agent-1".into()).unwrap();
        assert_eq!((agent.fees_paid_near.0, agent.payments, agent.total_paid.0), (1_400, 0, 0));
    }

    #[test]
    fn owner_withdraws_only_collected_near_fees() {
        let mut r = setup();
        ctx_deposit("agent1.near", 1_000);
        r.pay_fee("agent-1".into(), "platform".into());
        ctx_deposit("owner.near", 1);
        let _ = r.withdraw_near(U128(600), acc("orvyn.near"));
        let f = r.get_fee_revenue();
        assert_eq!((f.near_available.0, f.near_total.0), (400, 1_000));
    }

    #[test]
    #[should_panic(expected = "amount exceeds collected NEAR fees")]
    fn cannot_withdraw_more_near_than_collected() {
        let mut r = setup();
        ctx_deposit("agent1.near", 1_000);
        r.pay_fee("agent-1".into(), "platform".into());
        ctx_deposit("owner.near", 1);
        let _ = r.withdraw_near(U128(1_001), acc("orvyn.near"));
    }

    #[test]
    #[should_panic(expected = "only the registry owner")]
    fn only_the_owner_withdraws_near() {
        let mut r = setup();
        ctx_deposit("agent1.near", 1_000);
        r.pay_fee("agent-1".into(), "platform".into());
        ctx_deposit("mallory.near", 1);
        let _ = r.withdraw_near(U128(1), acc("mallory.near"));
    }

    #[test]
    fn payments_are_logged_for_invoices() {
        let mut r = setup();
        ctx("usdc.near", 2_000);
        let _ = r.ft_on_transfer(acc("alice.near"), U128(USDC_PRICE * 2 + 5), "agent-1".into());
        ctx("usdt.near", 5_000);
        let _ = r.ft_on_transfer(acc("agent1.near"), U128(USDC_PRICE), "agent-1".into());
        let all = r.get_payments("agent-1".into(), None, None);
        assert_eq!(all.len(), 2);
        let (first, second) = (&all[0], &all[1]);
        assert_eq!((first.index, first.payment.months, first.payment.amount.0), (0, 2, USDC_PRICE * 2));
        assert_eq!(first.payment.payer, acc("alice.near"));
        assert_eq!(first.payment.token, acc("usdc.near"));
        // The first payment extends the trial; the second starts where the first ended.
        assert_eq!(first.payment.period_start_ms.0, 1_000 + 3 * DAY_MS);
        assert_eq!(first.payment.paid_until_ms.0, 1_000 + 3 * DAY_MS + 2 * MONTH_MS);
        assert_eq!(second.payment.period_start_ms.0, first.payment.paid_until_ms.0);
        assert_eq!((second.index, second.payment.token.as_str(), second.payment.paid_at_ms.0), (1, "usdt.near", 5_000));
        // Paging
        let page = r.get_payments("agent-1".into(), Some(1), Some(10));
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].index, 1);
        assert!(r.get_payments("nobody".into(), None, None).is_empty());
    }

    #[test]
    fn registration_grants_trial() {
        let r = setup();
        assert_eq!(paid_until(&r), 1_000 + 3 * DAY_MS);
        assert!(r.is_active("agent-1".into()));
    }

    #[test]
    #[should_panic(expected = "only an operator")]
    fn strangers_cannot_register() {
        let mut r = setup();
        ctx("mallory.near", 2_000);
        r.register_agent("agent-2".into(), acc("mallory.near"), acc("x.near"));
    }

    #[test]
    fn payment_extends_and_refunds_remainder() {
        let mut r = setup();
        ctx("usdc.near", 2_000);
        let refund = match r.ft_on_transfer(acc("alice.near"), U128(USDC_PRICE * 2 + 5), "agent-1".into()) {
            PromiseOrValue::Value(v) => v.0,
            _ => panic!(),
        };
        assert_eq!(refund, 5);
        assert_eq!(paid_until(&r), 1_000 + 3 * DAY_MS + 2 * MONTH_MS);
        assert_eq!(r.get_revenue(acc("usdc.near")).0, USDC_PRICE * 2);
        assert_eq!(r.get_agent("agent-1".into()).unwrap().payments, 1);
    }

    #[test]
    fn payment_after_expiry_starts_from_now() {
        let mut r = setup();
        let later = 1_000 + 40 * DAY_MS;
        ctx("usdt.near", later);
        assert!(!r.is_active("agent-1".into()));
        let _ = r.ft_on_transfer(acc("agent1.near"), U128(USDC_PRICE), "agent-1".into());
        assert_eq!(paid_until(&r), later + MONTH_MS);
        assert!(r.is_active("agent-1".into()));
    }

    #[test]
    #[should_panic(expected = "token not accepted")]
    fn rejects_other_tokens() {
        let mut r = setup();
        ctx("scam.near", 2_000);
        let _ = r.ft_on_transfer(acc("alice.near"), U128(USDC_PRICE), "agent-1".into());
    }

    #[test]
    #[should_panic(expected = "unknown agent_id")]
    fn rejects_unknown_agent() {
        let mut r = setup();
        ctx("usdc.near", 2_000);
        let _ = r.ft_on_transfer(acc("alice.near"), U128(USDC_PRICE), "nope".into());
    }

    #[test]
    #[should_panic(expected = "less than one month")]
    fn rejects_underpayment() {
        let mut r = setup();
        ctx("usdc.near", 2_000);
        let _ = r.ft_on_transfer(acc("alice.near"), U128(USDC_PRICE - 1), "agent-1".into());
    }

    #[test]
    fn caps_months_and_refunds_the_rest() {
        let mut r = setup();
        ctx("usdc.near", 2_000);
        let refund = match r.ft_on_transfer(acc("alice.near"), U128(USDC_PRICE * 40), "agent-1".into()) {
            PromiseOrValue::Value(v) => v.0,
            _ => panic!(),
        };
        assert_eq!(refund, USDC_PRICE * 4);
    }

    #[test]
    #[should_panic(expected = "exceeds collected revenue")]
    fn cannot_withdraw_more_than_collected() {
        let mut r = setup();
        let mut b = VMContextBuilder::new();
        b.predecessor_account_id(acc("owner.near")).attached_deposit(NearToken::from_yoctonear(1));
        testing_env!(b.build());
        let _ = r.withdraw(acc("usdc.near"), U128(1), acc("treasury.near"));
    }

    fn ctx_upgrade(predecessor: &str, code: &[u8]) {
        let mut b = VMContextBuilder::new();
        b.current_account_id(acc("registry.orvyn.near")).predecessor_account_id(acc(predecessor)).prepaid_gas(Gas::from_tgas(300));
        let mut c = b.build();
        c.input = code.into();
        testing_env!(c);
    }

    const WASM: &[u8] = b"\0asm\x01\0\0\0";

    #[test]
    fn owner_can_upgrade() {
        let c = setup();
        ctx_upgrade("owner.near", WASM);
        let _ = c.upgrade();
    }

    #[test]
    #[should_panic(expected = "only the registry owner")]
    fn only_the_owner_upgrades() {
        let c = setup();
        ctx_upgrade("stranger.near", WASM);
        let _ = c.upgrade();
    }

    #[test]
    #[should_panic(expected = "input must be the new contract WASM")]
    fn upgrade_needs_wasm() {
        let c = setup();
        ctx_upgrade("owner.near", b"{}");
        let _ = c.upgrade();
    }

    #[test]
    fn migrate_keeps_the_state() {
        let mut c = setup();
        // On chain, collections are flushed at the end of each call; do the same here.
        c.prices.flush();
        c.operators.flush();
        c.agents.flush();
        env::state_write(&c);
        let m = Registry::migrate();
        let cfg = m.get_config();
        assert_eq!(cfg.owner, acc("owner.near"));
        assert_eq!(cfg.trial_days, 3);
        assert_eq!(cfg.prices.len(), 2);
        assert_eq!(m.get_agent("agent-1".into()).unwrap().owner, acc("alice.near"));
        assert_eq!(m.version(), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn api_clients_pay_near_swap_fees() {
        let mut r = setup();
        ctx("backend.near", 1_000);
        r.register_client("acme".into());
        ctx_deposit("acme-wallet.near", 300);
        r.pay_client_fee("acme".into());
        ctx_deposit("another.near", 200);
        r.pay_client_fee("acme".into());
        let c = r.get_client("acme".into()).unwrap();
        assert_eq!((c.fees_paid_near.0, c.payments), (500, 2));
        assert_eq!(r.get_fee_revenue().near_available.0, 500);
    }

    #[test]
    #[should_panic(expected = "unknown client_id")]
    fn client_fee_needs_a_registered_client() {
        let mut r = setup();
        ctx_deposit("x.near", 10);
        r.pay_client_fee("nobody".into());
    }

    #[test]
    #[should_panic(expected = "attach the fee as a deposit")]
    fn client_fee_needs_a_deposit() {
        let mut r = setup();
        ctx("backend.near", 1_000);
        r.register_client("acme".into());
        ctx_deposit("x.near", 0);
        r.pay_client_fee("acme".into());
    }

    #[test]
    #[should_panic(expected = "only an operator can register clients")]
    fn strangers_cannot_register_clients() {
        let mut r = setup();
        ctx("stranger.near", 1_000);
        r.register_client("acme".into());
    }

    #[test]
    #[should_panic(expected = "client already registered")]
    fn a_client_registers_once() {
        let mut r = setup();
        ctx("backend.near", 1_000);
        r.register_client("acme".into());
        r.register_client("acme".into());
    }
}
