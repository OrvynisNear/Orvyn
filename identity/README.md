# OrvynID

Deployed at [`id.orvyn.near`](https://nearblocks.io/address/id.orvyn.near). It gives each Orvyn agent a named NEAR account, `<handle>.id.orvyn.near`, and keeps the agent's identity record on chain.

## Identities

When an agent is registered, the contract creates `<handle>.id.orvyn.near` with the agent's own ed25519 key as its only full-access key, and funds it with `account_deposit` (0.01 NEAR by default). Neither the contract nor Orvyn holds a key to the new account.

The record holds the agent's ID, the person who created it, its public key, its wallets (NEAR, EVM and Solana), its token, a link to its full identity document, an optional link to its [ERC-8004](https://eips.ethereum.org/EIPS/eip-8004) registration, and its status:

| Status | Meaning |
|---|---|
| `Pending` | The account is being created. If creation fails, the registration is rolled back |
| `Active` | Live |
| `Suspended` | Temporarily not verifying; can be reactivated |
| `Retired` | Permanent |

Handles are 2–32 characters of `a-z`, `0-9` and single hyphens. An agent and a handle can each have only one OrvynID.

## Methods

| Method | Who | |
|---|---|---|
| `register(handle, agent_id, owner, public_key, wallets, token, profile_uri)` | operators | Creates the account and the record |
| `update(handle, token?, wallets?, profile_uri?, erc8004?)` | operators | Updates a record (not a retired one) |
| `set_status(handle, status)` | operators; the agent's owner can retire it | Suspends, reactivates or retires |
| `set_account_deposit(account_deposit)`, `add_operator(account)`, `remove_operator(account)`, `transfer_ownership(new_owner)` | owner | Settings and access |
| `upgrade()` (the new WASM as raw input), `migrate()` | owner / the contract | Code upgrades that keep state |
| `get(handle)`, `get_by_agent(agent_id)`, `get_by_account(account)`, `is_active(account)` | view | Look up an identity |
| `get_config()`, `is_operator(account)`, `version()` | view | Settings |

## Events

[NEP-297](https://github.com/near/NEPs/blob/master/neps/nep-0297.md) events with standard `orvyn_id`: `identity_registered`, `identity_failed`, `identity_updated`, `identity_status`, `upgraded`.

## Build and test

```bash
cargo test
cargo near build reproducible-wasm   # output: target/near/orvyn_identity.wasm
```
