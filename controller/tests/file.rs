// Copyright 2018 The Grin Developers
// Modifications Copyright 2019 The Gotts Developers
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Test a wallet file send/recieve
#[macro_use]
extern crate log;
extern crate gotts_wallet_controller as wallet;
extern crate gotts_wallet_impls as impls;

use gotts_wallet_util::gotts_core as core;
use gotts_wallet_util::gotts_keychain as keychain;
use gotts_wallet_util::gotts_util as util;

use self::core::global;
use self::core::global::ChainTypes;
use self::keychain::ExtKeychain;
use gotts_wallet_libwallet as libwallet;
use impls::test_framework::{self, LocalWalletClient, WalletProxy};
use impls::FileWalletCommAdapter;
use std::fs;
use std::thread;
use std::time::Duration;

use gotts_wallet_libwallet::InitTxArgs;

use serde_json;

fn clean_output_dir(test_dir: &str) {
	let _ = fs::remove_dir_all(test_dir);
}

fn setup(test_dir: &str) {
	util::init_test_logger();
	clean_output_dir(test_dir);
	global::set_mining_mode(ChainTypes::AutomatedTesting);
}

/// self send impl
fn file_exchange_test_impl(test_dir: &str) -> Result<(), libwallet::Error> {
	setup(test_dir);
	// Create a new proxy to simulate server and wallet responses
	let mut wallet_proxy: WalletProxy<LocalWalletClient, ExtKeychain> = WalletProxy::new(test_dir);
	let chain = wallet_proxy.chain.clone();

	let client1 = LocalWalletClient::new("wallet1", wallet_proxy.tx.clone());
	let wallet1 =
		test_framework::create_wallet(&format!("{}/wallet1", test_dir), client1.clone(), None);
	wallet_proxy.add_wallet("wallet1", client1.get_send_instance(), wallet1.clone());

	let client2 = LocalWalletClient::new("wallet2", wallet_proxy.tx.clone());
	let wallet2 =
		test_framework::create_wallet(&format!("{}/wallet2", test_dir), client2.clone(), None);
	wallet_proxy.add_wallet("wallet2", client2.get_send_instance(), wallet2.clone());

	// Set the wallet proxy listener running
	thread::spawn(move || {
		if let Err(e) = wallet_proxy.run() {
			error!("Wallet Proxy error: {}", e);
		}
	});

	// few values to keep things shorter
	let reward = core::consensus::REWARD;

	// add some accounts
	wallet::controller::owner_single_use(wallet1.clone(), |api| {
		api.create_account_path("mining")?;
		api.create_account_path("listener")?;
		Ok(())
	})?;

	// add some accounts
	wallet::controller::owner_single_use(wallet2.clone(), |api| {
		api.create_account_path("account1")?;
		api.create_account_path("account2")?;
		Ok(())
	})?;

	// Get some mining done
	{
		let mut w = wallet1.lock();
		w.set_parent_key_id_by_name("mining")?;
	}
	let mut bh = 10u64;
	let _ = test_framework::award_blocks_to_wallet(&chain, wallet1.clone(), bh as usize, false);

	let send_file = format!("{}/part_tx_1.tx", test_dir);
	let receive_file = format!("{}/part_tx_2.tx", test_dir);

	// test optional message
	let message = "sender test message, sender test message";

	// Should have 5 in account1 (5 spendable), 5 in account (2 spendable)
	wallet::controller::owner_single_use(wallet1.clone(), |api| {
		let (wallet1_refreshed, wallet1_info) = api.retrieve_summary_info(true, 1)?;
		assert!(wallet1_refreshed);
		assert_eq!(wallet1_info.last_confirmed_height, bh);
		assert_eq!(wallet1_info.total, bh * reward);
		// send to send
		let args = InitTxArgs {
			src_acct_name: Some("mining".to_owned()),
			amount: reward * 2,
			minimum_confirmations: 2,
			max_outputs: 500,
			num_change_outputs: 1,
			selection_strategy: "all".to_owned(),
			message: Some(message.to_owned()),
			..Default::default()
		};
		let mut slate = api.init_send_tx(args)?;
		// output tx file
		let file_adapter = FileWalletCommAdapter::new();
		file_adapter.send_tx_async(&send_file, &mut slate)?;
		api.tx_lock_outputs(&slate, 0)?;
		Ok(())
	})?;

	// Get some mining done
	{
		let mut w = wallet2.lock();
		w.set_parent_key_id_by_name("account1")?;
	}

	let adapter = FileWalletCommAdapter::new();
	let mut slate = adapter.receive_tx_async(&send_file)?;
	let mut naughty_slate = slate.clone();
	naughty_slate.participant_data[0].message = Some("I changed the message".to_owned());

	// verify messages on slate match
	wallet::controller::owner_single_use(wallet1.clone(), |api| {
		api.verify_slate_messages(&slate)?;
		assert!(api.verify_slate_messages(&naughty_slate).is_err());
		Ok(())
	})?;

	let sender2_message = "And this is sender 2's message".to_owned();

	// wallet 2 receives file, completes, sends file back
	wallet::controller::foreign_single_use(wallet2.clone(), |api| {
		slate = api.receive_tx(&slate, None, Some(sender2_message.clone()))?;
		adapter.send_tx_async(&receive_file, &mut slate)?;
		Ok(())
	})?;

	// wallet 1 finalises and posts
	wallet::controller::owner_single_use(wallet1.clone(), |api| {
		let adapter = FileWalletCommAdapter::new();
		let mut slate = adapter.receive_tx_async(&receive_file)?;
		api.verify_slate_messages(&slate)?;
		slate = api.finalize_tx(&slate)?;
		api.post_tx(Some(slate.id), &slate.tx, false)?;
		bh += 1;
		Ok(())
	})?;

	let _ = test_framework::award_blocks_to_wallet(&chain, wallet1.clone(), 3, false);
	bh += 3;

	// Check total in mining account
	wallet::controller::owner_single_use(wallet1.clone(), |api| {
		let (wallet1_refreshed, wallet1_info) = api.retrieve_summary_info(true, 1)?;
		assert!(wallet1_refreshed);
		assert_eq!(wallet1_info.last_confirmed_height, bh);
		assert_eq!(wallet1_info.total, bh * reward - reward * 2);
		Ok(())
	})?;

	// Check total in 'wallet 2' account
	wallet::controller::owner_single_use(wallet2.clone(), |api| {
		let (wallet2_refreshed, wallet2_info) = api.retrieve_summary_info(true, 1)?;
		assert!(wallet2_refreshed);
		assert_eq!(wallet2_info.last_confirmed_height, bh);
		assert_eq!(wallet2_info.total, 2 * reward);
		Ok(())
	})?;

	// Check messages, all participants should have both
	wallet::controller::owner_single_use(wallet1.clone(), |api| {
		let (_, tx) = api.retrieve_txs(true, None, Some(slate.id))?;
		assert_eq!(
			tx[0].clone().messages.unwrap().messages[0].message,
			Some(message.to_owned())
		);
		assert_eq!(
			tx[0].clone().messages.unwrap().messages[1].message,
			Some(sender2_message.to_owned())
		);

		let msg_json = serde_json::to_string_pretty(&tx[0].clone().messages.unwrap()).unwrap();
		println!("{}", msg_json);
		Ok(())
	})?;

	wallet::controller::owner_single_use(wallet2.clone(), |api| {
		let (_, tx) = api.retrieve_txs(true, None, Some(slate.id))?;
		assert_eq!(
			tx[0].clone().messages.unwrap().messages[0].message,
			Some(message.to_owned())
		);
		assert_eq!(
			tx[0].clone().messages.unwrap().messages[1].message,
			Some(sender2_message)
		);
		Ok(())
	})?;

	// let logging finish
	thread::sleep(Duration::from_millis(200));
	Ok(())
}

#[test]
fn wallet_file_exchange() {
	let test_dir = "test_output/file_exchange";
	if let Err(e) = file_exchange_test_impl(test_dir) {
		panic!("Libwallet Error: {} - {}", e, e.backtrace().unwrap());
	}
}
