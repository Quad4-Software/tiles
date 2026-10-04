//! s3:// backend: a `DataReaderTrait` implementation over `object_store`,
//! so VersaTiles/PMTiles containers can be range-read straight from a bucket.

use std::fmt::Debug;
use std::ops::Range;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use object_store::aws::AmazonS3Builder;
use object_store::path::Path as StorePath;
use object_store::{ObjectStore, ObjectStoreExt};
use versatiles_core::io::{DataReader, DataReaderTrait};
use versatiles_core::{Blob, ByteRange};

/// A parsed `s3://bucket/key` URI.
#[derive(Debug, Clone)]
pub struct S3Uri {
    pub bucket: String,
    pub key: String,
}

pub fn is_s3_uri(spec: &str) -> bool {
    spec.starts_with("s3://")
}

pub fn parse_s3_uri(spec: &str) -> Result<S3Uri> {
    let rest = spec
        .strip_prefix("s3://")
        .context("expected an s3://bucket/key URI")?;
    let (bucket, key) = rest
        .split_once('/')
        .context("expected an s3://bucket/key URI")?;
    if bucket.is_empty() || key.is_empty() {
        bail!("expected a non-empty bucket and key in '{spec}'");
    }
    Ok(S3Uri {
        bucket: bucket.to_string(),
        key: key.to_string(),
    })
}

/// Build an S3 object store for `bucket`.
///
/// Region, endpoint, credentials and `AWS_ALLOW_HTTP` come from the standard
/// AWS environment variables via `AmazonS3Builder::from_env`, which also covers
/// S3-compatible services (MinIO, R2, Garage) through `AWS_ENDPOINT`.
pub fn s3_store(bucket: &str) -> Result<Arc<dyn ObjectStore>> {
    let store = AmazonS3Builder::from_env()
        .with_bucket_name(bucket)
        .build()
        .with_context(|| {
            format!(
                "failed to build S3 client for bucket '{bucket}' \
				 (check AWS_REGION / AWS_ENDPOINT / credentials)"
            )
        })?;
    Ok(Arc::new(store))
}

/// `DataReaderTrait` over any `ObjectStore`.
///
/// Works with S3 today, and with `object_store::memory::InMemory` in tests.
/// Other backends (GCS, Azure) need only a different `store`.
pub struct ObjectStoreDataReader {
    store: Arc<dyn ObjectStore>,
    path: StorePath,
    name: String,
}

impl ObjectStoreDataReader {
    pub fn new(store: Arc<dyn ObjectStore>, key: &str, name: String) -> Self {
        Self {
            store,
            path: StorePath::from(key),
            name,
        }
    }
}

impl Debug for ObjectStoreDataReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObjectStoreDataReader")
            .field("name", &self.name)
            .finish()
    }
}

#[async_trait::async_trait]
impl DataReaderTrait for ObjectStoreDataReader {
    async fn read_range(&self, range: &ByteRange) -> Result<Blob> {
        let r: Range<u64> = range.offset..range.offset + range.length;
        let bytes = self
            .store
            .get_range(&self.path, r)
            .await
            .with_context(|| format!("S3 read_range failed for '{}'", self.name))?;
        Ok(Blob::from(bytes.to_vec()))
    }

    async fn read_all(&self) -> Result<Blob> {
        let bytes = self
            .store
            .get(&self.path)
            .await
            .with_context(|| format!("S3 read failed for '{}'", self.name))?
            .bytes()
            .await?;
        Ok(Blob::from(bytes.to_vec()))
    }

    fn name(&self) -> &str {
        &self.name
    }
}

/// Object size via HEAD, used by the fetch fast path for progress reporting.
pub async fn s3_object_size(store: &Arc<dyn ObjectStore>, key: &str) -> Result<u64> {
    let meta = store.head(&StorePath::from(key)).await?;
    Ok(meta.size)
}

/// Turn an `s3://` URI into a boxed `DataReader` for the container readers.
pub fn s3_data_reader(spec: &str) -> Result<DataReader> {
    let uri = parse_s3_uri(spec)?;
    let store = s3_store(&uri.bucket)?;
    Ok(Box::new(ObjectStoreDataReader::new(
        store,
        &uri.key,
        spec.to_string(),
    )))
}
