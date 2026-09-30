# Orvyn registry

Deployed at [`registry.orvyn.near`](https://nearblocks.io/address/registry.orvyn.near). It keeps each Orvyn agent's hosting subscription, paid in USDC or USDT, and collects the fees agents and API clients pay Orvyn in NEAR.

## Subscriptions

An agent is registered with its ID, the NEAR account of the person who created it, and the agent's own NEAR account. Registration grants `trial_days` of hosting.

Anyone can pay for an agent's hosting with `ft_transfer_call` on an accepted token:

```
ft_transfer_call(receiver_id = "registry.orvyn.near", amount, msg = "<agent id>")
```

Whole months are credited at the token's monthly price, up to 36 per payment, and the remainder is refunded by the token. Paying before expiry extends the current period; paying after it starts a new one from now. Every payment is stored on chain, so invoices and receipts can be rebuilt from the registry alone.

## Fees

Orvyn's fees are paid to the registry with the agent's ID, never to an Orvyn wallet:

- `pay_fee(agent_id, kind)`, with the fee attached in NEAR. `kind` is `platform` (Orvyn's share of the agent's creator earnings) or `swap` (its NEAR swaps). Orvyn tokens launch paired with NEAR, so fees are paid in NEAR.
- `pay_client_fee(client_id)`: apps using the Orvyn Swap API pay the NEAR swap fee the same way.

Each fee is recorded against the agent or client and logged. The owner can withdraw collected NEAR fees with `withdraw_near`, which can never touch the balance the contract needs for its own storage.

## Methods

| Method | Who | |
|---|---|---|
| `register_agent(agent_id, owner, agent_account)` | operators | Registers an agent and grants the trial |
| `register_client(client_id)` | operators | Registers an API client |
| `ft_on_transfer(sender_id, amount, msg)` | accepted tokens | A subscription payment (through `ft_transfer_call`) |
| `pay_fee(agent_id, kind)` | anyone, with NEAR attached | An agent's platform or swap fee |
| `pay_client_fee(client_id)` | anyone, with NEAR attached | An API client's swap fee |
| `set_price(token, monthly_price)`, `remove_token(token)`, `set_trial_days(trial_days)` | owner | Payment settings |
| `add_operator(account)`, `remove_operator(account)`, `transfer_ownership(new_owner)` | owner | Access |
| `grant_days(agent_id, days)` | owner | Extends an agent's hosting |
| `withdraw(token, amount, receiver)` (1 yocto), `withdraw_near(amount, receiver)` | owner | Collected revenue |
| `upgrade()` (the new WASM as raw input), `migrate()` | owner / the contract | Code upgrades that keep state |
| `get_agent(agent_id)`, `is_active(agent_id)`, `get_payments(agent_id, from_index?, limit?)` | view | Subscriptions |
| `get_client(client_id)`, `get_fee_revenue()`, `get_config()`, `is_operator(account)`, `version()` | view | Everything else |

## Events

[NEP-297](https://github.com/near/NEPs/blob/master/neps/nep-0297.md) events with standard `orvyn_registry`: `agent_registered`, `client_registered`, `subscription_paid`, `fee_paid`, `fees_withdrawn`, `days_granted`, `upgraded`.

## Build and test

```bash
cargo test
cargo near build reproducible-wasm   # output: target/near/orvyn_registry.wasm
```
