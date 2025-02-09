// Copyright 2020 Ant Group. All rights reserved.
// Copyright (C) 2021 Alibaba Cloud. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::fs;
use std::io::Result;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::runtime::{Builder, Runtime};

use nydus_utils::metrics::BlobcacheMetrics;

use self::cache_entry::FileCacheEntry;
use crate::backend::BlobBackend;
use crate::cache::worker::{AsyncPrefetchConfig, AsyncWorkerMgr};
use crate::cache::{BlobCache, BlobCacheMgr};
use crate::device::BlobInfo;
use crate::factory::CacheConfig;

mod cache_entry;

fn default_work_dir() -> String {
    ".".to_string()
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct BlobCacheConfig {
    #[serde(default = "default_work_dir")]
    work_dir: String,
    #[serde(default)]
    disable_indexed_map: bool,
}

impl BlobCacheConfig {
    fn get_work_dir(&self) -> Result<&str> {
        let path = fs::metadata(&self.work_dir)
            .or_else(|_| {
                fs::create_dir_all(&self.work_dir)?;
                fs::metadata(&self.work_dir)
            })
            .map_err(|e| {
                last_error!(format!(
                    "fail to stat blobcache work_dir {}: {}",
                    self.work_dir, e
                ))
            })?;

        if path.is_dir() {
            Ok(&self.work_dir)
        } else {
            Err(enoent!(format!(
                "blobcache work_dir {} is not a directory",
                self.work_dir
            )))
        }
    }
}

/// An implementation of [BlobCacheMgr](../trait.BlobCacheMgr.html) to improve performance by
/// caching uncompressed blob with local storage.
#[derive(Clone)]
pub struct FileCacheMgr {
    blobs: Arc<RwLock<HashMap<String, Arc<FileCacheEntry>>>>,
    backend: Arc<dyn BlobBackend>,
    metrics: Arc<BlobcacheMetrics>,
    prefetch_config: Arc<AsyncPrefetchConfig>,
    runtime: Arc<Runtime>,
    worker_mgr: Arc<AsyncWorkerMgr>,
    work_dir: String,
    validate: bool,
    disable_indexed_map: bool,
    is_compressed: bool,
}

impl FileCacheMgr {
    /// Create a new instance of `FileCacheMgr`.
    pub fn new(
        config: CacheConfig,
        backend: Arc<dyn BlobBackend>,
        id: &str,
    ) -> Result<FileCacheMgr> {
        let blob_config: BlobCacheConfig =
            serde_json::from_value(config.cache_config).map_err(|e| einval!(e))?;
        let work_dir = blob_config.get_work_dir()?;
        let metrics = BlobcacheMetrics::new(id, work_dir);
        let runtime = Arc::new(
            Builder::new_multi_thread()
                .worker_threads(1) // Limit the number of worker thread to 1 since this runtime is generally used to do blocking IO.
                .thread_keep_alive(Duration::from_secs(10))
                .max_blocking_threads(8)
                .thread_name("cache-flusher")
                .build()
                .map_err(|e| eother!(e))?,
        );
        let prefetch_config: Arc<AsyncPrefetchConfig> = Arc::new(config.prefetch_config.into());
        let worker_mgr = AsyncWorkerMgr::new(metrics.clone(), prefetch_config.clone())?;

        Ok(FileCacheMgr {
            blobs: Arc::new(RwLock::new(HashMap::new())),
            backend,
            metrics,
            prefetch_config,
            runtime,
            worker_mgr: Arc::new(worker_mgr),
            work_dir: work_dir.to_owned(),
            disable_indexed_map: blob_config.disable_indexed_map,
            validate: config.cache_validate,
            is_compressed: config.cache_compressed,
        })
    }

    // Get the file cache entry for the specified blob object.
    fn get(&self, blob: &Arc<BlobInfo>) -> Option<Arc<FileCacheEntry>> {
        self.blobs.read().unwrap().get(blob.blob_id()).cloned()
    }

    // Create a file cache entry for the specified blob object if not present, otherwise
    // return the existing one.
    fn get_or_create_cache_entry(&self, blob: &Arc<BlobInfo>) -> Result<Arc<FileCacheEntry>> {
        if let Some(entry) = self.get(blob) {
            return Ok(entry);
        }

        let entry = FileCacheEntry::new(
            &self,
            blob.clone(),
            self.prefetch_config.clone(),
            self.runtime.clone(),
            self.worker_mgr.clone(),
        )?;
        let entry = Arc::new(entry);
        let mut guard = self.blobs.write().unwrap();
        if let Some(entry) = guard.get(blob.blob_id()) {
            Ok(entry.clone())
        } else {
            guard.insert(blob.blob_id().to_owned(), entry.clone());
            self.metrics
                .underlying_files
                .lock()
                .unwrap()
                .insert(blob.blob_id().to_string());
            Ok(entry)
        }
    }
}

impl BlobCacheMgr for FileCacheMgr {
    fn init(&self) -> Result<()> {
        AsyncWorkerMgr::start(self.worker_mgr.clone())
    }

    fn destroy(&self) {
        self.worker_mgr.stop();
        self.backend().shutdown();
        self.metrics.release().unwrap_or_else(|e| error!("{:?}", e));
    }

    fn gc(&self) {
        let mut reclaim = Vec::new();

        {
            let guard = self.blobs.write().unwrap();
            for (id, entry) in guard.iter() {
                if Arc::strong_count(entry) == 1 {
                    reclaim.push(id.to_owned());
                }
            }
        }

        for key in reclaim.iter() {
            let mut guard = self.blobs.write().unwrap();
            if let Some(entry) = guard.get(key) {
                if Arc::strong_count(entry) > 1 {
                    continue;
                }
            }
            guard.remove(key);
        }
    }

    fn backend(&self) -> &(dyn BlobBackend) {
        self.backend.as_ref()
    }

    fn get_blob_cache(&self, blob_info: &Arc<BlobInfo>) -> Result<Arc<dyn BlobCache>> {
        self.get_or_create_cache_entry(blob_info)
            .map(|v| v as Arc<dyn BlobCache>)
    }
}

#[cfg(test)]
pub mod blob_cache_tests {
    /*
    use std::alloc::{alloc_zeroed, Layout};
    use std::slice::from_raw_parts;
    use std::sync::Arc;

    use vm_memory::{VolatileMemory, VolatileSlice};
    use vmm_sys_util::tempdir::TempDir;

    use crate::backend::{BackendResult, BlobBackend, BlobReader, BlobWrite};
    use crate::cache::{filecache, BlobPrefetchConfig, BlobV5Cache, MergedBackendRequest};
    use crate::compress;
    use crate::device::v5::{BlobIoDesc, BlobV5ChunkInfo};
    use crate::device::{BlobChunkFlags, BlobChunkInfo, BlobInfo};
    use crate::factory::CacheConfig;
    use crate::impl_getter;
    use crate::RAFS_DEFAULT_BLOCK_SIZE;

    use nydus_utils::{
        digest::{self, RafsDigest},
        metrics::BackendMetrics,
    };
    */

    use vmm_sys_util::tempdir::TempDir;
    use vmm_sys_util::tempfile::TempFile;

    use super::*;

    #[test]
    fn test_blob_cache_config() {
        // new blob cache
        let tmp_dir = TempDir::new().unwrap();
        let dir = tmp_dir.as_path().to_path_buf();
        let s = format!(
            r###"
        {{
            "work_dir": {:?}
        }}
        "###,
            dir
        );

        let mut blob_config: BlobCacheConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(blob_config.disable_indexed_map, false);
        assert_eq!(blob_config.work_dir, dir.to_str().unwrap());
        /*
        assert_eq!(blob_config.get_work_dir().unwrap(), dir.to_str().unwrap());

        blob_config.work_dir += "/cache";
        assert_eq!(blob_config.get_work_dir().unwrap(), dir.to_str().unwrap().to_owned() + "/cache");
         */

        let tmp_file = TempFile::new().unwrap();
        let file = tmp_file.as_path().to_path_buf();
        blob_config.work_dir = file.to_str().unwrap().to_owned();
        assert!(blob_config.get_work_dir().is_err());
    }

    /*
       #[test]
       fn test_add() {
           // new blob cache
           let tmp_dir = TempDir::new().unwrap();
           let s = format!(
               r###"
           {{
               "work_dir": {:?}
           }}
           "###,
               tmp_dir.as_path().to_path_buf().join("cache"),
           );

           let cache_config = CacheConfig {
               cache_validate: true,
               cache_compressed: false,
               cache_type: String::from("blobcache"),
               cache_config: serde_json::from_str(&s).unwrap(),
               prefetch_config: BlobPrefetchConfig::default(),
           };
           let blob_cache = filecache::new(
               cache_config,
               Arc::new(MockBackend {
                   metrics: BackendMetrics::new("id", "mock"),
               }) as Arc<dyn BlobBackend + Send + Sync>,
               compress::Algorithm::Lz4Block,
               digest::Algorithm::Blake3,
               "id",
           )
           .unwrap();

           // generate backend data
           let mut expect = vec![1u8; 100];
           let blob_id = "blobcache";
           blob_cache
               .backend
               .read(blob_id, expect.as_mut(), 0)
               .unwrap();

           // generate chunk and bio
           let mut chunk = MockChunkInfo::new();
           chunk.block_id = RafsDigest::from_buf(&expect, digest::Algorithm::Blake3);
           chunk.file_offset = 0;
           chunk.compress_offset = 0;
           chunk.compress_size = 100;
           chunk.decompress_offset = 0;
           chunk.decompress_size = 100;
           let bio = BlobIoDesc::new(
               Arc::new(chunk),
               Arc::new(BlobInfo {
                   chunk_count: 0,
                   readahead_offset: 0,
                   readahead_size: 0,
                   blob_id: blob_id.to_string(),
                   blob_index: 0,
                   blob_decompressed_size: 0,
                   blob_compressed_size: 0,
               }),
               50,
               50,
               RAFS_DEFAULT_BLOCK_SIZE as u32,
               true,
           );

           // read from cache
           let r1 = unsafe {
               let layout = Layout::from_size_align(50, 1).unwrap();
               let ptr = alloc_zeroed(layout);
               let vs = VolatileSlice::new(ptr, 50);
               blob_cache.read(&mut [bio.clone()], &[vs]).unwrap();
               Vec::from(from_raw_parts(ptr, 50))
           };

           let r2 = unsafe {
               let layout = Layout::from_size_align(50, 1).unwrap();
               let ptr = alloc_zeroed(layout);
               let vs = VolatileSlice::new(ptr, 50);
               blob_cache.read(&mut [bio], &[vs]).unwrap();
               Vec::from(from_raw_parts(ptr, 50))
           };

           assert_eq!(r1, &expect[50..]);
           assert_eq!(r2, &expect[50..]);
       }

       #[test]
       fn test_merge_bio() {
           let tmp_dir = TempDir::new().unwrap();
           let s = format!(
               r###"
           {{
               "work_dir": {:?}
           }}
           "###,
               tmp_dir.as_path().to_path_buf().join("cache"),
           );

           let cache_config = CacheConfig {
               cache_validate: true,
               cache_compressed: false,
               cache_type: String::from("blobcache"),
               cache_config: serde_json::from_str(&s).unwrap(),
               prefetch_worker: BlobPrefetchConfig::default(),
           };

           let blob_cache = filecache::new(
               cache_config,
               Arc::new(MockBackend {
                   metrics: BackendMetrics::new("id", "mock"),
               }) as Arc<dyn BlobBackend + Send + Sync>,
               compress::Algorithm::Lz4Block,
               digest::Algorithm::Blake3,
               "id",
           )
           .unwrap();

           let merging_size: u64 = 128 * 1024 * 1024;

           let single_chunk = MockChunkInfo {
               compress_offset: 1000,
               compress_size: merging_size as u32 - 1,
               ..Default::default()
           };

           let bio = BlobIoDesc::new(
               Arc::new(single_chunk.clone()),
               Arc::new(BlobInfo {
                   chunk_count: 0,
                   readahead_offset: 0,
                   readahead_size: 0,
                   blob_id: "1".to_string(),
                   blob_index: 0,
                   blob_decompressed_size: 0,
                   blob_compressed_size: 0,
               }),
               50,
               50,
               RAFS_DEFAULT_BLOCK_SIZE as u32,
               true,
           );

           let (mut send, recv) = spmc::channel::<MergedBackendRequest>();
           let mut bios = vec![bio];

           blob_cache.generate_merged_requests_for_prefetch(
               &mut bios,
               &mut send,
               merging_size as usize,
           );
           let mr = recv.recv().unwrap();

           assert_eq!(mr.blob_offset, single_chunk.compress_offset());
           assert_eq!(mr.blob_size, single_chunk.compress_size());

           // ---
           let chunk1 = MockChunkInfo {
               compress_offset: 1000,
               compress_size: merging_size as u32 - 2000,
               ..Default::default()
           };

           let bio1 = BlobIoDesc::new(
               Arc::new(chunk1.clone()),
               Arc::new(BlobInfo {
                   chunk_count: 0,
                   readahead_offset: 0,
                   readahead_size: 0,
                   blob_id: "1".to_string(),
                   blob_index: 0,
                   blob_decompressed_size: 0,
                   blob_compressed_size: 0,
               }),
               50,
               50,
               RAFS_DEFAULT_BLOCK_SIZE as u32,
               true,
           );

           let chunk2 = MockChunkInfo {
               compress_offset: 1000 + merging_size - 2000,
               compress_size: 200,
               ..Default::default()
           };

           let bio2 = BlobIoDesc::new(
               Arc::new(chunk2.clone()),
               Arc::new(BlobInfo {
                   chunk_count: 0,
                   readahead_offset: 0,
                   readahead_size: 0,
                   blob_id: "1".to_string(),
                   blob_index: 0,
                   blob_decompressed_size: 0,
                   blob_compressed_size: 0,
               }),
               50,
               50,
               RAFS_DEFAULT_BLOCK_SIZE as u32,
               true,
           );

           let mut bios = vec![bio1, bio2];
           let (mut send, recv) = spmc::channel::<MergedBackendRequest>();
           blob_cache.generate_merged_requests_for_prefetch(
               &mut bios,
               &mut send,
               merging_size as usize,
           );
           let mr = recv.recv().unwrap();

           assert_eq!(mr.blob_offset, chunk1.compress_offset());
           assert_eq!(
               mr.blob_size,
               chunk1.compress_size() + chunk2.compress_size()
           );

           // ---
           let chunk1 = MockChunkInfo {
               compress_offset: 1000,
               compress_size: merging_size as u32 - 2000,
               ..Default::default()
           };

           let bio1 = BlobIoDesc::new(
               Arc::new(chunk1.clone()),
               Arc::new(BlobInfo {
                   chunk_count: 0,
                   readahead_offset: 0,
                   readahead_size: 0,
                   blob_id: "1".to_string(),
                   blob_index: 0,
                   blob_decompressed_size: 0,
                   blob_compressed_size: 0,
               }),
               50,
               50,
               RAFS_DEFAULT_BLOCK_SIZE as u32,
               true,
           );

           let chunk2 = MockChunkInfo {
               compress_offset: 1000 + merging_size - 2000 + 1,
               compress_size: 200,
               ..Default::default()
           };

           let bio2 = BlobIoDesc::new(
               Arc::new(chunk2.clone()),
               Arc::new(BlobInfo {
                   chunk_count: 0,
                   readahead_offset: 0,
                   readahead_size: 0,
                   blob_id: "1".to_string(),
                   blob_index: 0,
                   blob_decompressed_size: 0,
                   blob_compressed_size: 0,
               }),
               50,
               50,
               RAFS_DEFAULT_BLOCK_SIZE as u32,
               true,
           );

           let mut bios = vec![bio1, bio2];
           let (mut send, recv) = spmc::channel::<MergedBackendRequest>();
           blob_cache.generate_merged_requests_for_prefetch(
               &mut bios,
               &mut send,
               merging_size as usize,
           );

           let mr = recv.recv().unwrap();
           assert_eq!(mr.blob_offset, chunk1.compress_offset());
           assert_eq!(mr.blob_size, chunk1.compress_size());

           let mr = recv.recv().unwrap();
           assert_eq!(mr.blob_offset, chunk2.compress_offset());
           assert_eq!(mr.blob_size, chunk2.compress_size());

           // ---
           let chunk1 = MockChunkInfo {
               compress_offset: 1000,
               compress_size: merging_size as u32 - 2000,
               ..Default::default()
           };

           let bio1 = BlobIoDesc::new(
               Arc::new(chunk1.clone()),
               Arc::new(BlobInfo {
                   chunk_count: 0,
                   readahead_offset: 0,
                   readahead_size: 0,
                   blob_id: "1".to_string(),
                   blob_index: 0,
                   blob_decompressed_size: 0,
                   blob_compressed_size: 0,
               }),
               50,
               50,
               RAFS_DEFAULT_BLOCK_SIZE as u32,
               true,
           );

           let chunk2 = MockChunkInfo {
               compress_offset: 1000 + merging_size - 2000,
               compress_size: 200,
               ..Default::default()
           };

           let bio2 = BlobIoDesc::new(
               Arc::new(chunk2.clone()),
               Arc::new(BlobInfo {
                   chunk_count: 0,
                   readahead_offset: 0,
                   readahead_size: 0,
                   blob_id: "2".to_string(),
                   blob_index: 0,
                   blob_decompressed_size: 0,
                   blob_compressed_size: 0,
               }),
               50,
               50,
               RAFS_DEFAULT_BLOCK_SIZE as u32,
               true,
           );

           let mut bios = vec![bio1, bio2];
           let (mut send, recv) = spmc::channel::<MergedBackendRequest>();
           blob_cache.generate_merged_requests_for_prefetch(
               &mut bios,
               &mut send,
               merging_size as usize,
           );

           let mr = recv.recv().unwrap();
           assert_eq!(mr.blob_offset, chunk1.compress_offset());
           assert_eq!(mr.blob_size, chunk1.compress_size());

           let mr = recv.recv().unwrap();
           assert_eq!(mr.blob_offset, chunk2.compress_offset());
           assert_eq!(mr.blob_size, chunk2.compress_size());

           // ---
           let chunk1 = MockChunkInfo {
               compress_offset: 1000,
               compress_size: merging_size as u32 - 2000,
               ..Default::default()
           };

           let bio1 = BlobIoDesc::new(
               Arc::new(chunk1.clone()),
               Arc::new(BlobInfo {
                   chunk_count: 0,
                   readahead_offset: 0,
                   readahead_size: 0,
                   blob_id: "1".to_string(),
                   blob_index: 0,
                   blob_decompressed_size: 0,
                   blob_compressed_size: 0,
               }),
               50,
               50,
               RAFS_DEFAULT_BLOCK_SIZE as u32,
               true,
           );

           let chunk2 = MockChunkInfo {
               compress_offset: 1000 + merging_size - 2000,
               compress_size: 200,
               ..Default::default()
           };

           let bio2 = BlobIoDesc::new(
               Arc::new(chunk2.clone()),
               Arc::new(BlobInfo {
                   chunk_count: 0,
                   readahead_offset: 0,
                   readahead_size: 0,
                   blob_id: "1".to_string(),
                   blob_index: 0,
                   blob_decompressed_size: 0,
                   blob_compressed_size: 0,
               }),
               50,
               50,
               RAFS_DEFAULT_BLOCK_SIZE as u32,
               true,
           );

           let chunk3 = MockChunkInfo {
               compress_offset: 1000 + merging_size - 2000,
               compress_size: 200,
               ..Default::default()
           };

           let bio3 = BlobIoDesc::new(
               Arc::new(chunk3.clone()),
               Arc::new(BlobInfo {
                   chunk_count: 0,
                   readahead_offset: 0,
                   readahead_size: 0,
                   blob_id: "2".to_string(),
                   blob_index: 0,
                   blob_decompressed_size: 0,
                   blob_compressed_size: 0,
               }),
               50,
               50,
               RAFS_DEFAULT_BLOCK_SIZE as u32,
               true,
           );

           let mut bios = vec![bio1, bio2, bio3];
           let (mut send, recv) = spmc::channel::<MergedBackendRequest>();
           blob_cache.generate_merged_requests_for_prefetch(
               &mut bios,
               &mut send,
               merging_size as usize,
           );

           let mr = recv.recv().unwrap();
           assert_eq!(mr.blob_offset, chunk1.compress_offset());
           assert_eq!(
               mr.blob_size,
               chunk1.compress_size() + chunk2.compress_size()
           );

           let mr = recv.recv().unwrap();
           assert_eq!(mr.blob_offset, chunk3.compress_offset());
           assert_eq!(mr.blob_size, chunk3.compress_size());
       }
    */
}
