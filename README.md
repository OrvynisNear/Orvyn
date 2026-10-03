# Orvyn contracts

The NEAR smart contracts behind [Orvyn](https://orvyn.cash), a launchpad for autonomous AI agents on NEAR. Each Orvyn agent launches its own token and runs with its own wallets, and pays for its hosting and Orvyn's fees on chain through these contracts.

| Contract | Mainnet account | What it does |
|---|---|---|
| [Registry](registry) | [`registry.orvyn.near`](https://nearblocks.io/address/registry.orvyn.near) | Hosting subscriptions paid in USDC or USDT per agent, and the NEAR fees agents and API clients pay Orvyn |
| [OrvynID](identity) | [`id.orvyn.near`](https://nearblocks.io/address/id.orvyn.near) | A named NEAR account (`<handle>.id.orvyn.near`) and an on-chain identity record for each agent |
| [Swap router](swap) | [`swap.orvyn.near`](https://nearblocks.io/address/swap.orvyn.near) | Buys Nearly tokens with native NEAR on Rhea DCL in one transaction (sells are off since v0.2), with Orvyn's fee taken on chain |

All three are owned by `orvyn.near`. The Orvyn backend acts through `op1.orvyn.near`, whose keys are function-call keys limited to the few operator methods listed below.

## Verifying the deployed code

The contracts are built reproducibly with [`cargo near`](https://github.com/near/cargo-near) ([NEP-330](https://github.com/near/NEPs/blob/master/neps/nep-0330.md)). Each deployed contract reports the source it was built from:

```bash
near contract call-function as-read-only registry.orvyn.near contract_source_metadata json-args {} network-config mainnet now
```

The answer links to a commit in this repository and names the Docker image the build ran in (pinned by digest in each contract's `Cargo.toml`). Building that commit in that image gives the exact bytes deployed on chain. [NearBlocks](https://nearblocks.io) checks this through [SourceScan](https://github.com/SourceScan/verification-guide) and shows the verified source on each contract's **Contract** tab.

To check it yourself (needs Docker):

```bash
git clone https://github.com/OrvynisNear/Orvyn && cd Orvyn
git checkout <commit from contract_source_metadata>
cd registry            # or identity
cargo near build reproducible-wasm
sha256sum target/near/*.wasm
```

The base58 form of that SHA-256 is the `code_hash` NEAR reports for the account. The [Test and build](.github/workflows/build.yml) workflow runs the same build on every commit and prints both hashes in its summary.

## Trust model

- **Owner (`orvyn.near`).** Sets prices and settings, manages operators, withdraws collected revenue and upgrades the code. It can never move a user's or an agent's funds: the contracts only hold what was paid to them.
- **Operators (`op1.orvyn.near`).** The Orvyn backend. On the registry they can only register agents and API clients; on OrvynID they can only register, update and change the status of identities. Their keys can't call anything else.
- **Upgrades.** Both contracts are upgradeable by the owner, keeping their state. `upgrade()` deploys the new code and runs its `migrate()` in the same batch, so a failed migration reverts the deploy and the old code keeps running. Every upgrade is a new commit here, built and verified the same way. The contract accounts also hold a full-access key of the owner, from their setup; the account's key list on NearBlocks shows which keys exist.
- **Events.** Every state change is logged as a [NEP-297](https://github.com/near/NEPs/blob/master/neps/nep-0297.md) event (standards `orvyn_registry` and `orvyn_id`), so it can be indexed and audited.

## Build and test

Requirements: Rust (see [rustup](https://rustup.rs)) with the `wasm32-unknown-unknown` target, and [`cargo-near`](https://github.com/near/cargo-near#installation).

```bash
cd registry            # or identity
cargo test             # unit tests
cargo near build reproducible-wasm       # the verifiable build (Docker)
cargo near build non-reproducible-wasm   # a quick local build
```

The output is `target/near/<crate>.wasm`.

## Layout

```
registry/   the registry contract (orvyn-registry)
identity/   the OrvynID contract (orvyn-identity)
swap/       the swap router (orvyn-swap)
.github/    tests and the reproducible build on every commit
```
