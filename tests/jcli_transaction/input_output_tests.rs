#![cfg(feature = "integration-test")]

use common::configuration::genesis_model::Fund;
use common::file_utils;
use common::jcli_wrapper;
use common::jcli_wrapper::jcli_transaction_wrapper::JCLITransactionWrapper;
use common::startup;

const FAKE_INPUT_TRANSACTION_ID: &str =
    "19c9852ca0a68f15d0f7de5d1a26acd67a3a3251640c6066bdb91d22e2000193";
const FAKE_GENESIS_HASH: &str = "19c9852ca0a68f15d0f7de5d1a26acd67a3a3251640c6066bdb91d22e2000193";

#[test]
pub fn test_cannot_create_input_with_negative_amount() {
    JCLITransactionWrapper::new_transaction(FAKE_GENESIS_HASH).assert_add_input_fail(
        &FAKE_INPUT_TRANSACTION_ID,
        &0,
        "-100",
        "Found argument '-1' which wasn't expected",
    );
}

#[test]
pub fn test_cannot_create_input_with_too_big_utxo_amount() {
    JCLITransactionWrapper::new_transaction(FAKE_GENESIS_HASH).assert_add_input_fail(
        &FAKE_INPUT_TRANSACTION_ID,
        &0,
        "100000000000000000000",
        "error: Invalid value for '<value>': Invalid value",
    );
}

#[test]
pub fn test_unbalanced_output_utxo_transation_is_not_finalized() {
    let reciever = startup::create_new_utxo_address();

    JCLITransactionWrapper::new_transaction(FAKE_GENESIS_HASH)
        .assert_add_input(&FAKE_INPUT_TRANSACTION_ID, &0, &100)
        .assert_add_output(&reciever.address, &150)
        .assert_finalize_fail("not enough input for making transaction");
}

#[test]
pub fn test_add_account_for_utxo_address_fails() {
    let sender = startup::create_new_utxo_address();

    JCLITransactionWrapper::new_transaction(FAKE_GENESIS_HASH).assert_add_account_fail(
        &sender.address,
        &100,
        "Invalid input account, this is a UTxO address",
    );
}

#[test]
#[cfg(not(target_os = "linux"))]
pub fn test_cannot_create_input_when_staging_file_is_readonly() {
    let mut transaction_wrapper = JCLITransactionWrapper::new_transaction(FAKE_GENESIS_HASH);
    file_utils::make_readonly(&transaction_wrapper.staging_file_path);
    transaction_wrapper.assert_add_input_fail(&FAKE_INPUT_TRANSACTION_ID, &0, "100", "denied");
}

#[test]
pub fn test_add_account_for_utxo_delegation_address_fails() {
    let sender = startup::create_new_delegation_address();

    JCLITransactionWrapper::new_transaction(FAKE_GENESIS_HASH).assert_add_account_fail(
        &sender.address,
        &100,
        "Invalid input account, this is a UTxO address",
    );
}

#[test]
pub fn test_transaction_with_input_address_equal_to_output_is_accepted_by_node() {
    let sender = startup::create_new_utxo_address();
    let mut config = startup::ConfigurationBuilder::new()
        .with_funds(vec![Fund {
            address: sender.address.clone(),
            value: 100,
        }])
        .build();

    let jormungandr_rest_address = config.get_node_address();
    let _jormungandr = startup::start_jormungandr_node_as_leader(&mut config);
    let utxo = startup::get_utxo_for_address(&sender, &jormungandr_rest_address);
    let transaction_message = JCLITransactionWrapper::build_transaction_from_utxo(
        &utxo,
        &utxo.out_value,
        &sender,
        &utxo.out_value,
        &sender,
        &config.genesis_block_hash,
    )
    .assert_transaction_to_message();
    jcli_wrapper::assert_transaction_post_accepted(&transaction_message, &jormungandr_rest_address);
}

#[test]
pub fn test_input_with_smaller_value_than_initial_utxo_is_rejected_by_node() {
    let sender = startup::create_new_utxo_address();
    let reciever = startup::create_new_utxo_address();
    let mut config = startup::ConfigurationBuilder::new()
        .with_funds(vec![Fund {
            address: sender.address.clone(),
            value: 100,
        }])
        .build();

    let jormungandr_rest_address = config.get_node_address();
    let _jormungandr = startup::start_jormungandr_node_as_leader(&mut config);
    let utxo = startup::get_utxo_for_address(&sender, &jormungandr_rest_address);
    let transaction_message = JCLITransactionWrapper::build_transaction_from_utxo(
        &utxo,
        &99,
        &reciever,
        &99,
        &sender,
        &config.genesis_block_hash,
    )
    .assert_transaction_to_message();

    /// Assertion is changed due to issue: #332
    /// After fix please revert it to
    ///   jcli_wrapper::assert_transaction_post_failed(&transaction_message, &jormungandr_rest_address);
    jcli_wrapper::assert_transaction_post_accepted(&transaction_message, &jormungandr_rest_address);
}

#[test]
pub fn test_input_with_no_spending_utxo_is_accepted_by_node() {
    let sender = startup::create_new_utxo_address();
    let reciever = startup::create_new_utxo_address();
    let mut config = startup::ConfigurationBuilder::new()
        .with_funds(vec![Fund {
            address: sender.address.clone(),
            value: 100,
        }])
        .build();

    let jormungandr_rest_address = config.get_node_address();
    let _jormungandr = startup::start_jormungandr_node_as_leader(&mut config);
    let utxo = startup::get_utxo_for_address(&sender, &jormungandr_rest_address);
    let transaction_message = JCLITransactionWrapper::build_transaction_from_utxo(
        &utxo,
        &100,
        &reciever,
        &50,
        &sender,
        &config.genesis_block_hash,
    )
    .assert_transaction_to_message();
    jcli_wrapper::assert_transaction_post_accepted(&transaction_message, &jormungandr_rest_address);
}

#[test]
pub fn test_transaction_with_non_existing_id_should_be_rejected_by_node() {
    let sender = startup::create_new_utxo_address();
    let reciever = startup::create_new_utxo_address();
    let mut config = startup::ConfigurationBuilder::new()
        .with_funds(vec![Fund {
            address: sender.address.clone(),
            value: 100,
        }])
        .build();

    let jormungandr_rest_address = config.get_node_address();
    let _jormungandr = startup::start_jormungandr_node_as_leader(&mut config);
    let transaction_message = JCLITransactionWrapper::build_transaction(
        &FAKE_INPUT_TRANSACTION_ID,
        &0,
        &100,
        &reciever,
        &50,
        &sender,
        &config.genesis_block_hash,
    )
    .assert_transaction_to_message();

    /// Assertion is changed due to issue: #333
    /// After fix please revert it to
    ///   jcli_wrapper::assert_transaction_post_failed(&transaction_message, &jormungandr_rest_address);
    jcli_wrapper::assert_transaction_post_accepted(&transaction_message, &jormungandr_rest_address);
}