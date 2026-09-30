//! OrvynID: the identity layer for Orvyn AI agents.
//!
//! Deployed at `id.orvyn.near`. For each agent, the Orvyn backend (an operator) calls `register`,
//! which creates the sub-account `<handle>.id.orvyn.near` with the agent's own ed25519 public key
//! (its Dynamic MPC wallet) as the only full-access key, and stores the agent's identity record:
//! owner, wallets on every chain, token, profile URI and its ERC-8004 mirror.
//!
//! The account belongs to the agent: Orvyn holds no key to it. What Orvyn controls is the record's
//! `status`, which verifiers check, so a suspended or retired identity stops verifying.
//! An agent proves who it is by signing a NEP-413 message as its named account; anyone can check
//! that signature against the account's access keys on chain and the record here.
//!
//! Upgradeable by the owner (`upgrade`, then `migrate` on the new code), keeping all state.

use near_sdk::json_types::{U128, U64};
use near_sdk::serde::{Deserialize, Serialize};
use near_sdk::store::{IterableSet, LookupMap};
use near_sdk::{env, near, require, AccountId, BorshStorageKey, Gas, GasWeight, NearToken, PanicOnDefault, Promise, PublicKey};

const GAS_FOR_CALLBACK: Gas = Gas::from_tgas(10);
/// Minimum gas for `migrate` on the new code; it also gets all gas left over after `upgrade`.
const GAS_FOR_MIGRATE: Gas = Gas::from_tgas(20);

#[derive(BorshStorageKey)]
#[near]
enum StorageKey {
    Operators,
    Records,
    ByAgent,
}

#[derive(Clone, PartialEq, Debug)]
#[near(serializers = [borsh, json])]
pub enum Status {
    /// Account creation in flight.
    Pending,
    Active,
    /// Temporarily not verifying (e.g. abuse); can be reactivated.
    Suspended,
    /// Permanently retired by the operator or the owner.
    Retired,
}

#[derive(Clone)]
#[near(serializers = [borsh, json])]
pub struct Wallets {
    /// The agent's NEAR implicit account (holds its funds and is its token's creator).
    pub near: AccountId,
    /// EVM address (also its Hyperliquid account).
    pub evm: String,
    /// Solana address.
    pub solana: String,
}

/// The agent's mirror in the ERC-8004 Identity Registry.
#[derive(Clone)]
#[near(serializers = [borsh, json])]
pub struct Erc8004Ref {
    /// CAIP-2 style chain, e.g. "eip155:8453".
    pub chain: String,
    pub registry: String,
    pub agent_id: String,
}

#[derive(Clone)]
#[near(serializers = [borsh, json])]
pub struct Identity {
    pub handle: String,
    pub agent_id: String,
    /// `<handle>.<this contract>`.
    pub account: AccountId,
    /// The human who created the agent.
    pub owner: AccountId,
    /// The agent's key, the only full-access key on `account`.
    pub public_key: PublicKey,
    pub wallets: Wallets,
    pub token: Option<AccountId>,
    /// Where the full identity document lives (e.g. https://atlas.orvyn.cash/.well-known/orvyn-id.json).
    pub profile_uri: String,
    pub erc8004: Option<Erc8004Ref>,
    pub status: Status,
    pub created_at_ms: U64,
    pub updated_at_ms: U64,
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct OrvynId {
    owner: AccountId,
    operators: IterableSet<AccountId>,
    records: LookupMap<String, Identity>,
    by_agent: LookupMap<String, String>,
    count: u64,
    /// NEAR sent to each new named account (it only needs to exist and hold its key).
    account_deposit: NearToken,
}

fn emit(event: &str, data: near_sdk::serde_json::Value) {
    env::log_str(&format!(
        "EVENT_JSON:{}",
        near_sdk::serde_json::json!({ "standard": "orvyn_id", "version": "1.0.0", "event": event, "data": [data] })
    ));
}

/// Handles become NEAR account parts: 2-32 of a-z, 0-9 and single hyphens, not at the ends.
fn valid_handle(h: &str) -> bool {
    let b = h.as_bytes();
    (2..=32).contains(&b.len())
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
        && b[0] != b'-'
        && b[b.len() - 1] != b'-'
        && !h.contains("--")
}

#[derive(Serialize, Deserialize)]
#[serde(crate = "near_sdk::serde")]
pub struct Config {
    pub owner: AccountId,
    pub account_deposit: U128,
    pub count: u64,
}

#[near]
impl OrvynId {
    #[init]
    pub fn new(owner: AccountId, account_deposit: U128) -> Self {
        Self {
            owner,
            operators: IterableSet::new(StorageKey::Operators),
            records: LookupMap::new(StorageKey::Records),
            by_agent: LookupMap::new(StorageKey::ByAgent),
            count: 0,
            account_deposit: NearToken::from_yoctonear(account_deposit.0),
        }
    }

    // ---------- Admin ----------

    fn assert_owner(&self) {
        require!(env::predecessor_account_id() == self.owner, "only the contract owner");
    }

    fn assert_operator(&self) {
        let caller = env::predecessor_account_id();
        require!(caller == self.owner || self.operators.contains(&caller), "only an operator");
    }

    pub fn add_operator(&mut self, account: AccountId) {
        self.assert_owner();
        self.operators.insert(account);
    }

    pub fn remove_operator(&mut self, account: AccountId) {
        self.assert_owner();
        self.operators.remove(&account);
    }

    pub fn set_account_deposit(&mut self, account_deposit: U128) {
        self.assert_owner();
        self.account_deposit = NearToken::from_yoctonear(account_deposit.0);
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

    // ---------- Identities ----------

    /// Operators only. Creates `<handle>.<this>` with the agent's key and records the identity
    /// (Pending until the account exists, then Active).
    #[allow(clippy::too_many_arguments)]
    pub fn register(
        &mut self,
        handle: String,
        agent_id: String,
        owner: AccountId,
        public_key: PublicKey,
        wallets: Wallets,
        token: Option<AccountId>,
        profile_uri: String,
    ) -> Promise {
        self.assert_operator();
        require!(valid_handle(&handle), "handle: 2-32 of a-z, 0-9 and single hyphens");
        require!(!agent_id.is_empty() && agent_id.len() <= 64, "agent_id: 1-64 characters");
        require!(profile_uri.len() <= 256, "profile_uri too long");
        require!(self.records.get(&handle).is_none(), "handle already has an OrvynID");
        require!(self.by_agent.get(&agent_id).is_none(), "agent already has an OrvynID");
        require!(
            env::account_balance().as_yoctonear() > self.account_deposit.as_yoctonear() + NearToken::from_near(1).as_yoctonear(),
            "the OrvynID contract needs funding"
        );

        let account: AccountId = format!("{}.{}", handle, env::current_account_id()).parse().unwrap();
        let now = U64(env::block_timestamp_ms());
        let identity = Identity {
            handle: handle.clone(),
            agent_id: agent_id.clone(),
            account: account.clone(),
            owner,
            public_key: public_key.clone(),
            wallets,
            token,
            profile_uri,
            erc8004: None,
            status: Status::Pending,
            created_at_ms: now,
            updated_at_ms: now,
        };
        self.records.insert(handle.clone(), identity);
        self.by_agent.insert(agent_id, handle.clone());

        Promise::new(account)
            .create_account()
            .transfer(self.account_deposit)
            .add_full_access_key(public_key)
            .then(Self::ext(env::current_account_id()).with_static_gas(GAS_FOR_CALLBACK).on_account_created(handle))
    }

    /// Activates the identity once its account exists, or rolls the record back.
    #[private]
    pub fn on_account_created(&mut self, handle: String) -> bool {
        // Account creation returns nothing, so cap the result read at 0 bytes.
        let created = env::promise_result_checked(0, 0).is_ok();
        let Some(mut identity) = self.records.get(&handle).cloned() else { return false };
        if created {
            identity.status = Status::Active;
            identity.updated_at_ms = U64(env::block_timestamp_ms());
            self.records.insert(handle.clone(), identity.clone());
            self.count += 1;
            emit(
                "identity_registered",
                near_sdk::serde_json::json!({ "handle": handle, "account": identity.account, "agent_id": identity.agent_id, "owner": identity.owner }),
            );
        } else {
            self.records.remove(&handle);
            self.by_agent.remove(&identity.agent_id);
            emit("identity_failed", near_sdk::serde_json::json!({ "handle": handle, "account": identity.account }));
        }
        created
    }

    /// Operators only: update what changes over an agent's life. `None` leaves a field as is.
    pub fn update(
        &mut self,
        handle: String,
        token: Option<AccountId>,
        wallets: Option<Wallets>,
        profile_uri: Option<String>,
        erc8004: Option<Erc8004Ref>,
    ) {
        self.assert_operator();
        let mut identity = self.records.get(&handle).cloned().unwrap_or_else(|| env::panic_str("unknown handle"));
        require!(identity.status != Status::Retired, "identity is retired");
        if let Some(t) = token {
            identity.token = Some(t);
        }
        if let Some(w) = wallets {
            identity.wallets = w;
        }
        if let Some(p) = profile_uri {
            require!(p.len() <= 256, "profile_uri too long");
            identity.profile_uri = p;
        }
        if let Some(e) = erc8004 {
            identity.erc8004 = Some(e);
        }
        identity.updated_at_ms = U64(env::block_timestamp_ms());
        self.records.insert(handle.clone(), identity);
        emit("identity_updated", near_sdk::serde_json::json!({ "handle": handle }));
    }

    /// Operators can suspend, reactivate or retire an identity; its owner can retire it.
    pub fn set_status(&mut self, handle: String, status: Status) {
        let mut identity = self.records.get(&handle).cloned().unwrap_or_else(|| env::panic_str("unknown handle"));
        let caller = env::predecessor_account_id();
        let is_operator = caller == self.owner || self.operators.contains(&caller);
        require!(is_operator || (caller == identity.owner && status == Status::Retired), "not allowed");
        require!(identity.status != Status::Pending && status != Status::Pending, "identity is still being created");
        require!(identity.status != Status::Retired, "identity is retired");
        identity.status = status.clone();
        identity.updated_at_ms = U64(env::block_timestamp_ms());
        self.records.insert(handle.clone(), identity);
        emit("identity_status", near_sdk::serde_json::json!({ "handle": handle, "status": status }));
    }

    // ---------- Views ----------

    pub fn get(&self, handle: String) -> Option<Identity> {
        self.records.get(&handle).cloned()
    }

    pub fn get_by_agent(&self, agent_id: String) -> Option<Identity> {
        self.by_agent.get(&agent_id).and_then(|h| self.records.get(h).cloned())
    }

    /// Resolve a named account (`atlas.id.orvyn.near`) to its identity.
    pub fn get_by_account(&self, account: AccountId) -> Option<Identity> {
        let suffix = format!(".{}", env::current_account_id());
        account.as_str().strip_suffix(&suffix).and_then(|h| self.records.get(h).cloned())
    }

    /// True only for an Active identity whose account is `account`.
    pub fn is_active(&self, account: AccountId) -> bool {
        self.get_by_account(account).map(|i| i.status == Status::Active).unwrap_or(false)
    }

    pub fn get_config(&self) -> Config {
        Config { owner: self.owner.clone(), account_deposit: U128(self.account_deposit.as_yoctonear()), count: self.count }
    }

    pub fn is_operator(&self, account: AccountId) -> bool {
        self.operators.contains(&account)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use near_sdk::test_utils::VMContextBuilder;
    use near_sdk::{testing_env, PromiseResult, RuntimeFeesConfig};
    use near_sdk::test_vm_config;

    fn acc(s: &str) -> AccountId {
        s.parse().unwrap()
    }

    fn ctx(predecessor: &str) {
        let mut b = VMContextBuilder::new();
        b.current_account_id(acc("id.orvyn.near"))
            .predecessor_account_id(acc(predecessor))
            .account_balance(NearToken::from_near(10))
            .block_timestamp(5_000_000_000);
        testing_env!(b.build());
    }

    fn callback(result: PromiseResult) {
        let mut b = VMContextBuilder::new();
        b.current_account_id(acc("id.orvyn.near")).predecessor_account_id(acc("id.orvyn.near")).block_timestamp(6_000_000_000);
        testing_env!(b.build(), test_vm_config(), RuntimeFeesConfig::test(), Default::default(), vec![result]);
    }

    fn key() -> PublicKey {
        "ed25519:6E8sCci9badyRkXb3JoRpBj5p8C6Tw41ELDZoiihKEtp".parse().unwrap()
    }

    fn wallets() -> Wallets {
        Wallets { near: acc(&"a".repeat(64)), evm: "0x0000000000000000000000000000000000000001".into(), solana: "So1ana".into() }
    }

    fn setup() -> OrvynId {
        ctx("orvyn.near");
        let mut c = OrvynId::new(acc("orvyn.near"), U128(10u128.pow(22)));
        c.add_operator(acc("op1.orvyn.near"));
        c
    }

    fn register(c: &mut OrvynId, handle: &str, agent: &str) {
        ctx("op1.orvyn.near");
        let _ = c.register(handle.into(), agent.into(), acc("alice.near"), key(), wallets(), None, format!("https://{handle}.orvyn.cash/.well-known/orvyn-id.json"));
    }

    #[test]
    fn register_is_pending_until_the_account_exists() {
        let mut c = setup();
        register(&mut c, "atlas", "agent-1");
        let id = c.get("atlas".into()).unwrap();
        assert_eq!(id.status, Status::Pending);
        assert_eq!(id.account, acc("atlas.id.orvyn.near"));
        callback(PromiseResult::Successful(vec![]));
        assert!(c.on_account_created("atlas".into()));
        assert_eq!(c.get("atlas".into()).unwrap().status, Status::Active);
        assert!(c.is_active(acc("atlas.id.orvyn.near")));
        assert_eq!(c.get_by_agent("agent-1".into()).unwrap().handle, "atlas");
        assert_eq!(c.get_config().count, 1);
    }

    #[test]
    fn failed_account_creation_rolls_back() {
        let mut c = setup();
        register(&mut c, "atlas", "agent-1");
        callback(PromiseResult::Failed);
        assert!(!c.on_account_created("atlas".into()));
        assert!(c.get("atlas".into()).is_none());
        assert!(c.get_by_agent("agent-1".into()).is_none());
        // The handle and agent can register again.
        register(&mut c, "atlas", "agent-1");
        assert!(c.get("atlas".into()).is_some());
    }

    #[test]
    #[should_panic(expected = "only an operator")]
    fn strangers_cannot_register() {
        let mut c = setup();
        ctx("mallory.near");
        let _ = c.register("evil".into(), "x".into(), acc("mallory.near"), key(), wallets(), None, "".into());
    }

    #[test]
    #[should_panic(expected = "handle already has an OrvynID")]
    fn handles_are_unique() {
        let mut c = setup();
        register(&mut c, "atlas", "agent-1");
        register(&mut c, "atlas", "agent-2");
    }

    #[test]
    #[should_panic(expected = "agent already has an OrvynID")]
    fn one_identity_per_agent() {
        let mut c = setup();
        register(&mut c, "atlas", "agent-1");
        register(&mut c, "atlas2", "agent-1");
    }

    #[test]
    fn handle_rules() {
        for ok in ["ab", "atlas1938", "a-b", "x".repeat(32).as_str()] {
            assert!(valid_handle(ok), "{ok}");
        }
        for bad in ["a", "-ab", "ab-", "a--b", "Atlas", "a.b", "a_b", "x".repeat(33).as_str(), ""] {
            assert!(!valid_handle(bad), "{bad}");
        }
    }

    #[test]
    fn status_and_updates() {
        let mut c = setup();
        register(&mut c, "atlas", "agent-1");
        callback(PromiseResult::Successful(vec![]));
        c.on_account_created("atlas".into());

        ctx("op1.orvyn.near");
        c.update("atlas".into(), Some(acc("atlas.nearlytrade.near")), None, None, Some(Erc8004Ref { chain: "eip155:8453".into(), registry: "0x8004A169FB4a3325136EB29fA0ceB6D2e539a432".into(), agent_id: "42".into() }));
        let id = c.get("atlas".into()).unwrap();
        assert_eq!(id.token, Some(acc("atlas.nearlytrade.near")));
        assert_eq!(id.erc8004.unwrap().agent_id, "42");

        c.set_status("atlas".into(), Status::Suspended);
        assert!(!c.is_active(acc("atlas.id.orvyn.near")));
        c.set_status("atlas".into(), Status::Active);
        assert!(c.is_active(acc("atlas.id.orvyn.near")));

        // The owner can retire it (and nothing else); retired is final.
        ctx("alice.near");
        c.set_status("atlas".into(), Status::Retired);
        assert!(!c.is_active(acc("atlas.id.orvyn.near")));
    }

    #[test]
    #[should_panic(expected = "not allowed")]
    fn owner_cannot_reactivate() {
        let mut c = setup();
        register(&mut c, "atlas", "agent-1");
        callback(PromiseResult::Successful(vec![]));
        c.on_account_created("atlas".into());
        ctx("op1.orvyn.near");
        c.set_status("atlas".into(), Status::Suspended);
        ctx("alice.near");
        c.set_status("atlas".into(), Status::Active);
    }

    #[test]
    fn resolves_only_its_own_accounts() {
        let mut c = setup();
        register(&mut c, "atlas", "agent-1");
        assert!(c.get_by_account(acc("atlas.id.orvyn.near")).is_some());
        assert!(c.get_by_account(acc("atlas.evil.near")).is_none());
        assert!(!c.is_active(acc("atlas.id.orvyn.near"))); // still pending
    }

    fn ctx_upgrade(predecessor: &str, code: &[u8]) {
        let mut b = VMContextBuilder::new();
        b.current_account_id(acc("id.orvyn.near")).predecessor_account_id(acc(predecessor)).prepaid_gas(Gas::from_tgas(300));
        let mut c = b.build();
        c.input = code.into();
        testing_env!(c);
    }

    const WASM: &[u8] = b"\0asm\x01\0\0\0";

    #[test]
    fn owner_can_upgrade() {
        let c = setup();
        ctx_upgrade("orvyn.near", WASM);
        let _ = c.upgrade();
    }

    #[test]
    #[should_panic(expected = "only the contract owner")]
    fn only_the_owner_upgrades() {
        let c = setup();
        ctx_upgrade("stranger.near", WASM);
        let _ = c.upgrade();
    }

    #[test]
    #[should_panic(expected = "input must be the new contract WASM")]
    fn upgrade_needs_wasm() {
        let c = setup();
        ctx_upgrade("orvyn.near", b"{}");
        let _ = c.upgrade();
    }

    #[test]
    fn migrate_keeps_the_state() {
        let c = setup();
        env::state_write(&c);
        let m = OrvynId::migrate();
        let cfg = m.get_config();
        assert_eq!(cfg.owner, acc("orvyn.near"));
        assert_eq!(cfg.account_deposit, U128(10u128.pow(22)));
        assert_eq!(m.version(), env!("CARGO_PKG_VERSION"));
    }
}
