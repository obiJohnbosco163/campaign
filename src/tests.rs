use super::*;
use near_sdk::test_utils::{accounts, VMContextBuilder};
use near_sdk::testing_env;

fn context(caller: AccountId, time: u64, deposit: NearToken) -> VMContextBuilder {
    let mut builder = VMContextBuilder::new();
    builder.current_account_id(accounts(0))
        .signer_account_id(caller.clone())
        .predecessor_account_id(caller)
        .block_timestamp(time)
        .account_balance(NearToken::from_near(1_000))
        .attached_deposit(deposit)
        .prepaid_gas(Gas::from_tgas(100));
    builder
}

fn set_context(caller: AccountId, time: u64, deposit: NearToken) {
    testing_env!(context(caller, time, deposit).build());
}

fn credit(contract: &mut Contract, account: AccountId) {
    set_context(account, 10, NearToken::from_near(1));
    contract.storage_deposit();
}

fn setup(token: Option<AccountId>) -> (Contract, U64) {
    set_context(accounts(0), 10, NearToken::from_yoctonear(0));
    let mut contract = Contract::new();
    credit(&mut contract, accounts(1));
    set_context(accounts(1), 10, NearToken::from_yoctonear(0));
    let id = contract.create_campaign(U128(100), U64(1000), "education".into(), accounts(1), token);
    credit(&mut contract, accounts(2));
    (contract, id)
}

fn donate(contract: &mut Contract, id: U64, amount: u128) {
    set_context(accounts(2), 500, NearToken::from_yoctonear(amount));
    contract.donate(id);
}

#[test]
fn stores_campaign_and_charges_storage_separately() {
    let (contract, id) = setup(None);
    let campaign = contract.get_campaign(id).unwrap();
    assert_eq!(campaign.goal.0, 100);
    assert_eq!(campaign.total.0, 0);
    assert_eq!(campaign.deadline.0, 1000);
    assert_eq!(campaign.beneficiary, accounts(1));
    assert_eq!(campaign.status, "active");
    assert_eq!(contract.get_campaign_count().0, 1);
    assert!(contract.get_storage_credit(accounts(1)).0 < NearToken::from_near(1).as_yoctonear());
}

#[test]
fn aggregates_donations_and_allows_overfunding() {
    let (mut contract, id) = setup(None);
    donate(&mut contract, id, 60);
    donate(&mut contract, id, 70);
    assert_eq!(contract.get_campaign(id).unwrap().total.0, 130);
    assert_eq!(contract.get_contribution(id, accounts(2)).unwrap().amount.0, 130);
}

#[test]
#[should_panic(expected = "Campaign has ended")]
fn rejects_donation_at_deadline() {
    let (mut contract, id) = setup(None);
    set_context(accounts(2), 1000, NearToken::from_yoctonear(100));
    contract.donate(id);
}

#[test]
#[should_panic(expected = "Campaign is still active")]
fn no_early_payout_even_after_goal_reached() {
    let (mut contract, id) = setup(None);
    donate(&mut contract, id, 100);
    set_context(accounts(1), 999, NearToken::from_yoctonear(0));
    let _ = contract.claim_payout(id);
}

#[test]
#[should_panic(expected = "Goal was not reached")]
fn unsuccessful_campaign_cannot_pay_beneficiary() {
    let (mut contract, id) = setup(None);
    donate(&mut contract, id, 50);
    set_context(accounts(1), 1000, NearToken::from_yoctonear(0));
    let _ = contract.claim_payout(id);
}

#[test]
#[should_panic(expected = "Goal was reached")]
fn successful_campaign_cannot_refund_donor() {
    let (mut contract, id) = setup(None);
    donate(&mut contract, id, 100);
    set_context(accounts(2), 1000, NearToken::from_yoctonear(0));
    let _ = contract.claim_refund(id, accounts(2));
}

#[test]
#[should_panic(expected = "Refund is pending or already paid")]
fn prevents_concurrent_refunds() {
    let (mut contract, id) = setup(None);
    donate(&mut contract, id, 50);
    set_context(accounts(2), 1000, NearToken::from_yoctonear(0));
    let _ = contract.claim_refund(id, accounts(2));
    let _ = contract.claim_refund(id, accounts(2));
}

#[allow(deprecated)]
fn callback_context(result: PromiseResult) {
    near_sdk::test_utils::testing_env_with_promise_results(
        context(accounts(0), 1001, NearToken::from_yoctonear(0)).build(), result,
    );
}

#[test]
fn failed_payout_can_retry_but_success_is_final() {
    let (mut contract, id) = setup(None);
    donate(&mut contract, id, 100);
    set_context(accounts(3), 1000, NearToken::from_yoctonear(0));
    let _ = contract.claim_payout(id);
    assert_eq!(contract.get_campaign(id).unwrap().payout_state, "pending");
    callback_context(PromiseResult::Failed);
    assert!(!contract.on_payout_complete(id));
    assert_eq!(contract.get_campaign(id).unwrap().payout_state, "unclaimed");
    set_context(accounts(3), 1002, NearToken::from_yoctonear(0));
    let _ = contract.claim_payout(id);
    callback_context(PromiseResult::Successful(vec![]));
    assert!(contract.on_payout_complete(id));
    assert_eq!(contract.get_campaign(id).unwrap().payout_state, "paid");
    assert_eq!(contract.get_campaign(id).unwrap().total.0, 100);
}

#[test]
fn failed_refund_preserves_amount_for_retry() {
    let (mut contract, id) = setup(None);
    donate(&mut contract, id, 50);
    set_context(accounts(3), 1000, NearToken::from_yoctonear(0));
    let _ = contract.claim_refund(id, accounts(2));
    callback_context(PromiseResult::Failed);
    assert!(!contract.on_refund_complete(id, accounts(2)));
    let contribution = contract.get_contribution(id, accounts(2)).unwrap();
    assert_eq!(contribution.amount.0, 50);
    assert_eq!(contribution.refund_state, "unclaimed");
    set_context(accounts(3), 1002, NearToken::from_yoctonear(0));
    let _ = contract.claim_refund(id, accounts(2));
    callback_context(PromiseResult::Successful(vec![]));
    assert!(contract.on_refund_complete(id, accounts(2)));
    assert_eq!(contract.get_contribution(id, accounts(2)).unwrap().refund_state, "paid");
}

#[test]
fn verifies_ft_contract_and_records_real_sender() {
    let (mut contract, id) = setup(Some(accounts(4)));
    let msg = format!("{{\"campaign_id\":\"{}\"}}", id.0);
    set_context(accounts(3), 500, NearToken::from_yoctonear(0));
    assert_eq!(contract.ft_on_transfer(accounts(2), U128(100), msg.clone()).0, 100);
    assert_eq!(contract.get_campaign(id).unwrap().total.0, 0);
    set_context(accounts(4), 500, NearToken::from_yoctonear(0));
    assert_eq!(contract.ft_on_transfer(accounts(2), U128(100), msg.clone()).0, 0);
    assert_eq!(contract.get_contribution(id, accounts(2)).unwrap().amount.0, 100);
    assert!(contract.get_contribution(id, accounts(4)).is_none());
    set_context(accounts(4), 1000, NearToken::from_yoctonear(0));
    assert_eq!(contract.ft_on_transfer(accounts(2), U128(10), msg).0, 10);
}

#[test]
fn malformed_ft_message_is_returned_unused() {
    let (mut contract, _) = setup(Some(accounts(4)));
    set_context(accounts(4), 500, NearToken::from_yoctonear(0));
    assert_eq!(contract.ft_on_transfer(accounts(2), U128(100), "not json".into()).0, 100);
}

#[test]
#[should_panic(expected = "Campaign does not accept native NEAR")]
fn does_not_mix_assets() {
    let (mut contract, id) = setup(Some(accounts(4)));
    donate(&mut contract, id, 100);
}

#[test]
#[should_panic(expected = "Prepay storage first")]
fn requires_donor_storage_prepayment() {
    let (mut contract, id) = setup(None);
    set_context(accounts(3), 500, NearToken::from_yoctonear(10));
    contract.donate(id);
}
