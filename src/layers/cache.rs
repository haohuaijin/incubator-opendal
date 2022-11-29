// Copyright 2022 Datafuse Labs.
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

use std::fmt::Debug;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;

use async_trait::async_trait;
use futures::future::BoxFuture;
use futures::io::copy;
use futures::io::Cursor;
use futures::ready;
use futures::AsyncRead;
use futures::AsyncReadExt;
use futures::FutureExt;

use crate::raw::*;
use crate::*;

/// ContentCacheLayer will add content data cache support for OpenDAL.
///
/// # Notes
///
/// This layer only maintains its own states. Users should care about the cache
/// consistency by themselves. For example, in the following situations, users
/// could get out-dated metadata cache:
///
/// - Users have operations on underlying operator directly.
/// - Other nodes have operations on underlying storage directly.
/// - Concurrent read/write/delete on the same path.
///
/// To make sure content cache consistent across the cluster, please make sure
/// all nodes in the cluster use the same cache services like redis or tikv.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
///
/// use anyhow::Result;
/// use opendal::layers::CacheLayer;
/// use opendal::layers::CacheStrategy;
/// use opendal::services::memory;
/// use opendal::Operator;
/// use opendal::Scheme;
///
/// let _ = Operator::from_env(Scheme::Fs)
///     .expect("must init")
///     .layer(CacheLayer::new(
///         Operator::from_env(Scheme::Memory).expect("must init"),
///         CacheStrategy::Whole,
///     ));
/// ```
#[derive(Debug, Clone)]
pub struct CacheLayer {
    cache: Arc<dyn Accessor>,
    strategy: CacheStrategy,
}

impl CacheLayer {
    /// Create a new metadata cache layer.
    pub fn new(cache: Operator, strategy: CacheStrategy) -> Self {
        Self {
            cache: cache.inner(),
            strategy,
        }
    }
}

impl Layer for CacheLayer {
    fn layer(&self, inner: Arc<dyn Accessor>) -> Arc<dyn Accessor> {
        Arc::new(ContentCacheAccessor {
            inner,
            cache: self.cache.clone(),
            strategy: self.strategy.clone(),
        })
    }
}

/// The strategy of content cache.
#[derive(Debug, Clone)]
pub enum CacheStrategy {
    /// Always cache the whole object content.
    Whole,
    /// Cache the object content in parts with fixed size.
    Fixed(u64),
}

#[derive(Debug, Clone)]
struct ContentCacheAccessor {
    inner: Arc<dyn Accessor>,
    cache: Arc<dyn Accessor>,

    strategy: CacheStrategy,
}

#[async_trait]
impl Accessor for ContentCacheAccessor {
    fn inner(&self) -> Option<Arc<dyn Accessor>> {
        Some(self.inner.clone())
    }

    async fn create(&self, path: &str, args: OpCreate) -> Result<RpCreate> {
        self.cache
            .delete(&format_meta_cache_path(path), OpDelete::new())
            .await?;
        self.inner.create(path, args).await
    }

    async fn read(&self, path: &str, args: OpRead) -> Result<(RpRead, BytesReader)> {
        match self.strategy {
            CacheStrategy::Whole => self.new_whole_cache_reader(path, args).await,
            CacheStrategy::Fixed(step) => self.new_fixed_cache_reader(path, args, step).await,
        }
    }

    async fn write(&self, path: &str, args: OpWrite, r: BytesReader) -> Result<RpWrite> {
        self.cache
            .delete(&format_meta_cache_path(path), OpDelete::new())
            .await?;
        self.inner.write(path, args, r).await
    }

    async fn stat(&self, path: &str, args: OpStat) -> Result<RpStat> {
        match self
            .cache
            .read(&format_meta_cache_path(path), OpRead::new())
            .await
        {
            Ok((_, mut r)) => {
                let mut bs = Vec::with_capacity(1024);
                r.read_to_end(&mut bs).await.map_err(|err| {
                    Error::new(ErrorKind::Unexpected, "read object metadata from cache")
                        .set_source(err)
                })?;

                let meta = self.decode_metadata(&bs)?;
                Ok(RpStat::new(meta))
            }
            Err(err) if err.kind() == ErrorKind::ObjectNotFound => {
                let meta = self.inner.stat(path, args).await?.into_metadata();
                let bs = self.encode_metadata(&meta)?;
                self.cache
                    .write(
                        path,
                        OpWrite::new(bs.len() as u64),
                        Box::new(Cursor::new(bs)),
                    )
                    .await?;
                Ok(RpStat::new(meta))
            }
            // We will ignore any other errors happened in cache.
            Err(_) => self.inner.stat(path, args).await,
        }
    }

    async fn delete(&self, path: &str, args: OpDelete) -> Result<RpDelete> {
        self.cache
            .delete(&format_meta_cache_path(path), OpDelete::new())
            .await?;
        self.inner.delete(path, args).await
    }
}

impl ContentCacheAccessor {
    /// Create a new whole cache reader.
    async fn new_whole_cache_reader(
        &self,
        path: &str,
        args: OpRead,
    ) -> Result<(RpRead, BytesReader)> {
        match self.cache.read(path, args.clone()).await {
            Ok(r) => Ok(r),
            Err(err) if err.kind() == ErrorKind::ObjectNotFound => {
                let (rp, r) = self.inner.read(path, OpRead::new()).await?;

                let length = rp.into_metadata().content_length();
                self.cache.write(path, OpWrite::new(length), r).await?;
                self.cache.read(path, args).await
            }
            Err(err) => Err(err),
        }
    }

    async fn new_fixed_cache_reader(
        &self,
        path: &str,
        args: OpRead,
        step: u64,
    ) -> Result<(RpRead, BytesReader)> {
        let range = args.range();
        let it = match (range.offset(), range.size()) {
            (Some(offset), Some(size)) => FixedCacheRangeIterator::new(offset, size, step),
            _ => {
                let meta = self.inner.stat(path, OpStat::new()).await?.into_metadata();
                let bcr = BytesContentRange::from_bytes_range(meta.content_length(), range);
                let br = bcr.to_bytes_range().expect("bytes range must be valid");
                FixedCacheRangeIterator::new(
                    br.offset().expect("offset must be valid"),
                    br.size().expect("size must be valid"),
                    step,
                )
            }
        };

        let length = it.size();
        let r = FixedCacheReader::new(self.inner.clone(), self.cache.clone(), path, it);
        Ok((RpRead::new(length), Box::new(r)))
    }

    fn encode_metadata(&self, meta: &ObjectMetadata) -> Result<Vec<u8>> {
        bincode::serde::encode_to_vec(meta, bincode::config::standard()).map_err(|err| {
            Error::new(ErrorKind::Unexpected, "encode object metadata into cache")
                .with_operation("CacheLayer::encode_metadata")
                .set_source(err)
        })
    }

    fn decode_metadata(&self, bs: &[u8]) -> Result<ObjectMetadata> {
        let (meta, _) = bincode::serde::decode_from_slice(bs, bincode::config::standard())
            .map_err(|err| {
                Error::new(ErrorKind::Unexpected, "decode object metadata from cache")
                    .with_operation("CacheLayer::decode_metadata")
                    .set_source(err)
            })?;
        Ok(meta)
    }
}

#[derive(Copy, Clone, Debug)]
struct FixedCacheRangeIterator {
    offset: u64,
    size: u64,
    step: u64,

    cur: u64,
}

impl FixedCacheRangeIterator {
    fn new(offset: u64, size: u64, step: u64) -> Self {
        Self {
            offset,
            size,
            step,

            cur: offset,
        }
    }

    fn size(&self) -> u64 {
        self.size
    }

    /// Cache index is the file index across the whole file.
    fn cache_index(&self) -> u64 {
        self.cur / self.step
    }

    /// Cache range is the range that we need to read from cache file.
    fn cache_range(&self) -> BytesRange {
        let skipped_rem = self.cur % self.step;
        let to_read = self.size + self.offset - self.cur;
        if to_read >= (self.step - skipped_rem) {
            (skipped_rem..self.step).into()
        } else {
            (skipped_rem..skipped_rem + to_read).into()
        }
    }

    /// Total range is the range that we need to read from underlying storage.
    ///
    /// # Note
    ///
    /// We will always read `step` bytes from underlying storage.
    fn total_range(&self) -> BytesRange {
        let idx = self.cur / self.step;
        (self.step * idx..self.step * (idx + 1)).into()
    }
}

impl Iterator for FixedCacheRangeIterator {
    /// Item with return (cache_idx, cache_range, total_range)
    type Item = (u64, BytesRange, BytesRange);

    fn next(&mut self) -> Option<Self::Item> {
        if self.cur >= self.offset + self.size {
            None
        } else {
            let (cache_index, cache_range, total_range) =
                (self.cache_index(), self.cache_range(), self.total_range());
            self.cur += cache_range.size().expect("cache range size must be valid");
            Some((cache_index, cache_range, total_range))
        }
    }
}

enum FixedCacheState {
    Iterating(FixedCacheRangeIterator),
    Fetching(
        (
            FixedCacheRangeIterator,
            BoxFuture<'static, Result<(RpRead, BytesReader)>>,
        ),
    ),
    Reading((FixedCacheRangeIterator, (RpRead, BytesReader))),
}

/// Build the path for OpenDAL Content Cache.
fn format_content_cache_path(path: &str, idx: u64) -> String {
    format!("{path}.occ_{idx}")
}

/// Build the path for OpenDAL Metadata Cache.
fn format_meta_cache_path(path: &str) -> String {
    format!("{path}.omc")
}

struct FixedCacheReader {
    inner: Arc<dyn Accessor>,
    cache: Arc<dyn Accessor>,
    state: FixedCacheState,

    path: String,
}

impl FixedCacheReader {
    fn new(
        inner: Arc<dyn Accessor>,
        cache: Arc<dyn Accessor>,
        path: &str,
        it: FixedCacheRangeIterator,
    ) -> Self {
        Self {
            inner,
            cache,
            state: FixedCacheState::Iterating(it),
            path: path.to_string(),
        }
    }
}

impl AsyncRead for FixedCacheReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let cache = self.cache.clone();
        let inner = self.inner.clone();
        let path = self.path.clone();

        match &mut self.state {
            FixedCacheState::Iterating(it) => {
                let range = it.next();
                match range {
                    None => Poll::Ready(Ok(0)),
                    Some((idx, cache_range, total_range)) => {
                        let cache_path = format_content_cache_path(&path, idx);
                        let fut = async move {
                            match cache
                                .read(&cache_path, OpRead::new().with_range(cache_range))
                                .await
                            {
                                Ok(r) => Ok(r),
                                Err(err) if err.kind() == ErrorKind::ObjectNotFound => {
                                    let (rp, r) = inner
                                        .read(&path, OpRead::new().with_range(total_range))
                                        .await?;
                                    let size = rp.into_metadata().content_length();
                                    let mut bs = Vec::with_capacity(size as usize);
                                    copy(r, &mut bs).await.map_err(|err| {
                                        Error::new(ErrorKind::Unexpected, "read from inner storage")
                                            .with_operation(Operation::Read.into_static())
                                            .with_context("path", &cache_path)
                                            .set_source(err)
                                    })?;

                                    cache
                                        .write(
                                            &cache_path,
                                            OpWrite::new(size),
                                            Box::new(Cursor::new(bs.clone())),
                                        )
                                        .await?;

                                    // TODO: Extract this as a new function.
                                    let br = cache_range;
                                    let bs = match (br.offset(), br.size()) {
                                        (Some(offset), Some(size)) => {
                                            let mut bs = bs.split_off(offset as usize);
                                            if (size as usize) < bs.len() {
                                                let _ = bs.split_off(size as usize);
                                            }
                                            bs
                                        }
                                        (Some(offset), None) => bs.split_off(offset as usize),
                                        (None, Some(size)) => {
                                            bs.split_off(bs.len() - size as usize)
                                        }
                                        (None, None) => bs,
                                    };
                                    let length = bs.len();

                                    Ok((
                                        RpRead::new(length as u64),
                                        Box::new(Cursor::new(bs)) as BytesReader,
                                    ))
                                }
                                Err(err) => Err(err),
                            }
                        };
                        self.state = FixedCacheState::Fetching((*it, Box::pin(fut)));
                        self.poll_read(cx, buf)
                    }
                }
            }
            FixedCacheState::Fetching((it, fut)) => {
                let r = ready!(fut.poll_unpin(cx))?;
                self.state = FixedCacheState::Reading((*it, r));
                self.poll_read(cx, buf)
            }
            FixedCacheState::Reading((it, (_, r))) => {
                let n = match ready!(Pin::new(r).poll_read(cx, buf)) {
                    Ok(n) => n,
                    Err(err) => return Poll::Ready(Err(err)),
                };

                if n == 0 {
                    self.state = FixedCacheState::Iterating(*it);
                    self.poll_read(cx, buf)
                } else {
                    Poll::Ready(Ok(n))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::memory;
    use crate::Operator;

    #[tokio::test]
    async fn test_whole_content_cache() -> anyhow::Result<()> {
        let op = Operator::new(memory::Builder::default().build()?);

        let cache_layer = CacheLayer::new(
            Arc::new(memory::Builder::default().build()?).into(),
            CacheStrategy::Whole,
        );
        let cached_op = op.clone().layer(cache_layer);

        // Write a new object into op.
        op.object("test_exist")
            .write("Hello, World!".as_bytes())
            .await?;

        // Read from cached op.
        let data = cached_op.object("test_exist").read().await?;
        assert_eq!(data.len(), 13);

        // Wait for https://github.com/datafuselabs/opendal/issues/957
        // // Write into cache op.
        // cached_op
        //     .object("test_exist")
        //     .write("Hello, Xuanwo!".as_bytes())
        //     .await?;
        // // op and cached op should have same data.
        // let data = op.object("test_exist").read().await?;
        // assert_eq!(data.len(), 14);
        // let data = cached_op.object("test_exist").read().await?;
        // assert_eq!(data.len(), 14);

        // Read not exist object.
        let data = cached_op.object("test_not_exist").read().await;
        assert_eq!(data.unwrap_err().kind(), ErrorKind::ObjectNotFound);

        Ok(())
    }

    #[tokio::test]
    async fn test_fixed_content_cache() -> anyhow::Result<()> {
        let op = Operator::new(memory::Builder::default().build()?);

        let cache_layer = CacheLayer::new(
            Arc::new(memory::Builder::default().build()?).into(),
            CacheStrategy::Fixed(5),
        );
        let cached_op = op.clone().layer(cache_layer);

        // Write a new object into op.
        op.object("test_exist")
            .write("Hello, World!".as_bytes())
            .await?;

        // Read from cached op.
        let data = cached_op.object("test_exist").read().await?;
        assert_eq!(data.len(), 13);

        // Wait for https://github.com/datafuselabs/opendal/issues/957
        // Write into cache op.
        // cached_op
        //     .object("test_exist")
        //     .write("Hello, Xuanwo!".as_bytes())
        //     .await?;
        // // op and cached op should have same data.
        // let data = op.object("test_exist").read().await?;
        // assert_eq!(data.len(), 14);
        // let data = cached_op.object("test_exist").read().await?;
        // assert_eq!(data.len(), 14);

        // Read part of data
        let data = cached_op.object("test_exist").range_read(5..).await?;
        assert_eq!(data.len(), 8);
        assert_eq!(data, ", World!".as_bytes());

        // Write a new object into op.
        op.object("test_new")
            .write("Hello, OpenDAL!".as_bytes())
            .await?;

        // Read part of data
        let data = cached_op.object("test_new").range_read(6..).await?;
        assert_eq!(data.len(), 9);
        assert_eq!(data, " OpenDAL!".as_bytes());

        // Read not exist object.
        let data = cached_op.object("test_not_exist").read().await;
        assert_eq!(data.unwrap_err().kind(), ErrorKind::ObjectNotFound);

        Ok(())
    }

    #[test]
    fn test_fixed_cache_range_iterator() {
        let cases = vec![
            (
                "first part",
                0,
                1,
                1000,
                vec![(0, BytesRange::from(0..1), BytesRange::from(0..1000))],
            ),
            (
                "first part with offset",
                900,
                1,
                1000,
                vec![(0, BytesRange::from(900..901), BytesRange::from(0..1000))],
            ),
            (
                "first part with edge case",
                900,
                100,
                1000,
                vec![(0, BytesRange::from(900..1000), BytesRange::from(0..1000))],
            ),
            (
                "two parts",
                900,
                101,
                1000,
                vec![
                    (0, BytesRange::from(900..1000), BytesRange::from(0..1000)),
                    (1, BytesRange::from(0..1), BytesRange::from(1000..2000)),
                ],
            ),
            (
                "second part",
                1001,
                1,
                1000,
                vec![(1, BytesRange::from(1..2), BytesRange::from(1000..2000))],
            ),
        ];

        for (name, offset, size, step, expected) in cases {
            let it = FixedCacheRangeIterator::new(offset, size, step);
            let actual: Vec<_> = it.collect();

            assert_eq!(expected, actual, "{name}")
        }
    }

    #[tokio::test]
    async fn test_metadata_cache() -> anyhow::Result<()> {
        let op = Operator::new(memory::Builder::default().build()?);

        let cache_layer = CacheLayer::new(
            Arc::new(memory::Builder::default().build()?).into(),
            CacheStrategy::Fixed(5),
        );
        let cached_op = op.clone().layer(cache_layer);

        // Write a new object into op.
        op.object("test_exist")
            .write("Hello, World!".as_bytes())
            .await?;
        // Stat from cached op.
        let meta = cached_op.object("test_exist").metadata().await?;
        assert_eq!(meta.content_length(), 13);

        // Write into cache op.
        cached_op
            .object("test_exist")
            .write("Hello, Xuanwo!".as_bytes())
            .await?;
        // op and cached op should have same data.
        let meta = op.object("test_exist").metadata().await?;
        assert_eq!(meta.content_length(), 14);
        let meta = cached_op.object("test_exist").metadata().await?;
        assert_eq!(meta.content_length(), 14);

        // Stat not exist object.
        let meta = cached_op.object("test_not_exist").metadata().await;
        assert_eq!(meta.unwrap_err().kind(), ErrorKind::ObjectNotFound);

        Ok(())
    }
}