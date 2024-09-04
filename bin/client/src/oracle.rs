// This file is copied with minor modifications from Optimism's Kona Client.

//! Contains the [CachingOracle], which is a wrapper around an [OracleReader] that stores a
//! configurable number of responses in an [LruCache] for quick retrieval.
//!
//! [OracleReader]: kona_preimage::OracleReader

extern crate alloc;

use async_trait::async_trait;
use kona_common::FileDescriptor;
use kona_preimage::{
    HintWriter, HintWriterClient, OracleReader, PipeHandle, PreimageKey, PreimageOracleClient,
};
use lru::LruCache;
use spin::Mutex;
use std::num::NonZeroUsize;
use std::sync::Arc;

/// The global preimage oracle reader pipe.
static ORACLE_READER_PIPE: PipeHandle =
    PipeHandle::new(FileDescriptor::PreimageRead, FileDescriptor::PreimageWrite);
/// The global hint writer pipe.
static HINT_WRITER_PIPE: PipeHandle =
    PipeHandle::new(FileDescriptor::HintRead, FileDescriptor::HintWrite);
/// The global preimage oracle reader.
pub(crate) static ORACLE_READER: OracleReader = OracleReader::new(ORACLE_READER_PIPE);
/// The global hint writer.
pub(crate) static HINT_WRITER: HintWriter = HintWriter::new(HINT_WRITER_PIPE);

/// A wrapper around an [OracleReader] that stores a configurable number of responses in an
/// [LruCache] for quick retrieval.
///
/// [OracleReader]: kona_preimage::OracleReader
#[derive(Debug, Clone)]
pub struct CachingOracle {
    /// The spin-locked cache that stores the responses from the oracle.
    cache: Arc<Mutex<LruCache<PreimageKey, Vec<u8>>>>,
}

impl CachingOracle {
    /// Creates a new [CachingOracle] that wraps the given [OracleReader] and stores up to `N`
    /// responses in the cache.
    ///
    /// [OracleReader]: kona_preimage::OracleReader
    pub fn new(cache_size: usize) -> Self {
        Self {
            cache: Arc::new(Mutex::new(LruCache::new(
                NonZeroUsize::new(cache_size).expect("N must be greater than 0"),
            ))),
        }
    }
}

#[async_trait]
impl PreimageOracleClient for CachingOracle {
    async fn get(&self, key: PreimageKey) -> anyhow::Result<Vec<u8>> {
        let mut cache_lock = self.cache.lock();
        if let Some(value) = cache_lock.get(&key) {
            Ok(value.clone())
        } else {
            let value = ORACLE_READER.get(key).await?;
            cache_lock.put(key, value.clone());
            Ok(value)
        }
    }

    async fn get_exact(&self, key: PreimageKey, buf: &mut [u8]) -> anyhow::Result<()> {
        let mut cache_lock = self.cache.lock();
        if let Some(value) = cache_lock.get(&key) {
            // SAFETY: The value never enters the cache unless the preimage length matches the
            // buffer length, due to the checks in the OracleReader.
            buf.copy_from_slice(value.as_slice());
            Ok(())
        } else {
            ORACLE_READER.get_exact(key, buf).await?;
            cache_lock.put(key, buf.to_vec());
            Ok(())
        }
    }
}

#[async_trait]
impl HintWriterClient for CachingOracle {
    async fn write(&self, hint: &str) -> anyhow::Result<()> {
        HINT_WRITER.write(hint).await
    }
}
