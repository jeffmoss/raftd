use std::collections::BTreeMap;
use std::fmt::Debug;
use std::io::Cursor;
use std::ops::RangeBounds;
use std::path::Path;
use std::sync::Arc;

use byteorder::BigEndian;
use byteorder::ReadBytesExt;
use byteorder::WriteBytesExt;
use openraft::storage::RaftLogStorage;
use openraft::storage::RaftStateMachine;
use openraft::AnyError;
use openraft::EntryPayload;
use openraft::ErrorVerb;
use openraft::OptionalSend;
use openraft::RaftLogReader;
use openraft::RaftSnapshotBuilder;
use rocksdb::ColumnFamily;
use rocksdb::ColumnFamilyDescriptor;
// use rocksdb::DBCompressionType;
use rocksdb::Direction;
use rocksdb::Options;
use rocksdb::DB;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::RwLock;

use crate::protobuf::SetRequest;
use crate::protobuf::Response;
use crate::raft_types::*;
use crate::TypeConfig;

// The RocksDB key-value store is a lockable BTreeMap
pub type KeyValueStore = Arc<RwLock<BTreeMap<String, String>>>;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct StoredSnapshot {
    pub meta: SnapshotMeta,

    /// The data of the state machine at the time of this snapshot.
    pub data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct StateMachineStore {
    pub data: StateMachineData,

    /// snapshot index is not persisted in this example.
    ///
    /// It is only used as a suffix of snapshot id, and should be globally unique.
    /// In practice, using a timestamp in micro-second would be good enough.
    snapshot_idx: u64,

    /// State machine stores snapshot in db.
    db: Arc<DB>,
}

#[derive(Debug, Clone)]
pub struct StateMachineData {
    pub last_applied_log_id: Option<LogId>,

    pub last_membership: StoredMembership,

    /// State built from applying the raft logs
    pub kvs: KeyValueStore,
}

impl RaftSnapshotBuilder<TypeConfig> for StateMachineStore {
    async fn build_snapshot(&mut self) -> Result<Snapshot, StorageError> {
        let last_applied_log = self.data.last_applied_log_id;
        let last_membership = self.data.last_membership.clone();

        let kv_json = {
            let kvs = self.data.kvs.read().await;
            serde_json::to_vec(&*kvs).map_err(|e| StorageError::read_state_machine(&e))?
        };

        let snapshot_id = if let Some(last) = last_applied_log {
            format!("{}-{}-{}", last.leader_id, last.index, self.snapshot_idx)
        } else {
            format!("--{}", self.snapshot_idx)
        };

        let meta = SnapshotMeta {
            last_log_id: last_applied_log,
            last_membership,
            snapshot_id,
        };

        let snapshot = StoredSnapshot {
            meta: meta.clone(),
            data: kv_json.clone(),
        };

        self.set_current_snapshot_(snapshot)?;

        Ok(Snapshot {
            meta,
            snapshot: Box::new(Cursor::new(kv_json)),
        })
    }
}

impl StateMachineStore {
    async fn new(db: Arc<DB>) -> Result<StateMachineStore, StorageError> {
        let mut sm = Self {
            data: StateMachineData {
                last_applied_log_id: None,
                last_membership: Default::default(),
                kvs: Arc::new(Default::default()),
            },
            snapshot_idx: 0,
            db,
        };

        let snapshot = sm.get_current_snapshot_()?;
        if let Some(snap) = snapshot {
            sm.update_state_machine_(snap).await?;
        }

        Ok(sm)
    }

    async fn update_state_machine_(&mut self, snapshot: StoredSnapshot) -> Result<(), StorageError> {
        let kvs: BTreeMap<String, String> = serde_json::from_slice(&snapshot.data)
            .map_err(|e| StorageError::read_snapshot(Some(snapshot.meta.signature()), &e))?;

        self.data.last_applied_log_id = snapshot.meta.last_log_id;
        self.data.last_membership = snapshot.meta.last_membership.clone();
        let mut x = self.data.kvs.write().await;
        *x = kvs;

        Ok(())
    }

    fn get_current_snapshot_(&self) -> StorageResult<Option<StoredSnapshot>> {
        Ok(self
            .db
            .get_cf(self.store(), b"snapshot")
            .map_err(|e| StorageError::read(&e))?
            .and_then(|v| serde_json::from_slice(&v).ok()))
    }

    fn set_current_snapshot_(&self, snap: StoredSnapshot) -> StorageResult<()> {
        self.db
            .put_cf(self.store(), b"snapshot", serde_json::to_vec(&snap).unwrap().as_slice())
            .map_err(|e| StorageError::write_snapshot(Some(snap.meta.signature()), &e))?;
        self.flush(ErrorSubject::Snapshot(Some(snap.meta.signature())), ErrorVerb::Write)?;
        Ok(())
    }

    fn flush(&self, subject: ErrorSubject, verb: ErrorVerb) -> Result<(), StorageError> {
        self.db.flush_wal(true).map_err(|e| StorageError::new(subject, verb, AnyError::new(&e)))?;
        Ok(())
    }

    fn store(&self) -> &ColumnFamily {
        self.db.cf_handle("store").unwrap()
    }
}

impl RaftStateMachine<TypeConfig> for StateMachineStore {
    type SnapshotBuilder = Self;

    async fn applied_state(&mut self) -> Result<(Option<LogId>, StoredMembership), StorageError> {
        Ok((self.data.last_applied_log_id, self.data.last_membership.clone()))
    }

    async fn apply<I>(&mut self, entries: I) -> Result<Vec<Response>, StorageError>
    where
        I: IntoIterator<Item = Entry> + OptionalSend,
        I::IntoIter: OptionalSend,
    {
        let entries = entries.into_iter();
        let mut replies = Vec::with_capacity(entries.size_hint().0);

        for ent in entries {
            self.data.last_applied_log_id = Some(ent.log_id);

            let mut resp_value = None;

            match ent.payload {
                EntryPayload::Blank => {}
                EntryPayload::Normal(req) => match req {
                    SetRequest { key, value } => {
                        resp_value = Some(value.clone());

                        let mut st = self.data.kvs.write().await;
                        st.insert(key, value);
                    }
                },
                EntryPayload::Membership(mem) => {
                    self.data.last_membership = StoredMembership::new(Some(ent.log_id), mem);
                }
            }

            replies.push(Response { value: resp_value });
        }
        Ok(replies)
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        self.snapshot_idx += 1;
        self.clone()
    }

    async fn begin_receiving_snapshot(&mut self) -> Result<Box<Cursor<Vec<u8>>>, StorageError> {
        Ok(Box::new(Cursor::new(Vec::new())))
    }

    async fn install_snapshot(&mut self, meta: &SnapshotMeta, snapshot: Box<SnapshotData>) -> Result<(), StorageError> {
        let new_snapshot = StoredSnapshot {
            meta: meta.clone(),
            data: snapshot.into_inner(),
        };

        self.update_state_machine_(new_snapshot.clone()).await?;

        self.set_current_snapshot_(new_snapshot)?;

        Ok(())
    }

    async fn get_current_snapshot(&mut self) -> Result<Option<Snapshot>, StorageError> {
        let x = self.get_current_snapshot_()?;
        Ok(x.map(|s| Snapshot {
            meta: s.meta.clone(),
            snapshot: Box::new(Cursor::new(s.data.clone())),
        }))
    }
}

#[derive(Debug, Clone)]
pub struct LogStore {
    db: Arc<DB>,
}
type StorageResult<T> = Result<T, StorageError>;

/// converts an id to a byte vector for storing in the database.
/// Note that we're using big endian encoding to ensure correct sorting of keys
fn id_to_bin(id: u64) -> Vec<u8> {
    let mut buf = Vec::with_capacity(8);
    buf.write_u64::<BigEndian>(id).unwrap();
    buf
}

fn bin_to_id(buf: &[u8]) -> u64 {
    (&buf[0..8]).read_u64::<BigEndian>().unwrap()
}

impl LogStore {
    fn store(&self) -> &ColumnFamily {
        self.db.cf_handle("store").unwrap()
    }

    fn logs(&self) -> &ColumnFamily {
        self.db.cf_handle("logs").unwrap()
    }

    fn flush(&self, subject: ErrorSubject, verb: ErrorVerb) -> Result<(), StorageError> {
        self.db.flush_wal(true).map_err(|e| StorageError::new(subject, verb, AnyError::new(&e)))?;
        Ok(())
    }

    fn get_last_purged_(&self) -> StorageResult<Option<LogId>> {
        Ok(self
            .db
            .get_cf(self.store(), b"last_purged_log_id")
            .map_err(|e| StorageError::read(&e))?
            .and_then(|v| serde_json::from_slice(&v).ok()))
    }

    fn set_last_purged_(&self, log_id: LogId) -> StorageResult<()> {
        self.db
            .put_cf(
                self.store(),
                b"last_purged_log_id",
                serde_json::to_vec(&log_id).unwrap().as_slice(),
            )
            .map_err(|e| StorageError::write(&e))?;

        self.flush(ErrorSubject::Store, ErrorVerb::Write)?;
        Ok(())
    }

    fn set_committed_(&self, committed: &Option<LogId>) -> Result<(), StorageError> {
        let json = serde_json::to_vec(committed).unwrap();

        self.db.put_cf(self.store(), b"committed", json).map_err(|e| StorageError::write(&e))?;

        self.flush(ErrorSubject::Store, ErrorVerb::Write)?;
        Ok(())
    }

    fn get_committed_(&self) -> StorageResult<Option<LogId>> {
        Ok(self
            .db
            .get_cf(self.store(), b"committed")
            .map_err(|e| StorageError::read(&e))?
            .and_then(|v| serde_json::from_slice(&v).ok()))
    }

    fn set_vote_(&self, vote: &Vote) -> StorageResult<()> {
        self.db
            .put_cf(self.store(), b"vote", serde_json::to_vec(vote).unwrap())
            .map_err(|e| StorageError::write_vote(&e))?;

        self.flush(ErrorSubject::Vote, ErrorVerb::Write)?;
        Ok(())
    }

    fn get_vote_(&self) -> StorageResult<Option<Vote>> {
        Ok(self
            .db
            .get_cf(self.store(), b"vote")
            .map_err(|e| StorageError::write_vote(&e))?
            .and_then(|v| serde_json::from_slice(&v).ok()))
    }
}

impl RaftLogReader<TypeConfig> for LogStore {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + OptionalSend>(
        &mut self,
        range: RB,
    ) -> StorageResult<Vec<Entry>> {
        let start = match range.start_bound() {
            std::ops::Bound::Included(x) => id_to_bin(*x),
            std::ops::Bound::Excluded(x) => id_to_bin(*x + 1),
            std::ops::Bound::Unbounded => id_to_bin(0),
        };
        self.db
            .iterator_cf(self.logs(), rocksdb::IteratorMode::From(&start, Direction::Forward))
            .map(|res| {
                let (id, val) = res.unwrap();
                let entry: StorageResult<Entry> = serde_json::from_slice(&val).map_err(|e| StorageError::read_logs(&e));
                let id = bin_to_id(&id);

                assert_eq!(Ok(id), entry.as_ref().map(|e| e.log_id.index));
                (id, entry)
            })
            .take_while(|(id, _)| range.contains(id))
            .map(|x| x.1)
            .collect()
    }

    async fn read_vote(&mut self) -> Result<Option<Vote>, StorageError> {
        self.get_vote_()
    }
}

impl RaftLogStorage<TypeConfig> for LogStore {
    type LogReader = Self;

    async fn get_log_state(&mut self) -> StorageResult<LogState> {
        let last = self.db.iterator_cf(self.logs(), rocksdb::IteratorMode::End).next().and_then(|res| {
            let (_, ent) = res.unwrap();
            Some(serde_json::from_slice::<Entry>(&ent).ok()?.log_id)
        });

        let last_purged_log_id = self.get_last_purged_()?;

        let last_log_id = match last {
            None => last_purged_log_id,
            Some(x) => Some(x),
        };
        Ok(LogState {
            last_purged_log_id,
            last_log_id,
        })
    }

    async fn save_committed(&mut self, _committed: Option<LogId>) -> Result<(), StorageError> {
        self.set_committed_(&_committed)?;
        Ok(())
    }

    async fn read_committed(&mut self) -> Result<Option<LogId>, StorageError> {
        let c = self.get_committed_()?;
        Ok(c)
    }

    #[tracing::instrument(level = "trace", skip(self))]
    async fn save_vote(&mut self, vote: &Vote) -> Result<(), StorageError> {
        self.set_vote_(vote)
    }

    #[tracing::instrument(level = "trace", skip_all)]
    async fn append<I>(&mut self, entries: I, callback: IOFlushed) -> StorageResult<()>
    where
        I: IntoIterator<Item = Entry> + Send,
        I::IntoIter: Send,
    {
        for entry in entries {
            let id = id_to_bin(entry.log_id.index);
            assert_eq!(bin_to_id(&id), entry.log_id.index);
            self.db
                .put_cf(
                    self.logs(),
                    id,
                    serde_json::to_vec(&entry).map_err(|e| StorageError::write_logs(&e))?,
                )
                .map_err(|e| StorageError::write_logs(&e))?;
        }

        callback.io_completed(Ok(()));

        Ok(())
    }

    #[tracing::instrument(level = "debug", skip(self))]
    async fn truncate(&mut self, log_id: LogId) -> StorageResult<()> {
        tracing::debug!("delete_log: [{:?}, +oo)", log_id);

        let from = id_to_bin(log_id.index);
        let to = id_to_bin(0xff_ff_ff_ff_ff_ff_ff_ff);
        self.db.delete_range_cf(self.logs(), &from, &to).map_err(|e| StorageError::write_logs(&e))
    }

    #[tracing::instrument(level = "debug", skip(self))]
    async fn purge(&mut self, log_id: LogId) -> Result<(), StorageError> {
        tracing::debug!("delete_log: [0, {:?}]", log_id);

        self.set_last_purged_(log_id)?;
        let from = id_to_bin(0);
        let to = id_to_bin(log_id.index + 1);
        self.db.delete_range_cf(self.logs(), &from, &to).map_err(|e| StorageError::write_logs(&e))
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        self.clone()
    }
}

pub(crate) async fn new_storage<P: AsRef<Path>>(db_path: P) -> (LogStore, StateMachineStore) {
    let mut db_opts = Options::default();
    db_opts.create_missing_column_families(true);
    db_opts.create_if_missing(true);
    // db_opts.set_wal_compression_type(DBCompressionType::Lz4);

    let store = ColumnFamilyDescriptor::new("store", Options::default());
    let logs = ColumnFamilyDescriptor::new("logs", Options::default());

    let db = DB::open_cf_descriptors(&db_opts, db_path, vec![store, logs]).unwrap();
    let db = Arc::new(db);

    let log_store = LogStore { db: db.clone() };
    let sm_store = StateMachineStore::new(db).await.unwrap();

    (log_store, sm_store)
}
