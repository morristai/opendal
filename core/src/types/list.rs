// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

use std::cmp;
use std::collections::VecDeque;
use std::pin::Pin;
use std::task::ready;
use std::task::Context;
use std::task::Poll;

use flagset::FlagSet;
use futures::future::BoxFuture;
use futures::FutureExt;
use futures::Stream;
use tokio::task::JoinHandle;

use crate::raw::*;
use crate::*;

/// Future constructed by listing.
type ListFuture = BoxFuture<'static, (oio::Lister, Result<Option<Vec<oio::Entry>>>)>;
/// Future constructed by stating.
type StatFuture = BoxFuture<'static, Result<Entry>>;

/// Lister is designed to list entries at given path in an asynchronous
/// manner.
///
/// Users can construct Lister by [`Operator::lister`].
///
/// User can use lister as `Stream<Item = Result<Entry>>`.
pub struct Lister {
    acc: FusedAccessor,
    /// required_metakey is the metakey required by users.
    required_metakey: FlagSet<Metakey>,

    buf: VecDeque<oio::Entry>,
    lister: Option<oio::Lister>,
    listing: Option<ListFuture>,

    task_queue: VecDeque<JoinHandle<(String, Result<RpStat>)>>,
    stating: Option<StatFuture>,
}

/// # Safety
///
/// Lister will only be accessed by `&mut Self`
unsafe impl Sync for Lister {}

impl Lister {
    /// Create a new lister.
    pub(crate) async fn create(acc: FusedAccessor, path: &str, args: OpList) -> Result<Self> {
        let required_metakey = args.metakey();
        let concurrent = cmp::max(1, args.concurrent());

        let (_, lister) = acc.list(path, args).await?;

        Ok(Self {
            acc,
            required_metakey,

            buf: VecDeque::new(),
            lister: Some(lister),
            listing: None,
            task_queue: VecDeque::with_capacity(concurrent),
            stating: None,
        })
    }
}

impl Stream for Lister {
    type Item = Result<Entry>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Some(fut) = self.stating.as_mut() {
            let entry = ready!(fut.poll_unpin(cx));

            // Make sure we will not poll this future again
            self.stating = None;

            return Poll::Ready(Some(entry));
        }

        if !self.task_queue.is_empty() {
            let task = self.task_queue.pop_back();
            let fut = async move {
                if let Some(task) = task {
                    let (path, rp) = task.await.map_err(|err| {
                        Error::new(
                            ErrorKind::Unexpected,
                            format!("failed to stat: {}", err).as_str(),
                        )
                    })?;
                    let metadata = rp?.into_metadata();
                    Ok(Entry::new(path, metadata))
                } else {
                    Err(Error::new(ErrorKind::Unexpected, "stat task is None"))
                }
            };
            self.stating = Some(Box::pin(fut));
            return self.poll_next(cx);
        }

        while !self.buf.is_empty() {
            if let Some(oe) = self.buf.front() {
                let (path, metadata) = oe.clone().into_entry().into_parts();
                // TODO: we can optimize this by checking the provided metakey provided by services.
                if metadata.contains_metakey(self.required_metakey) {
                    return if self.task_queue.is_empty() {
                        self.buf.pop_front();
                        Poll::Ready(Some(Ok(Entry::new(path, metadata))))
                    } else {
                        self.poll_next(cx)
                    };
                }

                if self.task_queue.len() < self.task_queue.capacity() {
                    let acc = self.acc.clone();

                    let fut = async move {
                        let res = acc.stat(&path, OpStat::default()).await;
                        (path, res)
                    };

                    self.task_queue.push_front(tokio::spawn(fut));
                    self.buf.pop_front();
                } else {
                    return self.poll_next(cx);
                };
            }
        }

        if let Some(fut) = self.listing.as_mut() {
            let (op, res) = ready!(fut.poll_unpin(cx));

            // Make sure we will not poll this future again.
            self.listing = None;

            return match res? {
                Some(oes) => {
                    self.lister = Some(op);
                    self.buf = oes.into();
                    self.poll_next(cx)
                }
                None => Poll::Ready(None),
            };
        }

        match self.lister.take() {
            Some(mut lister) => {
                let fut = async move {
                    let res = lister.next().await;

                    (lister, res)
                };
                self.listing = Some(Box::pin(fut));
                self.poll_next(cx)
            }
            None => Poll::Ready(None),
        }
    }
}

/// BlockingLister is designed to list entries at given path in a blocking
/// manner.
///
/// Users can construct Lister by `blocking_lister`.
pub struct BlockingLister {
    acc: FusedAccessor,
    /// required_metakey is the metakey required by users.
    required_metakey: FlagSet<Metakey>,

    lister: Option<oio::BlockingLister>,
    buf: VecDeque<oio::Entry>,
}

/// # Safety
///
/// BlockingLister will only be accessed by `&mut Self`
unsafe impl Sync for BlockingLister {}

impl BlockingLister {
    /// Create a new lister.
    pub(crate) fn create(acc: FusedAccessor, path: &str, args: OpList) -> Result<Self> {
        let required_metakey = args.metakey();
        let (_, lister) = acc.blocking_list(path, args)?;

        Ok(Self {
            acc,
            required_metakey,

            buf: VecDeque::new(),
            lister: Some(lister),
        })
    }
}

/// TODO: we can implement next_chunk.
impl Iterator for BlockingLister {
    type Item = Result<Entry>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(oe) = self.buf.pop_front() {
            let (path, metadata) = oe.into_entry().into_parts();
            // TODO: we can optimize this by checking the provided metakey provided by services.
            if metadata.contains_metakey(self.required_metakey) {
                return Some(Ok(Entry::new(path, metadata)));
            }

            let metadata = match self.acc.blocking_stat(&path, OpStat::default()) {
                Ok(rp) => rp.into_metadata(),
                Err(err) => return Some(Err(err)),
            };
            return Some(Ok(Entry::new(path, metadata)));
        }

        let lister = match self.lister.as_mut() {
            Some(lister) => lister,
            None => return None,
        };

        self.buf = match lister.next() {
            // Ideally, the convert from `Vec` to `VecDeque` will not do reallocation.
            //
            // However, this could be changed as described in [impl<T, A> From<Vec<T, A>> for VecDeque<T, A>](https://doc.rust-lang.org/std/collections/struct.VecDeque.html#impl-From%3CVec%3CT%2C%20A%3E%3E-for-VecDeque%3CT%2C%20A%3E)
            Ok(Some(entries)) => entries.into(),
            Ok(None) => {
                self.lister = None;
                return None;
            }
            Err(err) => return Some(Err(err)),
        };

        self.next()
    }
}

#[cfg(test)]
mod tests {
    use futures::future;
    use futures::StreamExt;

    use super::*;
    use crate::services::Azblob;

    /// Inspired by <https://gist.github.com/kyle-mccarthy/1e6ae89cc34495d731b91ebf5eb5a3d9>
    ///
    /// Invalid lister should not panic nor endless loop.
    #[tokio::test]
    async fn test_invalid_lister() -> Result<()> {
        let mut builder = Azblob::default();

        builder
            .container("container")
            .account_name("account_name")
            .account_key("account_key")
            .endpoint("https://account_name.blob.core.windows.net");

        let operator = Operator::new(builder)?.finish();

        let lister = operator.lister("/").await?;

        lister
            .filter_map(|entry| {
                dbg!(&entry);
                future::ready(entry.ok())
            })
            .for_each(|entry| {
                println!("{:?}", entry);
                future::ready(())
            })
            .await;

        Ok(())
    }
}
