// Copyright 2024 RISC Zero, Inc.
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

pub mod oracle;
pub mod witness;

use crate::witness::{BlobWitnessProvider, OracleWitnessProvider};
use alloy_primitives::{keccak256, B256};
use anyhow::Context;
use clap::Parser;
use kailua_build::{KAILUA_FPVM_ELF, KAILUA_FPVM_ID};
use kailua_common::blobs::BlobWitnessData;
use kailua_common::journal::ProofJournal;
use kailua_common::oracle::OracleWitnessData;
use kailua_common::witness::Witness;
use kona_preimage::{HintWriterClient, PreimageOracleClient};
use kona_proof::l1::OracleBlobProvider;
use kona_proof::{BootInfo, CachingOracle};
use risc0_zkvm::{default_prover, ExecutorEnv, ProveInfo, ProverOpts};
use serde::Serialize;
use std::fmt::Debug;
use std::ops::DerefMut;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tokio::join;
use tokio::task::spawn_blocking;
use tracing::info;

/// The size of the LRU cache in the oracle.
pub const ORACLE_LRU_SIZE: usize = 1024;

/// The client binary CLI application arguments.
#[derive(Parser, Serialize, Clone, Debug)]
pub struct KailuaClientCli {
    #[arg(long, action = clap::ArgAction::Count, env)]
    pub kailua_verbosity: u8,

    #[clap(long, value_parser = parse_b256, env)]
    pub precondition_validation_data_hash: Option<B256>,
}

pub fn parse_b256(s: &str) -> Result<B256, String> {
    B256::from_str(s).map_err(|_| format!("Invalid B256 value: {}", s))
}

pub async fn run_client<P, H>(
    oracle_client: P,
    hint_client: H,
    precondition_validation_data_hash: B256,
) -> anyhow::Result<()>
where
    P: PreimageOracleClient + Send + Sync + Debug + Clone + 'static,
    H: HintWriterClient + Send + Sync + Debug + Clone + 'static,
{
    // preload all data natively
    info!("Running native client.");
    let witness = run_native_client(
        oracle_client.clone(),
        hint_client.clone(),
        precondition_validation_data_hash,
    )
    .await
    .expect("Failed to run native client.");
    // compute the receipt in the zkvm
    info!("Running zk client.");
    let prove_info = run_zk_client(witness)
        .await
        .expect("Failed to run zk client.");
    // Prepare receipt file
    let proof_journal = ProofJournal::decode_packed(prove_info.receipt.journal.as_ref())
        .expect("Failed to decode receipt output");
    let mut output_file = File::create(fpvm_proof_file_name(
        proof_journal.precondition_output,
        proof_journal.l1_head,
        proof_journal.claimed_l2_output_root,
        proof_journal.claimed_l2_block_number,
        proof_journal.agreed_l2_output_root,
    ))
    .await
    .expect("Failed to create receipt output file");
    // Write receipt data to file
    let receipt_bytes =
        bincode::serialize(&prove_info.receipt).expect("Could not serialize receipt.");
    output_file
        .write_all(receipt_bytes.as_slice())
        .await
        .expect("Failed to write receipt to file");
    output_file
        .flush()
        .await
        .expect("Failed to flush receipt output file data.");

    Ok(())
}

pub async fn run_native_client<P, H>(
    oracle_client: P,
    hint_client: H,
    precondition_validation_data_hash: B256,
) -> anyhow::Result<Witness>
where
    P: PreimageOracleClient + Send + Sync + Debug + Clone,
    H: HintWriterClient + Send + Sync + Debug + Clone,
{
    let oracle_witness = Arc::new(Mutex::new(OracleWitnessData::default()));
    let blobs_witness = Arc::new(Mutex::new(BlobWitnessData::default()));
    info!("Preamble");
    let oracle = Arc::new(OracleWitnessProvider {
        oracle: CachingOracle::new(ORACLE_LRU_SIZE, oracle_client, hint_client),
        witness: oracle_witness.clone(),
    });
    let boot = Arc::new(
        BootInfo::load(oracle.as_ref())
            .await
            .context("BootInfo::load")?,
    );
    let beacon = BlobWitnessProvider {
        provider: OracleBlobProvider::new(oracle.clone()),
        witness: blobs_witness.clone(),
    };
    // Run client
    let (_, real_output_hash) = kailua_common::client::run_client(
        precondition_validation_data_hash,
        oracle,
        boot.clone(),
        beacon,
    )?;
    // Check output
    if let Some(computed_output) = real_output_hash {
        // With sufficient data, the input l2_claim must be true
        assert_eq!(boot.claimed_l2_output_root, computed_output);
    } else {
        // We use the zero claim hash to denote that the data as of l1 head is insufficient
        assert_eq!(boot.claimed_l2_output_root, B256::ZERO);
    }
    let witness = Witness {
        oracle_witness: core::mem::take(oracle_witness.lock().unwrap().deref_mut()),
        blobs_witness: core::mem::take(blobs_witness.lock().unwrap().deref_mut()),
        precondition_validation_data_hash,
    };
    Ok(witness)
}

pub async fn run_zk_client(witness: Witness) -> anyhow::Result<ProveInfo> {
    let client_task = spawn_blocking(move || {
        // Execution environment
        let env = ExecutorEnv::builder()
            // Pass in witness data
            .write(&witness)?
            .build()?;
        let prover = default_prover();
        let prove_info = prover
            .prove_with_opts(env, KAILUA_FPVM_ELF, &ProverOpts::groth16())
            .context("prove_with_opts")?;
        println!(
            "Proof of {} total cycles ({} user cycles) computed.",
            prove_info.stats.total_cycles, prove_info.stats.user_cycles
        );
        prove_info
            .receipt
            .verify(KAILUA_FPVM_ID)
            .context("receipt verification")?;
        println!("Receipt verified.");
        Ok::<_, anyhow::Error>(prove_info)
    });
    join!(client_task).0?
}

pub fn fpvm_proof_file_name(
    precondition_output: B256,
    l1_head: B256,
    claimed_l2_output_root: B256,
    claimed_l2_block_number: u64,
    agreed_l2_output_root: B256,
) -> String {
    let version = risc0_zkvm::get_version().unwrap();
    let suffix = if risc0_zkvm::is_dev_mode() {
        "fake"
    } else {
        "zkp"
    };
    let claimed_l2_block_number = claimed_l2_block_number.to_be_bytes();
    let data = [
        bytemuck::cast::<_, [u8; 32]>(KAILUA_FPVM_ID).as_slice(),
        precondition_output.as_slice(),
        l1_head.as_slice(),
        claimed_l2_output_root.as_slice(),
        claimed_l2_block_number.as_slice(),
        agreed_l2_output_root.as_slice(),
    ]
    .concat();
    let file_name = keccak256(data);
    format!("risc0-{version}-{file_name}.{suffix}")
}
