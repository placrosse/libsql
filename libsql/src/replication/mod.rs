//! Utilities used when using a replicated version of libsql.

use std::path::PathBuf;
use std::sync::Arc;

pub use libsql_replication::frame::{Frame, FrameNo};
use libsql_replication::replicator::{Either, Replicator};
pub use libsql_replication::snapshot::SnapshotFile;

use libsql_replication::rpc::proxy::{
    query::Params, DescribeRequest, DescribeResult, ExecuteResults, Positional, Program,
    ProgramReq, Query, Step,
};
use tokio::sync::Mutex;

use crate::parser::Statement;
use crate::Result;

pub(crate) use connection::RemoteConnection;

use self::local_client::LocalClient;
use self::remote_client::RemoteClient;

pub(crate) mod client;
mod connection;
pub(crate) mod local_client;
pub(crate) mod remote_client;

/// A set of rames to be injected via `sync_frames`.
pub enum Frames {
    /// A set of frames, in increasing frame_no.
    Vec(Vec<Frame>),
    /// A stream of snapshot frames. The frames must be in reverse frame_no, and the pages
    /// deduplicated. The snapshot is expected to be a single commit unit.
    Snapshot(SnapshotFile),
}

#[derive(Clone)]
pub(crate) struct Writer {
    pub(crate) client: client::Client,
    pub(crate) replicator: Option<EmbeddedReplicator>,
}

impl Writer {
    pub(crate) async fn execute_program(
        &self,
        steps: Vec<Statement>,
        params: impl Into<Params>,
    ) -> anyhow::Result<ExecuteResults> {
        let mut params = Some(params.into());

        let steps = steps
            .into_iter()
            .map(|stmt| Step {
                query: Some(Query {
                    stmt: stmt.stmt,
                    // TODO(lucio): Pass params
                    params: Some(
                        params
                            .take()
                            .unwrap_or(Params::Positional(Positional::default())),
                    ),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .collect();

        self.client
            .execute_program(ProgramReq {
                client_id: self.client.client_id(),
                pgm: Some(Program { steps }),
            })
            .await
    }

    pub(crate) async fn describe(&self, stmt: impl Into<String>) -> anyhow::Result<DescribeResult> {
        let stmt = stmt.into();

        self.client
            .describe(DescribeRequest {
                client_id: self.client.client_id(),
                stmt,
            })
            .await
    }

    pub(crate) fn replicator(&self) -> Option<&EmbeddedReplicator> {
        self.replicator.as_ref()
    }
}

#[derive(Clone)]
pub(crate) struct EmbeddedReplicator {
    replicator: Arc<Mutex<Replicator<Either<RemoteClient, LocalClient>>>>,
}

impl EmbeddedReplicator {
    pub async fn with_remote(client: RemoteClient, db_path: PathBuf, auto_checkpoint: u32, encryption_key: Option<bytes::Bytes>) -> Self {
        let replicator = Arc::new(Mutex::new(
            Replicator::new(Either::Left(client), db_path, auto_checkpoint, |_| (), encryption_key)
                .await
                .unwrap(),
        ));

        Self { replicator }
    }

    pub async fn with_local(client: LocalClient, db_path: PathBuf, auto_checkpoint: u32, encryption_key: Option<bytes::Bytes>) -> Self {
        let replicator = Arc::new(Mutex::new(
            Replicator::new(Either::Right(client), db_path, auto_checkpoint, |_| (), encryption_key)
                .await
                .unwrap(),
        ));

        Self { replicator }
    }

    /// Returns the new replication index, and how many log entries have been synced
    pub async fn sync_oneshot(&self) -> Result<(FrameNo, usize)> {
        let mut replicator = self.replicator.lock().await;
        if !matches!(replicator.client_mut(), Either::Left(_)) {
            return Err(crate::errors::Error::Misuse(
                "Trying to replicate from HTTP, but this is a local replicator".into(),
            ));
        }

        // we force a handshake to get the most up to date replication index from the primary.
        replicator.force_handshake();

        let mut count_synced = 0;
        loop {
            match replicator.replicate().await {
                Err(libsql_replication::replicator::Error::Meta(
                    libsql_replication::meta::Error::LogIncompatible,
                )) => {
                    // The meta must have been marked as dirty, replicate again from scratch
                    // this time.
                    tracing::debug!("re-replicating database after LogIncompatible error");
                    replicator
                        .replicate()
                        .await
                        .map_err(|e| crate::Error::Replication(e.into()))?;
                }
                Err(e) => return Err(crate::Error::Replication(e.into())),
                Ok(n) => {
                    count_synced += n;
                    let Either::Left(client) = replicator.client_mut() else {
                        unreachable!()
                    };
                    let Some(primary_index) = client.last_handshake_replication_index() else {
                        break;
                    };
                    if replicator.current_commit_index() >= primary_index {
                        break;
                    }
                }
            }
        }

        Ok((replicator.current_commit_index(), count_synced))
    }

    pub async fn sync_frames(&self, frames: Frames) -> Result<FrameNo> {
        let mut replicator = self.replicator.lock().await;

        match replicator.client_mut() {
            Either::Right(c) => {
                c.load_frames(frames);
            }
            Either::Left(_) => {
                return Err(crate::errors::Error::Misuse(
                    "Trying to call sync_frames with an HTTP replicator".into(),
                ))
            }
        }
        replicator
            .replicate()
            .await
            .map_err(|e| crate::Error::Replication(e.into()))?;

        Ok(replicator.current_commit_index())
    }

    pub async fn flush(&self) -> Result<FrameNo> {
        let mut replicator = self.replicator.lock().await;
        replicator
            .flush()
            .await
            .map_err(|e| crate::Error::Replication(e.into()))?;
        Ok(replicator.current_commit_index())
    }

    pub async fn committed_frame_no(&self) -> Option<FrameNo> {
        todo!()
    }
}
