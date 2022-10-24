use std::{
    borrow::Cow,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use anyhow::{anyhow, Context, Result};
use cid::Cid;
use iroh_metrics::{
    core::{MObserver, MRecorder},
    inc, observe, record,
    store::{StoreHistograms, StoreMetrics},
};
use iroh_rpc_client::Client as RpcClient;
use libmdbx::{
    DatabaseFlags, Environment, EnvironmentKind, TransactionKind, WriteFlags, WriteMap, RW,
};
use multihash::Multihash;
use smallvec::SmallVec;
use tokio::task;

use crate::{
    cf::{GraphV0, MetadataV0, CF_BLOBS_V0, CF_GRAPH_V0, CF_ID_V0, CF_METADATA_V0},
    Config,
};

#[derive(Clone)]
pub struct Store {
    inner: Arc<InnerStore>,
}

struct InnerStore {
    content: Environment<WriteMap>,
    next_id: AtomicU64,
    _rpc_client: RpcClient,
}

/// The key used in CF_ID_V0
///
/// The multihash followed by the be encoded code. This allows both looking up an id by multihash and code (aka Cid),
/// and looking up all codes and ids for a multihash, for the rare case that there are mulitple cids with the same
/// multihash but different codes.
fn id_key(cid: &Cid) -> SmallVec<[u8; 64]> {
    let mut key = SmallVec::new();
    cid.hash().write(&mut key).unwrap();
    key.extend_from_slice(&cid.codec().to_be_bytes());
    key
}

/// Struct used to iterate over all the ids for a multihash
struct CodeAndId {
    // the ipld code of the id
    #[allow(dead_code)]
    code: u64,
    // the id for the cid, used in most other column families
    id: u64,
}

impl Store {
    /// Creates a new database.
    #[tracing::instrument]
    pub async fn create(config: Config) -> Result<Self> {
        let path = config.path.clone();
        let db = task::spawn_blocking(move || -> Result<_> {
            let db = libmdbx::Environment::new().set_max_dbs(4).open(&path)?;
            let tx = db.begin_rw_txn()?;
            tx.create_db(Some(CF_BLOBS_V0), DatabaseFlags::default())?;
            tx.create_db(Some(CF_METADATA_V0), DatabaseFlags::default())?;
            tx.create_db(Some(CF_GRAPH_V0), DatabaseFlags::default())?;
            tx.create_db(Some(CF_ID_V0), DatabaseFlags::default())?;
            tx.commit()?;

            Ok(db)
        })
        .await??;

        let _rpc_client = RpcClient::new(config.rpc_client)
            .await
            .context("Error creating rpc client for store")?;

        Ok(Store {
            inner: Arc::new(InnerStore {
                content: db,
                next_id: 1.into(),
                _rpc_client,
            }),
        })
    }

    /// Opens an existing database.
    #[tracing::instrument]
    pub async fn open(config: Config) -> Result<Self> {
        let path = config.path.clone();
        let (db, next_id) = task::spawn_blocking(move || -> Result<_> {
            let db = libmdbx::Environment::new().set_max_dbs(4).open(&path)?;

            // read last inserted id
            let next_id = {
                let tx = db.begin_ro_txn()?;
                let db = tx.open_db(Some(CF_METADATA_V0))?;
                let last_id = tx
                    .cursor(&db)?
                    .last::<Cow<_>, Cow<_>>()?
                    .and_then(|(key, _)| key[..8].try_into().ok())
                    .map(u64::from_be_bytes)
                    .unwrap_or_default();

                last_id + 1
            };

            Ok((db, next_id))
        })
        .await??;

        let _rpc_client = RpcClient::new(config.rpc_client)
            .await
            // TODO: first conflict between `anyhow` & `anyhow`
            // .map_err(|e| e.context("Error creating rpc client for store"))?;
            .map_err(|e| anyhow!("Error creating rpc client for store: {:?}", e))?;

        Ok(Store {
            inner: Arc::new(InnerStore {
                content: db,
                next_id: next_id.into(),
                _rpc_client,
            }),
        })
    }

    #[tracing::instrument(skip(self, links, blob))]
    pub async fn put<T: AsRef<[u8]>, L>(&self, cid: Cid, blob: T, links: L) -> Result<()>
    where
        L: IntoIterator<Item = Cid>,
    {
        inc!(StoreMetrics::PutRequests);

        let tx = self.inner.content.begin_rw_txn()?;

        if Self::priv_has(&tx, &cid)? {
            return Ok(());
        }

        let id = self.next_id();

        let start = std::time::Instant::now();

        let id_bytes = id.to_be_bytes();

        // guranteed that the key does not exists, so we want to store it

        let metadata = MetadataV0 {
            codec: cid.codec(),
            multihash: cid.hash().to_bytes(),
        };
        let metadata_bytes = rkyv::to_bytes::<_, 1024>(&metadata)?; // TODO: is this the right amount of scratch space?
        let id_key = id_key(&cid);

        let children = Self::ensure_id_many(&tx, || self.next_id(), links.into_iter()).await?;

        let graph = GraphV0 { children };
        let graph_bytes = rkyv::to_bytes::<_, 1024>(&graph)?; // TODO: is this the right amount of scratch space?

        let blob_size = blob.as_ref().len();

        tx.put(
            &tx.open_db(Some(CF_ID_V0))?,
            id_key,
            &id_bytes,
            WriteFlags::UPSERT,
        )?;
        tx.put(
            &tx.open_db(Some(CF_BLOBS_V0))?,
            &id_bytes,
            blob,
            WriteFlags::UPSERT,
        )?;
        tx.put(
            &tx.open_db(Some(CF_METADATA_V0))?,
            &id_bytes,
            metadata_bytes,
            WriteFlags::UPSERT,
        )?;
        tx.put(
            &tx.open_db(Some(CF_GRAPH_V0))?,
            &id_bytes,
            graph_bytes,
            WriteFlags::UPSERT,
        )?;

        tx.commit()?;
        observe!(StoreHistograms::PutRequests, start.elapsed().as_secs_f64());
        record!(StoreMetrics::PutBytes, blob_size as u64);

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub async fn get_blob_by_hash(&self, hash: &Multihash) -> Result<Option<Vec<u8>>> {
        let tx = self.inner.content.begin_ro_txn()?;
        let cf_blobs = tx.open_db(Some(CF_BLOBS_V0))?;
        for elem in Self::get_ids_for_hash(&tx, hash)? {
            let id = elem?.id;
            let id_bytes = id.to_be_bytes();
            if let Some(blob) = tx.get(&cf_blobs, &id_bytes)? {
                return Ok(Some(blob));
            }
        }
        Ok(None)
    }

    #[tracing::instrument(skip(self))]
    pub async fn has_blob_for_hash(&self, hash: &Multihash) -> Result<bool> {
        let tx = self.inner.content.begin_ro_txn()?;
        let cf_blobs = tx.open_db(Some(CF_BLOBS_V0))?;
        for elem in Self::get_ids_for_hash(&tx, hash)? {
            let id = elem?.id;
            let id_bytes = id.to_be_bytes();
            if let Some(_blob) = tx.get::<()>(&cf_blobs, &id_bytes)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    #[tracing::instrument(skip(self))]
    pub async fn get(&self, cid: &Cid) -> Result<Option<Vec<u8>>> {
        inc!(StoreMetrics::GetRequests);
        let start = std::time::Instant::now();
        let tx = self.inner.content.begin_ro_txn()?;
        let res = match Self::get_id(&tx, cid)? {
            Some(id) => {
                let maybe_blob = Self::get_by_id(&tx, id)?.map(|v| v.into_owned());
                inc!(StoreMetrics::StoreHit);
                record!(
                    StoreMetrics::GetBytes,
                    maybe_blob.as_ref().map(|b| b.len()).unwrap_or(0) as u64
                );
                Ok(maybe_blob)
            }
            None => {
                inc!(StoreMetrics::StoreMiss);
                Ok(None)
            }
        };
        observe!(StoreHistograms::GetRequests, start.elapsed().as_secs_f64());
        res
    }

    #[tracing::instrument(skip(self))]
    pub async fn get_size(&self, cid: &Cid) -> Result<Option<usize>> {
        let tx = self.inner.content.begin_ro_txn()?;
        match Self::get_id(&tx, cid)? {
            Some(id) => {
                inc!(StoreMetrics::StoreHit);
                let maybe_size = Self::get_size_by_id(&tx, id)?;
                Ok(maybe_size)
            }
            None => {
                inc!(StoreMetrics::StoreMiss);
                Ok(None)
            }
        }
    }

    #[tracing::instrument(skip(tx))]
    fn priv_has<'tx, K: TransactionKind, E: EnvironmentKind>(
        tx: &'tx libmdbx::Transaction<'tx, K, E>,
        cid: &Cid,
    ) -> Result<bool> {
        match Self::get_id(tx, cid)? {
            Some(id) => {
                let exists = tx
                    .get::<()>(&tx.open_db(Some(CF_BLOBS_V0))?, &id.to_be_bytes())?
                    .is_some();
                Ok(exists)
            }
            None => Ok(false),
        }
    }

    #[tracing::instrument(skip(self))]
    pub async fn has(&self, cid: &Cid) -> Result<bool> {
        let tx = self.inner.content.begin_ro_txn()?;
        Self::priv_has(&tx, cid)
    }

    #[tracing::instrument(skip(self))]
    pub async fn get_links(&self, cid: &Cid) -> Result<Option<Vec<Cid>>> {
        inc!(StoreMetrics::GetLinksRequests);
        let start = std::time::Instant::now();
        let tx = self.inner.content.begin_ro_txn()?;
        let res = match Self::get_id(&tx, cid)? {
            Some(id) => {
                let maybe_links = Self::get_links_by_id(&tx, id)?;
                inc!(StoreMetrics::GetLinksHit);
                Ok(maybe_links)
            }
            None => {
                inc!(StoreMetrics::GetLinksMiss);
                Ok(None)
            }
        };
        observe!(
            StoreHistograms::GetLinksRequests,
            start.elapsed().as_secs_f64()
        );
        res
    }

    #[tracing::instrument(skip(tx))]
    fn get_id<'tx, K: TransactionKind, E: EnvironmentKind>(
        tx: &libmdbx::Transaction<'_, K, E>,
        cid: &Cid,
    ) -> Result<Option<u64>> {
        let id_key = id_key(cid);
        let maybe_id_bytes = tx.get::<Cow<_>>(&tx.open_db(Some(CF_ID_V0))?, &id_key)?;
        match maybe_id_bytes {
            Some(bytes) => {
                let arr = bytes[..8].try_into().map_err(|e| anyhow!("{:?}", e))?;
                Ok(Some(u64::from_be_bytes(arr)))
            }
            None => Ok(None),
        }
    }

    fn get_ids_for_hash<'tx, K: TransactionKind, E: EnvironmentKind>(
        tx: &'tx libmdbx::Transaction<'tx, K, E>,
        hash: &Multihash,
    ) -> Result<impl Iterator<Item = Result<CodeAndId>> + 'tx> {
        let hash = hash.to_bytes();
        let cf_id = tx.open_db(Some(CF_ID_V0))?;
        let iter = tx.cursor(&cf_id)?.into_iter_from::<Cow<_>, Cow<_>>(&hash);
        let hash_len = hash.len();
        Ok(iter
            .take_while(move |elem| {
                if let Ok((k, _)) = elem {
                    k.len() == hash_len + 8 && k.starts_with(&hash)
                } else {
                    // we don't want to swallow errors. An error is not the same as no result!
                    true
                }
            })
            .map(move |elem| {
                let (k, v) = elem?;
                let code = u64::from_be_bytes(k[hash_len..].try_into()?);
                let id = u64::from_be_bytes(v[..8].try_into()?);
                Ok(CodeAndId { code, id })
            }))
    }

    #[tracing::instrument(skip(tx))]
    fn get_by_id<'tx, K: TransactionKind, E: EnvironmentKind>(
        tx: &'tx libmdbx::Transaction<'tx, K, E>,
        id: u64,
    ) -> Result<Option<Cow<'tx, [u8]>>> {
        let maybe_blob = tx.get::<Cow<_>>(&tx.open_db(Some(CF_BLOBS_V0))?, &id.to_be_bytes())?;

        Ok(maybe_blob)
    }

    #[tracing::instrument(skip(tx))]
    fn get_size_by_id<'tx, K: TransactionKind, E: EnvironmentKind>(
        tx: &'tx libmdbx::Transaction<'tx, K, E>,
        id: u64,
    ) -> Result<Option<usize>> {
        let cf_blobs = tx.open_db(Some(CF_BLOBS_V0))?;
        let maybe_blob = tx.get::<Cow<_>>(&cf_blobs, &id.to_be_bytes())?;
        let maybe_size = maybe_blob.map(|b| b.len());
        Ok(maybe_size)
    }

    #[tracing::instrument(skip(tx))]
    fn get_links_by_id<'tx, K: TransactionKind, E: EnvironmentKind>(
        tx: &'tx libmdbx::Transaction<'tx, K, E>,
        id: u64,
    ) -> Result<Option<Vec<Cid>>> {
        let cf_graph = tx.open_db(Some(CF_GRAPH_V0))?;
        let id_bytes = id.to_be_bytes();
        // FIXME: can't use pinned because otherwise this can trigger alignment issues :/
        match tx.get::<Cow<_>>(&cf_graph, &id_bytes)? {
            Some(links_id) => {
                let cf_meta = tx.open_db(Some(CF_METADATA_V0))?;
                let graph = rkyv::check_archived_root::<GraphV0>(&links_id)
                    .map_err(|e| anyhow!("{:?}", e))?;
                let mut links = Vec::with_capacity(graph.children.len());
                for key in graph.children.iter() {
                    let meta = tx
                        .get::<Cow<_>>(&cf_meta, &key.to_be_bytes())?
                        .ok_or_else(|| anyhow::format_err!("invalid link: {}", key))?;

                    let meta = rkyv::check_archived_root::<MetadataV0>(&meta)
                        .map_err(|e| anyhow!("{:?}", e))?;
                    let multihash = cid::multihash::Multihash::from_bytes(&meta.multihash)?;
                    let c = cid::Cid::new_v1(meta.codec, multihash);
                    links.push(c);
                }
                Ok(Some(links))
            }
            None => Ok(None),
        }
    }

    /// Takes a list of cids and gives them ids, which are boths stored and then returned.
    #[tracing::instrument(skip(tx, next_id_source, cids))]
    async fn ensure_id_many<E: EnvironmentKind, I>(
        tx: &libmdbx::Transaction<'_, RW, E>,
        next_id_source: impl Fn() -> u64,
        cids: I,
    ) -> Result<Vec<u64>>
    where
        I: IntoIterator<Item = Cid>,
    {
        let cf_id = tx.open_db(Some(CF_ID_V0))?;
        let cf_meta = tx.open_db(Some(CF_METADATA_V0))?;

        let mut ids = Vec::new();
        for cid in cids {
            let id_key = id_key(&cid);
            let id = if let Some(id) = tx.get::<Cow<_>>(&cf_id, &id_key)? {
                u64::from_be_bytes(id.as_ref().try_into()?)
            } else {
                let id = (next_id_source)();
                let id_bytes = id.to_be_bytes();

                let metadata = MetadataV0 {
                    codec: cid.codec(),
                    multihash: cid.hash().to_bytes(),
                };
                let metadata_bytes = rkyv::to_bytes::<_, 1024>(&metadata)?; // TODO: is this the right amount of scratch space?
                tx.put(&cf_id, id_key, &id_bytes, WriteFlags::UPSERT)?;
                tx.put(&cf_meta, &id_bytes, metadata_bytes, WriteFlags::UPSERT)?;
                id
            };
            ids.push(id);
        }

        Ok(ids)
    }

    #[tracing::instrument(skip(self))]
    fn next_id(&self) -> u64 {
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        // TODO: better handling
        assert!(id > 0, "this store is full");
        id
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    use iroh_metrics::config::Config as MetricsConfig;
    use iroh_rpc_client::Config as RpcClientConfig;

    use cid::multihash::{Code, MultihashDigest};
    use libipld::{prelude::Encode, IpldCodec};
    use tempfile::TempDir;
    const RAW: u64 = 0x55;

    #[tokio::test]
    async fn test_basics() {
        let dir = tempfile::tempdir().unwrap();
        let rpc_client = RpcClientConfig::default();
        let config = Config {
            path: dir.path().into(),
            rpc_client,
            metrics: MetricsConfig::default(),
        };

        let store = Store::create(config).await.unwrap();

        let mut values = Vec::new();

        for i in 0..100 {
            let data = vec![i as u8; i * 16];
            let hash = Code::Sha2_256.digest(&data);
            let c = cid::Cid::new_v1(RAW, hash);

            let link_hash = Code::Sha2_256.digest(&[(i + 1) as u8; 64]);
            let link = cid::Cid::new_v1(RAW, link_hash);

            let links = [link];

            store.put(c, &data, links).await.unwrap();
            values.push((c, data, links));
        }

        for (i, (c, expected_data, expected_links)) in values.iter().enumerate() {
            dbg!(i);
            assert!(store.has(c).await.unwrap());
            let data = store.get(c).await.unwrap().unwrap();
            assert_eq!(expected_data, &data[..]);

            let links = store.get_links(c).await.unwrap().unwrap();
            assert_eq!(expected_links, &links[..]);
        }
    }

    #[tokio::test]
    async fn test_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let rpc_client = RpcClientConfig::default();
        let config = Config {
            path: dir.path().into(),
            rpc_client,
            metrics: MetricsConfig::default(),
        };

        let store = Store::create(config.clone()).await.unwrap();

        let mut values = Vec::new();

        for i in 0..100 {
            let data = vec![i as u8; i * 16];
            let hash = Code::Sha2_256.digest(&data);
            let c = cid::Cid::new_v1(RAW, hash);

            let link_hash = Code::Sha2_256.digest(&[(i + 1) as u8; 64]);
            let link = cid::Cid::new_v1(RAW, link_hash);

            let links = [link];

            store.put(c, &data, links).await.unwrap();
            values.push((c, data, links));
        }

        for (c, expected_data, expected_links) in values.iter() {
            let data = store.get(c).await.unwrap().unwrap();
            assert_eq!(expected_data, &data[..]);

            let links = store.get_links(c).await.unwrap().unwrap();
            assert_eq!(expected_links, &links[..]);
        }

        drop(store);

        let store = Store::open(config).await.unwrap();
        for (c, expected_data, expected_links) in values.iter() {
            let data = store.get(c).await.unwrap().unwrap();
            assert_eq!(expected_data, &data[..]);

            let links = store.get_links(c).await.unwrap().unwrap();
            assert_eq!(expected_links, &links[..]);
        }

        for i in 100..200 {
            let data = vec![i as u8; i * 16];
            let hash = Code::Sha2_256.digest(&data);
            let c = cid::Cid::new_v1(RAW, hash);

            let link_hash = Code::Sha2_256.digest(&[(i + 1) as u8; 64]);
            let link = cid::Cid::new_v1(RAW, link_hash);

            let links = [link];

            store.put(c, &data, links).await.unwrap();
            values.push((c, data, links));
        }

        for (c, expected_data, expected_links) in values.iter() {
            let data = store.get(c).await.unwrap().unwrap();
            assert_eq!(expected_data, &data[..]);

            let links = store.get_links(c).await.unwrap().unwrap();
            assert_eq!(expected_links, &links[..]);
        }
    }

    async fn test_store() -> anyhow::Result<(Store, TempDir)> {
        let dir = tempfile::tempdir()?;
        let rpc_client = RpcClientConfig::default();
        let config = Config {
            path: dir.path().into(),
            rpc_client,
            metrics: MetricsConfig::default(),
        };

        let store = Store::create(config).await?;
        Ok((store, dir))
    }

    #[tokio::test]
    async fn test_multiple_cids_same_hash() -> anyhow::Result<()> {
        let link1 = Cid::from_str("bafybeib4tddkl4oalrhe7q66rrz5dcpz4qwv5lmpstuqrls3djikw566y4")?;
        let link2 = Cid::from_str("QmcBphfXUFUNLcfAm31WEqYjrjEh19G5x4iAQANSK151DD")?;
        // some data with links
        let data = libipld::ipld!({
            "link1": link1,
            "link2": link2,
        });
        let mut blob = Vec::new();
        data.encode(IpldCodec::DagCbor, &mut blob)?;
        let hash = Code::Sha2_256.digest(&blob);
        let raw_cid = Cid::new_v1(IpldCodec::Raw.into(), hash);
        let cbor_cid = Cid::new_v1(IpldCodec::DagCbor.into(), hash);

        let (store, _dir) = test_store().await?;
        store.put(raw_cid, &blob, vec![]).await?;
        store.put(cbor_cid, &blob, vec![link1, link2]).await?;
        assert_eq!(store.get_links(&raw_cid).await?.unwrap().len(), 0);
        assert_eq!(store.get_links(&cbor_cid).await?.unwrap().len(), 2);

        let tx = store.inner.content.begin_ro_txn()?;
        let ids = Store::get_ids_for_hash(&tx, &hash)?;
        assert_eq!(ids.count(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn test_blob_by_hash() -> anyhow::Result<()> {
        let link1 = Cid::from_str("bafybeib4tddkl4oalrhe7q66rrz5dcpz4qwv5lmpstuqrls3djikw566y4")?;
        let link2 = Cid::from_str("QmcBphfXUFUNLcfAm31WEqYjrjEh19G5x4iAQANSK151DD")?;
        // some data with links
        let data = libipld::ipld!({
            "link1": link1,
            "link2": link2,
        });
        let mut expected = Vec::new();
        data.encode(IpldCodec::DagCbor, &mut expected)?;
        let hash = Code::Sha2_256.digest(&expected);
        let raw_cid = Cid::new_v1(IpldCodec::Raw.into(), hash);
        let cbor_cid = Cid::new_v1(IpldCodec::DagCbor.into(), hash);

        let (store, _dir) = test_store().await?;
        // we don't have it yet
        assert!(!store.has_blob_for_hash(&hash).await?);
        let actual = store.get_blob_by_hash(&hash).await?.map(|x| x.to_vec());
        assert_eq!(actual, None);

        store.put(raw_cid, &expected, vec![]).await?;
        assert!(store.has_blob_for_hash(&hash).await?);
        let actual = store.get_blob_by_hash(&hash).await?.map(|x| x.to_vec());
        assert_eq!(actual, Some(expected.clone()));

        store.put(cbor_cid, &expected, vec![link1, link2]).await?;
        assert!(store.has_blob_for_hash(&hash).await?);
        let actual = store.get_blob_by_hash(&hash).await?.map(|x| x.to_vec());
        assert_eq!(actual, Some(expected));
        Ok(())
    }
}
