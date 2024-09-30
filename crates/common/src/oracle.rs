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

use alloy_primitives::keccak256;
use anyhow::bail;
use async_trait::async_trait;
use bytemuck::Pod;
use kona_preimage::{HintWriterClient, PreimageKey, PreimageKeyType, PreimageOracleClient};
use kona_primitives::IndexedBlobHash;
use lazy_static::lazy_static;
use op_alloy_protocol::BlockInfo;
use risc0_zkvm::guest::env::{syscall, FdReader, FdWriter};
use risc0_zkvm::sha::{Impl as SHA2, Sha256};
use risc0_zkvm_platform::syscall::{Return, SyscallName};
use risc0_zkvm_platform::{align_up, declare_syscall, WORD_SIZE};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::sync::Mutex;

// Declare system calls for IO
declare_syscall!(pub FPVM_GET_PREIMAGE);
declare_syscall!(pub FPVM_WRITE_HINT);
declare_syscall!(pub FPVM_GET_BLOB);

/// Exchanges slices of plain old data with the host, receiving the response in a vector.
pub fn send_slice_recv_vec<T: Pod, U: Pod>(syscall_name: SyscallName, to_host: &[T]) -> Vec<U> {
    let Return(nbytes, _) = syscall(syscall_name, bytemuck::cast_slice(to_host), &mut []);
    let nwords = align_up(nbytes as usize, WORD_SIZE) / WORD_SIZE;
    let mut from_host_buf = vec![0u32; nwords];
    syscall(syscall_name, &[], from_host_buf.as_mut_slice());
    let v2: &[U] = bytemuck::cast_slice(from_host_buf.as_slice());
    v2.iter()
        .copied()
        .take(nbytes as usize / core::mem::size_of::<U>())
        .collect()
}

/// The size of the LRU cache in the oracle.
pub const ORACLE_LRU_SIZE: usize = 1024;

#[derive(Debug, Clone, Copy, Default)]
pub struct RISCZeroOracle;

pub static RISCZERO_ORACLE: RISCZeroOracle = RISCZeroOracle;

pub fn validate_preimage(key: &PreimageKey, value: &[u8]) -> anyhow::Result<()> {
    let key_type = key.key_type();
    let image = match key_type {
        PreimageKeyType::Keccak256 => keccak256(value).0,
        PreimageKeyType::Sha256 => {
            let x = SHA2::hash_bytes(value);
            x.as_bytes().try_into().unwrap()
        }
        PreimageKeyType::Blob => {
            // kzg validation done inside blob provider
            return Ok(());
        }
        _ => return Ok(()),
    };
    if key != &PreimageKey::new(image, key_type) {
        bail!("Invalid preimage provided for key: {:?}", key);
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RISCZeroPOSIXOracle;

pub static RISCZERO_POSIX_ORACLE: RISCZeroPOSIXOracle = RISCZeroPOSIXOracle;

lazy_static! {
    pub static ref RISCZERO_POSIX_ORACLE_READER: Mutex<FdReader> = Mutex::new(FdReader::new(100));
    pub static ref RISCZERO_POSIX_ORACLE_WRITER: Mutex<FdWriter<fn(&[u8])>> =
        Mutex::new(FdWriter::new(101, |_| {}));
}

#[async_trait]
impl PreimageOracleClient for RISCZeroPOSIXOracle {
    async fn get(&self, key: PreimageKey) -> anyhow::Result<Vec<u8>> {
        let key_bytes: [u8; 32] = key.into();
        RISCZERO_POSIX_ORACLE_WRITER
            .lock()
            .unwrap()
            .write(&key_bytes)?;

        let mut response = Vec::<u8>::new();
        RISCZERO_POSIX_ORACLE_READER
            .lock()
            .unwrap()
            .read_to_end(&mut response)?;

        validate_preimage(&key, &response)?;

        Ok(response)
    }

    async fn get_exact(&self, key: PreimageKey, buf: &mut [u8]) -> anyhow::Result<()> {
        let key_bytes: [u8; 32] = key.into();
        RISCZERO_POSIX_ORACLE_WRITER
            .lock()
            .unwrap()
            .write(&key_bytes)?;

        RISCZERO_POSIX_ORACLE_READER
            .lock()
            .unwrap()
            .read_exact(buf)?;

        validate_preimage(&key, buf)?;

        Ok(())
    }
}

#[async_trait]
impl PreimageOracleClient for RISCZeroOracle {
    async fn get(&self, key: PreimageKey) -> anyhow::Result<Vec<u8>> {
        let key_bytes: [u8; 32] = key.into();
        let preimage_vec: Vec<u8> = send_slice_recv_vec(FPVM_GET_PREIMAGE, key_bytes.as_slice());
        let preimage_bytes = preimage_vec.as_slice();

        validate_preimage(&key, preimage_bytes)?;

        Ok(preimage_vec)
    }

    async fn get_exact(&self, key: PreimageKey, buf: &mut [u8]) -> anyhow::Result<()> {
        let key_bytes: [u8; 32] = key.into();
        let preimage_vec: Vec<u8> = send_slice_recv_vec(FPVM_GET_PREIMAGE, key_bytes.as_slice());
        let preimage_bytes = preimage_vec.as_slice();

        buf.copy_from_slice(preimage_bytes);

        validate_preimage(&key, preimage_bytes)?;

        Ok(())
    }
}

// #[async_trait]
// impl HintWriterClient for RISCZeroPOSIXOracle {
//     async fn write(&self, hint: &str) -> anyhow::Result<()> {
//         // Form the hint into a byte buffer. The format is a 4-byte big-endian length prefix
//         // followed by the hint string.
//         let mut hint_bytes = vec![0u8; hint.len() + 4];
//         hint_bytes[0..4].copy_from_slice(u32::to_be_bytes(hint.len() as u32).as_ref());
//         hint_bytes[4..].copy_from_slice(hint.as_bytes());
//
//     }
// }

#[async_trait]
impl HintWriterClient for RISCZeroOracle {
    async fn write(&self, hint: &str) -> anyhow::Result<()> {
        // Form the hint into a byte buffer. The format is a 4-byte big-endian length prefix
        // followed by the hint string.
        let mut hint_bytes = vec![0u8; hint.len() + 4];
        hint_bytes[0..4].copy_from_slice(u32::to_be_bytes(hint.len() as u32).as_ref());
        hint_bytes[4..].copy_from_slice(hint.as_bytes());

        // let hint_ack: &[u8] = send_recv_slice(FPVM_WRITE_HINT, hint_bytes.as_slice());
        let hint_ack: Vec<u8> = send_slice_recv_vec(FPVM_WRITE_HINT, hint_bytes.as_slice());

        if hint_ack.is_empty() {
            bail!("Did not receive hint acknowledgement from host");
        }

        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BlobFetchRequest {
    pub block_ref: BlockInfo,
    pub blob_hash: IndexedBlobHash,
}
