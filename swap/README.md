# Orvyn swap router

To be deployed at `swap.orvyn.near`. It trades Nearly tokens against **native NEAR** on the token's Rhea DCL pool in a **single transaction**, and takes Orvyn's fee on chain. The router wraps and unwraps NEAR itself: traders only send and receive NEAR.

## Buy

```
swap.orvyn.near.buy({ token, pool_id, min_out })   attach the NEAR to spend, 300 Tgas
```

The router takes the fee, registers the buyer on the token if needed (0.0013 NEAR from the attached amount), wraps the rest and swaps it with the output sent straight to the buyer (DCL's `swap_out_recipient`). `min_out` is the least the pool must pay out, before the token's buy tax. Whatever the swap doesn't use is unwrapped and refunded, with its share of the fee; a swap below `min_out` refunds everything.

## Sell

```
<token>.ft_transfer_call({ receiver_id: "swap.orvyn.near", amount, msg: "{\"pool_id\":\"…\",\"min_out\":\"…\"}" })   1 yocto, 300 Tgas
```

The router reads the token's sell tax (`get_tax`; untaxed tokens have none), asks the pool for the exact NEAR it pays for the tokens it will receive, checks it against `min_out` (yocto NEAR, before the fee), and swaps with that quote as the minimum, so it knows exactly what it gets. It unwraps it and sends the seller the NEAR less the fee. If the pool moves in between, the swap is refunded and so are the seller's tokens. The router must be registered on the token once (`storage_deposit` for `swap.orvyn.near`); apps add it to the same approval when needed.

## Fees and guarantees

- The fee (`fee_bps`, 0.3% at launch, capped at 1% in code) goes to the Orvyn registry in the same transaction: `pay_client_fee` under this router's client id.
- Only Nearly tokens (`*.nearlytrade.near`) on a pool that pairs them with wNEAR.
- It holds no trader funds between transactions. A sell that fills above its quote leaves the difference as wNEAR, which the owner can only sweep into the registry as fees.
- Owner-only: `set_fee_bps`, `set_paused`, `set_client_id`, `sweep`, `transfer_ownership`, `upgrade`.

## Methods

| Method | Who | |
|---|---|---|
| `buy(token, pool_id, min_out)` | anyone, with NEAR attached | Buy |
| `ft_on_transfer(sender_id, amount, msg)` | Nearly tokens (via `ft_transfer_call`) | Sell |
| `setup()` | owner, with the wNEAR storage deposit | One-time wNEAR registration |
| `get_config()`, `get_stats()`, `version()` | view | Settings, trade counts, volume and fees |

Events (NEP-297, standard `orvyn_swap`): `bought`, `sold`, `refunded`, `config_changed`, `upgraded`.

## Build and test

```bash
cargo test
cargo near build reproducible-wasm   # output: target/near/orvyn_swap.wasm
```

It is also tested end to end on a NEAR sandbox against the real code of Rhea's DCL, wNEAR and Nearly's plain and taxed tokens.
