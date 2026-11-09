# Nearfund campaign contract

Rust / NEAR SDK implementation. This is an audited MVP; it has been compiled and tested in this workspace. Remix's Solidity compiler cannot compile it.

## Build and test

Install Rust and cargo-near, then run from this directory:

```sh
cargo test
cargo near build non-reproducible-wasm
```

Commit the generated Cargo.lock for reproducible dependency resolution. Deploy the generated WASM using NEAR tooling, batching deployment and `new {}` initialization together under the contract account. The initializer is private.

## Policy

- Anyone can create multiple campaigns. Each fixes its goal, deadline, category, beneficiary and a single asset.
- `token: null` means NEAR (24 decimal places). For USDC, use the independently verified NEP-141 contract for the selected network and that contract's actual decimals. Merely choosing a token contract does not certify it as USDC.
- A deadline is Unix epoch **nanoseconds**, JSON-encoded as a decimal string. Goals, donations and balances use integer smallest units, JSON-encoded as strings.
- Donations stop at the deadline. Reaching a goal does not end a campaign early.
- After the deadline: total >= goal permits one full beneficiary payout, including overfunding; otherwise each donor can claim their full contribution. No fees, early payouts, cancellations or partial refunds.
- Anyone can trigger a claim but cannot change its recipient. The creator has no privileged withdrawal authority.
- `total` is historical gross accepted donations, not a live escrow balance. Read payout/refund states to determine obligations.

## Usage

1. Creators and donors first call payable `storage_deposit {}` with NEAR to fund their persistent records; `get_storage_credit` returns remaining credit. A small amount such as 0.05 NEAR is usually ample for initial records, but the exact cost depends on storage size and network pricing. Unused credit is non-withdrawable in this MVP and is separate from donations.
2. Creator calls `create_campaign` with `goal`, `deadline`, `category`, `beneficiary`, and `token` (an account ID or null). No attached deposit. Store the returned ID.
3. Native donors call payable `donate {"campaign_id":"0"}` and attach the donation.
4. FT donors call the selected token's `ft_transfer_call` with this contract as `receiver_id`, an integer-string `amount`, and `msg` containing the JSON string `{"campaign_id":"0"}`. Attach the token's required 1 yoctoNEAR and sufficient gas (e.g. 150 Tgas). The Nearfund account must already be registered with that token; the sender must prepay Nearfund storage.
5. After the deadline, call `claim_payout {"campaign_id":"0"}` or `claim_refund {"campaign_id":"0","donor":"alice.testnet"}`. Attach at least 70 Tgas, and exactly 1 yoctoNEAR for FT claims or zero for NEAR claims. FT recipients must be registered with the token before transfer.

Views: `get_campaign`, `get_campaign_count`, `get_contribution`, `get_storage_credit`. Enumerate IDs from zero to count minus one with an off-chain indexer; no unbounded on-chain listing.

## Public ledger and asynchronous safety

NEP-297-formatted `nearfund` logs record campaign creation, donations, pending transfers and their callback outcomes. Index successful receipts to render the donation/withdrawal ledger; an attempted transfer is not proof of a completed payment.

A failed transfer resets its claim for retry only when its callback observes failure. Successful transfers mark claims paid. Pending claims cannot be concurrently paid again. If a callback itself fails, the claim stays pending: there is deliberately no timeout reset, since the transfer may already have succeeded. Receipt investigation and a carefully reviewed migration/reconciliation process would then be needed. Never blindly retry ambiguous transfers.

Only trusted, compliant token contracts should be used. A malicious FT contract can falsify donations in its own campaigns or return pathological results. Tokens are isolated per campaign, but this is not a vetted token registry. USDC account freezing, recipient registration, token implementation behavior and contract upgrade-key security remain operational risks. Beneficiary accounts must be controlled and able to receive the settlement asset. Direct native or FT transfers bypassing the donation entry points are not attributed or recoverable through this API.

## Integration boundaries

NEAR Intents swaps/cross-chain routing, social login, gas sponsorship, Next.js UI and indexing are separate integrations, not implemented here. An adapter must actually deliver the settlement asset through `donate` or `ft_transfer_call`. Refund ownership follows the on-chain predecessor or FT sender, so a router is recorded as donor if it sends the funds. Human attribution or refunds through a router require an authenticated routing design; arbitrary donor IDs are intentionally not accepted.

Milestones and co-signers are not implemented as on-chain approval rules in this version. Setting a properly configured Sputnik DAO or Trezu-controlled account as beneficiary places subsequent spending under that account's governance; it does not add campaign-level milestone approvals. These rules require a separate audited state machine before enabling early or partial payouts.

Before real funds: run unit tests, add sandbox tests against the exact USDC contract, test failed transfers and callback failures, verify storage/gas requirements, secure deployment keys, and obtain an independent audit.
