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

use crate::oracle::{HINT_WRITER, ORACLE_READER};
use alloy_primitives::{keccak256, B256};
use anyhow::Context;
use kailua_build::{KAILUA_FPVM_ELF, KAILUA_FPVM_ID};
use kailua_common::oracle::{
    BlobFetchRequest, FPVM_GET_BLOB, FPVM_GET_PREIMAGE, FPVM_WRITE_HINT, ORACLE_LRU_SIZE,
};
use kona_client::l1::OracleBlobProvider;
use kona_client::{BootInfo, CachingOracle};
use kona_derive::traits::BlobProvider;
use kona_preimage::{HintWriterClient, PreimageOracleClient};
use risc0_zkvm::serde::from_slice;
use risc0_zkvm::{default_prover, ExecutorEnv, ProverOpts, Receipt};
use std::collections::VecDeque;
use std::io::{BufReader, Read, Write};
use std::sync::{Arc, Mutex};
use tokio::join;
use tokio::runtime::Handle;
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::spawn_blocking;
use tracing::info;

pub async fn run_native_client() -> anyhow::Result<Option<B256>> {
    info!("Preamble");
    let oracle = Arc::new(CachingOracle::new(
        ORACLE_LRU_SIZE,
        ORACLE_READER,
        HINT_WRITER,
    ));

    let boot = Arc::new(
        BootInfo::load(oracle.as_ref())
            .await
            .context("BootInfo::load")?,
    );
    let beacon = OracleBlobProvider::new(oracle.clone());
    kailua_common::client::run_client(oracle, boot, beacon)
}

#[derive(Debug, Clone)]
pub struct POSIXPreimageOracleClient<OR: PreimageOracleClient> {
    pub oracle: Arc<OR>,
    pub preimage: Arc<Mutex<Option<BufReader<VecDeque<u8>>>>>,
}

// This receives an image from the zkvm to query for a preimage
impl<OR: PreimageOracleClient> Write for POSIXPreimageOracleClient<OR> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // Fetch the preimage if the key length is correct
        let byte_arr: [u8; 32] = buf
            .try_into()
            .or::<std::io::Error>(Err(std::io::ErrorKind::Interrupted.into()))?;
        let preimage: VecDeque<u8> = Handle::current()
            .block_on(async { self.oracle.get(byte_arr.try_into()?).await })
            .unwrap()
            .into();
        // update the reader
        self.preimage
            .lock()
            .unwrap()
            .replace(BufReader::new(preimage));
        Ok(32)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// This writes a preimage to the zkvm
impl<OR: PreimageOracleClient> Read for POSIXPreimageOracleClient<OR> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self.preimage.lock().unwrap().as_mut() {
            Some(reader) => reader.read(buf),
            _ => Err(std::io::ErrorKind::Interrupted.into()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PPOCHandle<OR: PreimageOracleClient> {
    inner: Arc<Mutex<POSIXPreimageOracleClient<OR>>>,
}

impl<OR: PreimageOracleClient> From<POSIXPreimageOracleClient<OR>> for PPOCHandle<OR> {
    fn from(oracle: POSIXPreimageOracleClient<OR>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(oracle)),
        }
    }
}

impl<OR: PreimageOracleClient> Read for PPOCHandle<OR> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .read(buf)
            .expect("PPOCHandle.read"))
    }
}

impl<OR: PreimageOracleClient> Write for PPOCHandle<OR> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .write(buf)
            .expect("PPOCHandle.write"))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .flush()
            .expect("PPOCHandle.flush"))
    }
}

pub async fn prove_zkvm_client() -> anyhow::Result<Receipt> {
    let client_task = spawn_blocking(|| {
        let oracle = Arc::new(CachingOracle::new(
            ORACLE_LRU_SIZE,
            ORACLE_READER,
            HINT_WRITER,
        ));
        let client = PPOCHandle::from(POSIXPreimageOracleClient {
            oracle: oracle.clone(),
            preimage: Arc::new(Mutex::new(None)),
        });
        let client_reader = BufReader::new(client.clone());
        let blob_provider = Arc::new(AsyncMutex::new(OracleBlobProvider::new(oracle.clone())));
        // todo: posix IO
        let env = ExecutorEnv::builder()
            .read_fd(100, client_reader)
            .write_fd(101, client)
            .io_callback(FPVM_GET_PREIMAGE, |key| {
                let byte_vec = key.to_vec();
                let byte_arr: [u8; 32] = byte_vec.as_slice().try_into()?;
                let res =
                    Handle::current().block_on(async { oracle.get(byte_arr.try_into()?).await })?;
                Ok(res.into())
            })
            .io_callback(FPVM_WRITE_HINT, |hint| {
                let byte_vec = Vec::<u8>::from(&hint.to_vec()[4..]);
                let string = String::from_utf8(byte_vec)?;
                Handle::current().block_on(async { oracle.write(&string).await })?;
                Ok(vec![1u8].into())
            })
            .io_callback(FPVM_GET_BLOB, |request| {
                let data = request.to_vec();
                let request: BlobFetchRequest = from_slice(&data)?;
                let blob = Handle::current()
                    .block_on(async {
                        let mut provider = blob_provider.lock().await;
                        provider
                            .get_blobs(&request.block_ref, &[request.blob_hash])
                            .await
                    })
                    .unwrap();
                // todo: refactor kzg commitment logic as function
                let c_kzg_blob = c_kzg::Blob::from_bytes(blob[0].as_slice())?;
                let settings = alloy::consensus::EnvKzgSettings::default();
                let commitment =
                    c_kzg::KzgCommitment::blob_to_kzg_commitment(&c_kzg_blob, settings.get())
                        .expect("Failed to convert blob to commitment");
                let proof = c_kzg::KzgProof::compute_blob_kzg_proof(
                    &c_kzg_blob,
                    &commitment.to_bytes(),
                    settings.get(),
                )?;
                let result = [blob[0].as_slice(), commitment.as_slice(), proof.as_slice()].concat();
                Ok(result.into())
            })
            .build()?;
        let prover = default_prover();
        let prove_info = prover.prove_with_opts(env, KAILUA_FPVM_ELF, &ProverOpts::groth16())?;
        println!(
            "Proof of {} total cycles ({} user cycles) computed.",
            prove_info.stats.total_cycles, prove_info.stats.user_cycles
        );
        prove_info
            .receipt
            .verify(KAILUA_FPVM_ID)
            .context("receipt verification")?;
        println!("Receipt verified.");
        Ok::<_, anyhow::Error>(prove_info.receipt)
    });
    join!(client_task).0?
}

pub fn fpvm_proof_file_name(l1_head: B256, l2_claim: B256, l2_output_root: B256) -> String {
    let version = risc0_zkvm::get_version().unwrap();
    let suffix = if risc0_zkvm::is_dev_mode() {
        "fake"
    } else {
        "zkp"
    };
    let data = [
        bytemuck::cast::<_, [u8; 32]>(KAILUA_FPVM_ID).as_slice(),
        l1_head.as_slice(),
        l2_output_root.as_slice(),
        l2_claim.as_slice(),
    ]
    .concat();
    let file_name = keccak256(data);
    format!("risc0-{}-{file_name}.{suffix}", version.to_string())
}
