use near_sdk::collections::LookupMap;
use near_sdk::json_types::{U128, U64};
use near_sdk::serde_json::json;
use near_sdk::{env, near, require, AccountId, Gas, NearToken, PanicOnDefault, Promise, PromiseResult};

// One settlement asset and one beneficiary per campaign. Amounts are in the
// asset's smallest units; deadlines are Unix timestamps in nanoseconds.
const NOT_CLAIMED: u8 = 0;
const PENDING: u8 = 1;
const PAID: u8 = 2;
const FT_TRANSFER_GAS: Gas = Gas::from_tgas(20);
const CALLBACK_GAS: Gas = Gas::from_tgas(15);
const MIN_CLAIM_GAS: u64 = 70_000_000_000_000;

#[near(serializers = [borsh])]
pub struct Campaign {
    creator: AccountId,
    beneficiary: AccountId,
    // None means native NEAR; Some is the NEP-141 token contract account.
    token: Option<AccountId>,
    category: String,
    goal: u128,
    deadline: u64,
    total: u128,
    payout_state: u8,
}

#[near(serializers = [borsh])]
pub struct Contribution {
    amount: u128,
    refund_state: u8,
}

#[near(serializers = [json])]
pub struct CampaignView {
    pub id: U64,
    pub creator: AccountId,
    pub beneficiary: AccountId,
    pub token: Option<AccountId>,
    pub category: String,
    pub goal: U128,
    pub deadline: U64,
    pub total: U128,
    pub status: String,
    pub payout_state: String,
}

#[near(serializers = [json])]
pub struct ContributionView {
    pub amount: U128,
    pub refund_state: String,
}

#[near(serializers = [json])]
struct DonationMessage {
    campaign_id: U64,
}

#[cfg(test)]
mod tests;

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct Contract {
    next_campaign_id: u64,
    campaigns: LookupMap<u64, Campaign>,
    contributions: LookupMap<(u64, AccountId), Contribution>,
    // Prepaid, non-withdrawable storage credit, denominated in yoctoNEAR.
    storage_credit: LookupMap<AccountId, u128>,
}

#[near]
impl Contract {
    /// Initialize in the same batch as deployment, signed by the contract account.
    #[init]
    #[private]
    pub fn new() -> Self {
        Self {
            next_campaign_id: 0,
            campaigns: LookupMap::new(b"c"),
            contributions: LookupMap::new(b"d"),
            storage_credit: LookupMap::new(b"s"),
        }
    }

    /// Prepay storage for the caller. Unused credit cannot be withdrawn in this MVP.
    #[payable]
    pub fn storage_deposit(&mut self) -> U128 {
        let account = env::predecessor_account_id();
        let deposit = env::attached_deposit().as_yoctonear();
        require!(deposit > 0, "Attach NEAR for storage");
        let before = env::storage_usage();
        let existing = self.storage_credit.get(&account);
        if existing.is_none() {
            self.storage_credit.insert(&account, &0);
        }
        let cost = Self::storage_cost_since(before);
        let balance = existing.unwrap_or(0).checked_add(deposit)
            .and_then(|v| v.checked_sub(cost))
            .expect("Deposit does not cover storage");
        self.storage_credit.insert(&account, &balance);
        U128(balance)
    }

    pub fn get_storage_credit(&self, account: AccountId) -> U128 {
        U128(self.storage_credit.get(&account).unwrap_or(0))
    }

    /// Create a campaign after prepaying storage. Token contracts must be trusted
    /// NEP-141 implementations; the contract must be registered with them.
    pub fn create_campaign(
        &mut self,
        goal: U128,
        deadline: U64,
        category: String,
        beneficiary: AccountId,
        token: Option<AccountId>,
    ) -> U64 {
        require!(goal.0 > 0, "Goal must be positive");
        require!(beneficiary != env::current_account_id(), "Beneficiary cannot be this contract");
        require!(deadline.0 > env::block_timestamp(), "Deadline must be in the future");
        require!(!category.is_empty() && category.len() <= 64, "Category must be 1 to 64 bytes");
        let creator = env::predecessor_account_id();
        let before = env::storage_usage();
        let id = self.next_campaign_id;
        self.next_campaign_id = id.checked_add(1).expect("Campaign ID overflow");
        let campaign = Campaign {
            creator: creator.clone(), beneficiary: beneficiary.clone(), token: token.clone(),
            category: category.clone(), goal: goal.0, deadline: deadline.0,
            total: 0, payout_state: NOT_CLAIMED,
        };
        self.campaigns.insert(&id, &campaign);
        self.charge_storage(&creator, before);
        Self::event("campaign_created", json!({
            "campaign_id": id.to_string(), "creator": creator, "beneficiary": beneficiary,
            "token": token, "category": category, "goal": goal.0.to_string(),
            "deadline": deadline.0.to_string()
        }));
        U64(id)
    }

    /// Donate attached native NEAR to a native-NEAR campaign.
    #[payable]
    pub fn donate(&mut self, campaign_id: U64) {
        let amount = env::attached_deposit().as_yoctonear();
        let donor = env::predecessor_account_id();
        let campaign = self.campaigns.get(&campaign_id.0).expect("Campaign not found");
        require!(campaign.token.is_none(), "Campaign does not accept native NEAR");
        self.record_donation(campaign_id.0, campaign, donor, amount);
    }

    /// NEP-141 receiver. Send via ft_transfer_call with message
    /// {"campaign_id":"0"}. The token sender is the recorded donor.
    /// Rejected transfers return the full amount as unused.
    pub fn ft_on_transfer(&mut self, sender_id: AccountId, amount: U128, msg: String) -> U128 {
        let Ok(message) = near_sdk::serde_json::from_str::<DonationMessage>(&msg) else {
            return amount;
        };
        let Some(campaign) = self.campaigns.get(&message.campaign_id.0) else {
            return amount;
        };
        if campaign.token.as_ref() != Some(&env::predecessor_account_id())
            || env::block_timestamp() >= campaign.deadline || amount.0 == 0
        {
            return amount;
        }
        // A panic (e.g. insufficient storage credit) rolls back this receipt;
        // a compliant NEP-141 resolver then treats the full amount as unused.
        self.record_donation(message.campaign_id.0, campaign, sender_id, amount.0);
        U128(0)
    }

    /// After a successful campaign ends, anyone may initiate the beneficiary's payout.
    /// For FT campaigns attach exactly 1 yoctoNEAR for the token transfer.
    #[payable]
    pub fn claim_payout(&mut self, campaign_id: U64) -> Promise {
        let id = campaign_id.0;
        let mut campaign = self.campaigns.get(&id).expect("Campaign not found");
        require!(env::block_timestamp() >= campaign.deadline, "Campaign is still active");
        require!(campaign.total >= campaign.goal, "Goal was not reached");
        require!(campaign.payout_state == NOT_CLAIMED, "Payout is pending or already paid");
        Self::validate_claim_call(&campaign.token);
        campaign.payout_state = PENDING;
        self.campaigns.insert(&id, &campaign);
        Self::event("payout_pending", json!({"campaign_id": id.to_string(),
            "beneficiary": campaign.beneficiary, "amount": campaign.total.to_string()}));
        Self::transfer(&campaign.token, campaign.beneficiary.clone(), campaign.total)
            .then(Self::ext(env::current_account_id()).with_static_gas(CALLBACK_GAS)
                .on_payout_complete(campaign_id))
    }

    /// After an unsuccessful campaign ends, anyone may initiate a donor's refund.
    /// For FT campaigns attach exactly 1 yoctoNEAR for the token transfer.
    #[payable]
    pub fn claim_refund(&mut self, campaign_id: U64, donor: AccountId) -> Promise {
        let id = campaign_id.0;
        let campaign = self.campaigns.get(&id).expect("Campaign not found");
        require!(env::block_timestamp() >= campaign.deadline, "Campaign is still active");
        require!(campaign.total < campaign.goal, "Goal was reached");
        let key = (id, donor.clone());
        let mut contribution = self.contributions.get(&key).expect("No contribution for donor");
        require!(contribution.refund_state == NOT_CLAIMED, "Refund is pending or already paid");
        Self::validate_claim_call(&campaign.token);
        contribution.refund_state = PENDING;
        self.contributions.insert(&key, &contribution);
        Self::event("refund_pending", json!({"campaign_id": id.to_string(),
            "donor": donor, "amount": contribution.amount.to_string()}));
        Self::transfer(&campaign.token, donor.clone(), contribution.amount)
            .then(Self::ext(env::current_account_id()).with_static_gas(CALLBACK_GAS)
                .on_refund_complete(campaign_id, donor))
    }

    #[private]
    pub fn on_payout_complete(&mut self, campaign_id: U64) -> bool {
        require!(env::promise_results_count() == 1, "Expected one promise result");
        let mut campaign = self.campaigns.get(&campaign_id.0).expect("Campaign not found");
        require!(campaign.payout_state == PENDING, "Payout is not pending");
        let success = matches!(env::promise_result(0), PromiseResult::Successful(_));
        campaign.payout_state = if success { PAID } else { NOT_CLAIMED };
        self.campaigns.insert(&campaign_id.0, &campaign);
        Self::event(if success { "payout_succeeded" } else { "payout_failed" },
            json!({"campaign_id": campaign_id.0.to_string(),
                "beneficiary": campaign.beneficiary, "amount": campaign.total.to_string()}));
        success
    }

    #[private]
    pub fn on_refund_complete(&mut self, campaign_id: U64, donor: AccountId) -> bool {
        require!(env::promise_results_count() == 1, "Expected one promise result");
        let key = (campaign_id.0, donor.clone());
        let mut contribution = self.contributions.get(&key).expect("Contribution not found");
        require!(contribution.refund_state == PENDING, "Refund is not pending");
        let success = matches!(env::promise_result(0), PromiseResult::Successful(_));
        contribution.refund_state = if success { PAID } else { NOT_CLAIMED };
        self.contributions.insert(&key, &contribution);
        Self::event(if success { "refund_succeeded" } else { "refund_failed" },
            json!({"campaign_id": campaign_id.0.to_string(), "donor": donor,
                "amount": contribution.amount.to_string()}));
        success
    }

    pub fn get_campaign(&self, campaign_id: U64) -> Option<CampaignView> {
        self.campaigns.get(&campaign_id.0).map(|c| {
            let status = if env::block_timestamp() < c.deadline { "active" }
                else if c.total >= c.goal { "successful" } else { "failed" };
            CampaignView { id: campaign_id, creator: c.creator, beneficiary: c.beneficiary,
                token: c.token, category: c.category, goal: U128(c.goal), deadline: U64(c.deadline),
                total: U128(c.total), status: status.to_string(),
                payout_state: Self::state_name(c.payout_state).to_string() }
        })
    }

    pub fn get_contribution(&self, campaign_id: U64, donor: AccountId) -> Option<ContributionView> {
        self.contributions.get(&(campaign_id.0, donor)).map(|c| ContributionView {
            amount: U128(c.amount), refund_state: Self::state_name(c.refund_state).to_string(),
        })
    }

    pub fn get_campaign_count(&self) -> U64 { U64(self.next_campaign_id) }

    fn record_donation(&mut self, id: u64, mut campaign: Campaign, donor: AccountId, amount: u128) {
        require!(env::block_timestamp() < campaign.deadline, "Campaign has ended");
        require!(amount > 0, "Donation must be positive");
        let before = env::storage_usage();
        let key = (id, donor.clone());
        let mut contribution = self.contributions.get(&key).unwrap_or(Contribution {
            amount: 0, refund_state: NOT_CLAIMED,
        });
        contribution.amount = contribution.amount.checked_add(amount).expect("Contribution overflow");
        campaign.total = campaign.total.checked_add(amount).expect("Campaign total overflow");
        self.contributions.insert(&key, &contribution);
        self.campaigns.insert(&id, &campaign);
        self.charge_storage(&donor, before);
        Self::event("donation", json!({"campaign_id": id.to_string(), "donor": donor,
            "amount": amount.to_string(), "total": campaign.total.to_string()}));
    }

    fn charge_storage(&mut self, account: &AccountId, before: u64) {
        let cost = Self::storage_cost_since(before);
        let credit = self.storage_credit.get(account).expect("Prepay storage first");
        let remaining = credit.checked_sub(cost).expect("Insufficient storage credit");
        self.storage_credit.insert(account, &remaining);
    }

    fn storage_cost_since(before: u64) -> u128 {
        u128::from(env::storage_usage().saturating_sub(before))
            .checked_mul(env::storage_byte_cost().as_yoctonear()).expect("Storage cost overflow")
    }

    fn validate_claim_call(token: &Option<AccountId>) {
        require!(env::prepaid_gas().as_gas() >= MIN_CLAIM_GAS, "Attach at least 70 Tgas");
        let expected = if token.is_some() { 1 } else { 0 };
        require!(env::attached_deposit().as_yoctonear() == expected,
            "Attach 1 yoctoNEAR for FT claims, or 0 for NEAR claims");
    }

    fn transfer(token: &Option<AccountId>, recipient: AccountId, amount: u128) -> Promise {
        match token {
            None => Promise::new(recipient).transfer(NearToken::from_yoctonear(amount)),
            Some(token) => Promise::new(token.clone()).function_call(
                "ft_transfer".to_string(),
                json!({"receiver_id": recipient, "amount": amount.to_string()}).to_string().into_bytes(),
                NearToken::from_yoctonear(1), FT_TRANSFER_GAS,
            ),
        }
    }

    fn state_name(state: u8) -> &'static str {
        match state { NOT_CLAIMED => "unclaimed", PENDING => "pending", PAID => "paid", _ => "unknown" }
    }

    fn event(event: &str, data: near_sdk::serde_json::Value) {
        env::log_str(&format!("EVENT_JSON:{}", json!({
            "standard": "nearfund", "version": "1.0.0", "event": event, "data": [data]
        })));
    }
}
