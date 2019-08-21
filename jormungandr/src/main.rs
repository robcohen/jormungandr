extern crate actix_net;
extern crate actix_web;
extern crate bincode;
extern crate bytes;
extern crate chain_core;
extern crate chain_crypto;
extern crate chain_impl_mockchain;
extern crate chain_storage;
extern crate chain_storage_sqlite;
extern crate chain_time;
#[macro_use]
extern crate custom_error;
#[macro_use]
extern crate error_chain;
#[macro_use(try_ready)]
extern crate futures;
extern crate http;
extern crate humantime;
extern crate hyper;
extern crate jormungandr_lib;
#[macro_use]
extern crate lazy_static;
extern crate native_tls;
extern crate network_core;
extern crate network_grpc;
extern crate poldercast;
extern crate rand;
extern crate serde;
#[macro_use]
extern crate serde_derive;
#[macro_use]
extern crate serde_json;
extern crate serde_yaml;
#[macro_use]
extern crate slog;
extern crate slog_async;
#[cfg(feature = "gelf")]
extern crate slog_gelf;
#[cfg(feature = "systemd")]
extern crate slog_journald;
extern crate slog_json;
#[cfg(unix)]
extern crate slog_syslog;
extern crate slog_term;
extern crate structopt;
extern crate tokio;

use crate::{
    blockcfg::{HeaderHash, Leader},
    blockchain::{Blockchain, CandidateRepo},
    secure::enclave::Enclave,
    settings::start::Settings,
    utils::{async_msg, task::Services},
};
use futures::Future;
use settings::{start::RawSettings, CommandLine};
use slog::Logger;
use std::thread;
use std::time::Duration;
use tokio::sync::lock::Lock;

pub mod blockcfg;
pub mod blockchain;
pub mod client;
pub mod fragment;
pub mod intercom;
pub mod leadership;
pub mod log;
pub mod network;
pub mod rest;
pub mod secure;
pub mod settings;
pub mod start_up;
pub mod state;
mod stats_counter;
pub mod utils;

use stats_counter::StatsCounter;

fn start() -> Result<(), start_up::Error> {
    let initialized_node = initialize_node()?;

    let bootstrapped_node = bootstrap(initialized_node)?;

    start_services(bootstrapped_node)
}

pub struct BootstrappedNode {
    settings: Settings,
    blockchain: Blockchain,
    blockchain_tip: blockchain::Branch,
    block0_hash: HeaderHash,
    new_epoch_announcements: tokio::sync::mpsc::Sender<self::leadership::NewEpochToSchedule>,
    new_epoch_notifier: tokio::sync::mpsc::Receiver<self::leadership::NewEpochToSchedule>,
    logger: Logger,
}

const FRAGMENT_TASK_QUEUE_LEN: usize = 1024;
const NETWORK_TASK_QUEUE_LEN: usize = 32;

fn start_services(bootstrapped_node: BootstrappedNode) -> Result<(), start_up::Error> {
    let mut services = Services::new(bootstrapped_node.logger.clone());

    // initialize the network propagation channel
    let (mut network_msgbox, network_queue) = async_msg::channel(NETWORK_TASK_QUEUE_LEN);
    let (fragment_msgbox, fragment_queue) = async_msg::channel(FRAGMENT_TASK_QUEUE_LEN);
    let mut new_epoch_announcements = bootstrapped_node.new_epoch_announcements;
    let new_epoch_notifier = bootstrapped_node.new_epoch_notifier;
    let blockchain_tip = bootstrapped_node.blockchain_tip;
    let blockchain = bootstrapped_node.blockchain;
    let leadership_logs =
        leadership::Logs::new(bootstrapped_node.settings.leadership.log_ttl.into());
    let leadership_garbage_collection_interval =
        bootstrapped_node.settings.leadership.log_ttl.into();

    let stats_counter = StatsCounter::default();

    let (fragment_pool, pool_logs) = {
        let stats_counter = stats_counter.clone();
        let process = fragment::Process::new(
            bootstrapped_node.settings.mempool.fragment_ttl.into(),
            bootstrapped_node.settings.mempool.log_ttl.into(),
            bootstrapped_node
                .settings
                .mempool
                .garbage_collection_interval
                .into(),
        );

        let pool = process.pool().clone();
        let logs = process.logs().clone();

        services.spawn_future("fragment", move |info| {
            process.start(info, stats_counter, fragment_queue)
        });
        (pool, logs)
    };

    let block_task = {
        let mut blockchain = blockchain.clone();
        let mut blockchain_tip = blockchain_tip.clone();
        // TODO: we should get this value from the configuration
        let block_cache_ttl: Duration = Duration::from_secs(3600);
        let candidate_repo = CandidateRepo::new(blockchain.storage().clone(), block_cache_ttl);
        let stats_counter = stats_counter.clone();
        services.spawn_future_with_inputs("block", move |info, input| {
            blockchain::handle_input(
                info,
                &mut blockchain,
                &mut blockchain_tip,
                &candidate_repo,
                &stats_counter,
                &mut new_epoch_announcements,
                &mut network_msgbox,
                input,
            )
        })
    };

    let client_task = {
        let storage = blockchain.storage().clone();
        let blockchain_tip = blockchain_tip.clone();

        services.spawn_with_inputs("client-query", move |info, input| {
            client::handle_input(info, &storage, &blockchain_tip, input)
        })
    };

    {
        let client_msgbox = client_task.clone();
        let fragment_msgbox = fragment_msgbox.clone();
        let block_msgbox = block_task.clone();
        let block0_hash = bootstrapped_node.block0_hash;
        let config = bootstrapped_node.settings.network.clone();
        let channels = network::Channels {
            client_box: client_msgbox,
            transaction_box: fragment_msgbox,
            block_box: block_msgbox,
        };

        services.spawn("network", move |info| {
            let params = network::TaskParams {
                config,
                block0_hash,
                input: network_queue,
                channels,
                logger: info.into_logger(),
            };
            network::run(params);
        });
    }

    let leader_secrets: Result<Vec<Leader>, start_up::Error> = bootstrapped_node
        .settings
        .secrets
        .iter()
        .map(|secret_path| {
            let secret = secure::NodeSecret::load_from_file(secret_path.as_path())?;
            Ok(Leader {
                bft_leader: secret.bft(),
                genesis_leader: secret.genesis(),
            })
        })
        .collect();
    let leader_secrets = leader_secrets?;
    let enclave = Enclave::from_vec(leader_secrets);

    {
        let leadership_logs = leadership_logs.clone();
        let fragment_pool = fragment_pool.clone();
        let block_task = block_task.clone();
        let blockchain_tip = blockchain_tip.clone();
        let enclave = leadership::Enclave::new(enclave.clone());

        services.spawn_future("leadership", move |info| {
            leadership::LeadershipModule::start(
                info,
                leadership_logs,
                leadership_garbage_collection_interval,
                enclave,
                fragment_pool,
                blockchain_tip,
                new_epoch_notifier,
                block_task,
            )
            .map_err(|e| unimplemented!("error in leadership {}", e))
        });
    }

    let rest_server = match bootstrapped_node.settings.rest {
        Some(rest) => {
            let context = rest::Context {
                stats_counter,
                blockchain,
                blockchain_tip,
                transaction_task: fragment_msgbox,
                logs: pool_logs,
                leadership_logs,
                server: Lock::new(None),
                enclave,
            };
            Some(rest::start_rest_server(&rest, context)?)
        }
        None => None,
    };

    match rest_server {
        Some(server) => server.wait_for_stop(),
        None => thread::sleep(Duration::from_secs(u64::max_value())),
    }
    info!(bootstrapped_node.logger, "Shutting down node");

    Ok(())
}

/// # Bootstrap phase
///
/// done at every startup: we need to bootstrap from whatever local state (including nothing)
/// to the latest network state (or close to latest). until this happen, we don't participate in the network
/// (no block creation) and our network connection(s) is only use to download data.
///
/// Various aspects to do, similar to hermes:
/// * download all the existing blocks
/// * verify all the downloaded blocks
/// * network / peer discoveries (?)
///
///
fn bootstrap(initialized_node: InitializedNode) -> Result<BootstrappedNode, start_up::Error> {
    let InitializedNode {
        settings,
        block0,
        storage,
        logger,
    } = initialized_node;
    let bootstrap_logger = logger.new(o!(log::KEY_TASK => "bootstrap"));

    let (new_epoch_announcements, new_epoch_notifier) = tokio::sync::mpsc::channel(100);

    let block0_hash = block0.header.hash();

    // TODO: we should get this value from the configuration
    let block_cache_ttl: Duration = Duration::from_secs(5 * 24 * 3600);

    let (blockchain, blockchain_tip) = start_up::load_blockchain(
        block0,
        storage,
        new_epoch_announcements.clone(),
        block_cache_ttl,
    )?;

    network::bootstrap(
        &settings.network,
        blockchain.clone(),
        blockchain_tip.clone(),
        &bootstrap_logger,
    )?;

    Ok(BootstrappedNode {
        settings,
        block0_hash,
        blockchain,
        blockchain_tip,
        new_epoch_announcements,
        new_epoch_notifier,
        logger,
    })
}

pub struct InitializedNode {
    pub settings: Settings,
    pub block0: blockcfg::Block,
    pub storage: start_up::NodeStorage,
    pub logger: Logger,
}

fn initialize_node() -> Result<InitializedNode, start_up::Error> {
    let command_line = CommandLine::load();
    let raw_settings = RawSettings::load(command_line)?;
    let logger = raw_settings.to_logger()?;

    let init_logger = logger.new(o!(log::KEY_TASK => "init"));
    info!(
        init_logger,
        "Starting {} {}",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION")
    );
    let settings = raw_settings.try_into_settings(&init_logger)?;
    let storage = start_up::prepare_storage(&settings, &init_logger)?;

    // TODO: load network module here too (if needed)

    let block0 = start_up::prepare_block_0(
        &settings,
        &storage,
        &init_logger, /* add network to fetch block0 */
    )?;

    Ok(InitializedNode {
        settings,
        block0,
        storage,
        logger,
    })
}

fn main() {
    use std::error::Error;

    if let Err(error) = start() {
        eprintln!("{}", error);
        if let Some(source) = error.source() {
            eprintln!("{}", source);
        }

        // TODO: https://github.com/rust-lang/rust/issues/43301
        //
        // as soon as #43301 is stabilized it would be nice to no use
        // `exit` but the more appropriate:
        // https://doc.rust-lang.org/stable/std/process/trait.Termination.html
        std::process::exit(error.code());
    }
}
