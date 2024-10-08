mod busses;
mod open;
mod utils;
use colored::*;
use drillx::{
    equix::{self},
    Hash, Solution,
};
use ore_api::{
    consts::{BUS_ADDRESSES, BUS_COUNT, EPOCH_DURATION},
    state::{Config, Proof},
};
use rand::Rng;
use reqwest::Client;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_program::pubkey::Pubkey;
use solana_rpc_client::spinner;
use std::{time::Duration, env};
use std::{sync::Arc, time::Instant};

use crate::open::open;
use crate::utils::{amount_u64_to_string, get_clock, get_config, get_updated_proof_with_authority};

//Default is Alvarium Mining Pool. You can replace with a different mining pool address
pub const MINING_POOL: Pubkey = solana_program::pubkey!("Cdh9QF6NmxCxWDEmuusFVkhQSZuVMRXj9nnZQyGraCna");

//Default is Alvarium Mining Pool. Change this to your pool's API endpoint
pub const MINING_POOL_URL: &str = "https://alvarium.bifrost.technology/submitwork";

#[tokio::main]
async fn main() {
    let mut rng = rand::thread_rng();
    let mut miner_rpc: String = String::new();
    let mut miner_address: Pubkey = solana_program::pubkey!("5vfk9nHusMZySCCytwDUEvS2u7rZdm6de2nukCpNTF4D");
    let random_depth = rng.gen_range(50..=500);
    let mut threads: u64 = 96;
    let mut buffer: u64 = 50;
    let args: Vec<String> = env::args().collect();
        
    if args.len() > 1 {
        if let Ok(value) = args[1].parse::<String>() {
            miner_rpc = value;
        }
    }
    if args.len() > 2 {
        if let Ok(value) = args[2].parse() {
            miner_address = value;
        }
    }
    if args.len() > 3 {
        if let Ok(value) = args[3].parse::<u64>() {
            threads = value;
        }
    }
    if args.len() > 4 {
        if let Ok(value) = args[4].parse::<u64>() {
            buffer = value;
        }else{
            buffer = 5;
        }
    }else{
        buffer = 5;
    }
            

    mine(threads, buffer, random_depth, miner_address, miner_rpc).await;
}

pub async fn mine(_threads: u64, _buffer: u64, _depth: u64, _miner: Pubkey, _rpc: String) {
    let quickrpc: RpcClient = RpcClient::new(_rpc.clone());
    open(&quickrpc).await;
    let mut _previous_challenge: String = String::new();
    let mut _current_challenge: String = String::new();

    loop {
        let rpc_client: RpcClient = RpcClient::new(_rpc.clone());
        let last_hash_at = 0;
        let proof = get_updated_proof_with_authority(&rpc_client, MINING_POOL, last_hash_at).await;

        println!(
            "\n Mining Pool Stake balance: {} ORE",
            amount_u64_to_string(proof.balance)
        );
        _current_challenge = bs58::encode(proof.challenge.as_slice()).into_string();
        println!("Current Challenge: {}", _current_challenge);
        if _current_challenge != _previous_challenge {

            // Calc cutoff time
            let cutoff_time = get_cutoff(&rpc_client, proof, _buffer).await;

            // Run drillx
            let config = get_config(&rpc_client).await;
            let (solution, _best_difficulty, _performance): (Solution, u32, u64) = find_hash_par(
                proof,
                cutoff_time,
                _threads,
                _depth,
                config.min_difficulty as u32,
            )
            .await;

            //Serialize work as a B58 hash
            let workhash: Vec<u8> = [
                solution.d.as_slice(),
                solution.n.as_slice(),
                _miner.to_bytes().as_slice(),
                proof.challenge.as_slice(),
                _best_difficulty.to_le_bytes().as_slice(),
                _performance.to_le_bytes().as_slice()
            ]
            .concat();

            let webclient = Client::new();
            //Send the work to the pool to score your work and get payment
            let _response = webclient
                .post(MINING_POOL_URL)
                .json(&bs58::encode(workhash).into_string())
                .send()
                .await;
            _previous_challenge = _current_challenge;
        } else {
            println!("Waiting for new work...");
            std::thread::sleep(Duration::from_millis(5000));
        }
    }
}


pub async fn find_hash_par(
    proof: Proof,
    cutoff_time: u64,
    threads: u64,
    depth: u64,
    min_difficulty: u32,
) -> (Solution, u32, u64) {
    // Dispatch job to each thread
    let progress_bar = Arc::new(spinner::new_progress_bar());
    progress_bar.set_message("Mining...");
    let handles: Vec<_> = (0..threads)
        .map(|i| {
            std::thread::spawn({
                let proof = proof.clone();
                let progress_bar = progress_bar.clone();
                let mut memory = equix::SolverMemory::new();
                move || {
                    let timer = Instant::now();
                    
                    let mut nonce = u64::MAX.saturating_div(depth).saturating_mul(i);
                    let seed = nonce;
                    let mut best_nonce = nonce;
                    let mut best_difficulty = 0;
                    let mut best_hash = Hash::default();
                    loop {
                        // Create hash
                        if let Ok(hx) = drillx::hash_with_memory(
                            &mut memory,
                            &proof.challenge,
                            &nonce.to_le_bytes(),
                        ) {
                            let difficulty = hx.difficulty();
                            if difficulty.gt(&best_difficulty) {
                                best_nonce = nonce;
                                best_difficulty = difficulty;
                                best_hash = hx;
                            }
                        }

                        // Exit if time has elapsed
                        if nonce % 100 == 0 {
                            if timer.elapsed().as_secs().ge(&cutoff_time) {
                                if best_difficulty.gt(&min_difficulty) {
                                    // Mine until min difficulty has been met
                                    break;
                                }
                            } else if i == 0 {
                                progress_bar.set_message(format!(
                                    "Mining... ({} sec remaining)",
                                    cutoff_time.saturating_sub(timer.elapsed().as_secs()),
                                ));
                            }
                        }

                        // Increment nonce
                        nonce += 1;
                    }

                    // Return the best nonce
                    (best_nonce, best_difficulty, best_hash, (nonce - seed))
                }
            })
        })
        .collect();

    // Join handles and return best nonce 
    let mut total_nonces = 0;
    let mut best_nonce = 0;
    let mut best_difficulty = 0;
    let mut best_hash = Hash::default();
    for h in handles {
        if let Ok((nonce, difficulty, hash, count)) = h.join() {
            if difficulty > best_difficulty {
                best_difficulty = difficulty;
                best_nonce = nonce;
                best_hash = hash;
            }
            total_nonces += count;
        }
    }

    // Update log
    progress_bar.finish_with_message(format!(
        "Best hash: {} (difficulty: {})",
        bs58::encode(best_hash.h).into_string(),
        best_difficulty
    ));
    println!("Hash Power: {} H/s | {} H/m", (total_nonces / 55), total_nonces);
    (
        Solution::new(best_hash.d, best_nonce.to_le_bytes()),
        best_difficulty,
        total_nonces
    )
}
pub struct Minersettings {
    _threads: u64,
    _buffer: u64,
    _depth: u64,
    _miner: Pubkey,
    _rpc: String
}
unsafe impl Send for Minersettings {}


pub fn check_num_cores(threads: u64) {
    // Check num threads
    let num_cores = num_cpus::get() as u64;
    if threads.gt(&num_cores) {
        println!(
            "{} Number of threads ({}) exceeds available cores ({})",
            "WARNING".bold().yellow(),
            threads,
            num_cores
        );
    }
}


pub async fn should_reset(client: &RpcClient, config: Config) -> bool {
    let rpc_client: &RpcClient = client;
    let clock = get_clock(&rpc_client).await;
    config
        .last_reset_at
        .saturating_add(EPOCH_DURATION)
        .saturating_sub(5) // Buffer
        .le(&clock.unix_timestamp)
}


pub async fn get_cutoff(client: &RpcClient, proof: Proof, buffer_time: u64) -> u64 {
    let rpc_client: &RpcClient = client;
    let clock = get_clock(&rpc_client).await;
    proof
        .last_hash_at
        .saturating_add(60)
        .saturating_sub(buffer_time as i64)
        .saturating_sub(clock.unix_timestamp)
        .max(0) as u64
}

// TODO Pick a better strategy (avoid draining bus)
pub fn find_bus() -> Pubkey {
    let i = rand::thread_rng().gen_range(0..BUS_COUNT);
    BUS_ADDRESSES[i]
}
