use std::pin::Pin;

use bytes::Bytes;
use chrono::{DateTime, NaiveDateTime, Utc};
use futures::TryStreamExt;
use libsql_replication::frame::Frame;
use libsql_replication::replicator::{map_frame_err, Error, ReplicatorClient};
use libsql_replication::rpc::replication::replication_log_client::ReplicationLogClient;
use libsql_replication::rpc::replication::{
    verify_session_token, HelloRequest, HelloResponse, LogOffset, NAMESPACE_METADATA_KEY,
    SESSION_TOKEN_KEY,
};
use tokio_stream::{Stream, StreamExt};
use tonic::metadata::{AsciiMetadataValue, BinaryMetadataValue};
use tonic::transport::Channel;
use tonic::Request;

use crate::connection::config::DatabaseConfig;
use crate::metrics::{
    REPLICATION_LATENCY, REPLICATION_LATENCY_CACHE_MISS, REPLICATION_LATENCY_OUT_OF_SYNC,
};
use crate::namespace::meta_store::MetaStoreHandle;
use crate::namespace::NamespaceName;
use crate::replication::FrameNo;

pub struct Client {
    client: ReplicationLogClient<Channel>,
    namespace: NamespaceName,
    session_token: Option<Bytes>,
    meta_store_handle: MetaStoreHandle,
    // the primary current replication index, as reported by the last handshake
    pub primary_replication_index: Option<FrameNo>,
}

impl Client {
    pub async fn new(
        namespace: NamespaceName,
        client: ReplicationLogClient<Channel>,
        meta_store_handle: MetaStoreHandle,
    ) -> crate::Result<Self> {
        Ok(Self {
            namespace,
            client,
            session_token: None,
            meta_store_handle,
            primary_replication_index: None,
        })
    }

    fn make_request<T>(&self, msg: T) -> Request<T> {
        let mut req = Request::new(msg);
        req.metadata_mut().insert_bin(
            NAMESPACE_METADATA_KEY,
            BinaryMetadataValue::from_bytes(self.namespace.as_slice()),
        );

        if let Some(token) = self.session_token.clone() {
            // SAFETY: we always check the session token
            req.metadata_mut().insert(SESSION_TOKEN_KEY, unsafe {
                AsciiMetadataValue::from_shared_unchecked(token)
            });
        }

        req
    }

    pub(crate) fn reset_token(&mut self) {
        self.session_token = None;
    }
}

#[async_trait::async_trait]
impl ReplicatorClient for Client {
    type FrameStream = Pin<Box<dyn Stream<Item = Result<Frame, Error>> + Send + 'static>>;

    #[tracing::instrument(skip(self))]
    async fn handshake(&mut self) -> Result<Option<HelloResponse>, Error> {
        tracing::info!("Attempting to perform handshake with primary.");
        let req = self.make_request(HelloRequest::new());
        let resp = self.client.hello(req).await?;
        let hello = resp.into_inner();
        verify_session_token(&hello.session_token).map_err(Error::Client)?;
        self.primary_replication_index = hello.current_replication_index;
        self.session_token.replace(hello.session_token.clone());

        if let Some(config) = &hello.config {
            self.meta_store_handle
                .store(DatabaseConfig::from(config))
                .await
                .map_err(|e| Error::Internal(e.into()))?;

            tracing::debug!("replica config has been updated");
        } else {
            tracing::debug!("no config passed in handshake");
        }

        tracing::trace!("handshake completed");

        Ok(Some(hello))
    }

    async fn next_frames(&mut self, next_offset: FrameNo) -> Result<Self::FrameStream, Error> {
        dbg!(next_offset);
        let offset = LogOffset { next_offset };
        let req = self.make_request(offset);
        let stream = self
            .client
            .log_entries(req)
            .await?
            .into_inner()
            .inspect_ok(|f| {
                match f.timestamp {
                    Some(ts_millis) => {
                        if let Some(ts_millis) = NaiveDateTime::from_timestamp_millis(ts_millis) {
                            let commited_at =
                                DateTime::<Utc>::from_naive_utc_and_offset(ts_millis, Utc);
                            let lat = Utc::now() - commited_at;
                            match lat.to_std() {
                                Ok(lat) => {
                                    // we can record negative values if the clocks are out-of-sync. There is not
                                    // point in recording those values.
                                    REPLICATION_LATENCY.record(lat);
                                }
                                Err(_) => {
                                    REPLICATION_LATENCY_OUT_OF_SYNC.increment(1);
                                }
                            }
                        }
                    }
                    None => REPLICATION_LATENCY_CACHE_MISS.increment(1),
                }
            })
            .map(map_frame_err);

        Ok(Box::pin(stream))
    }

    async fn snapshot(&mut self, next_offset: FrameNo) -> Result<Self::FrameStream, Error> {
        let offset = LogOffset { next_offset };
        let req = self.make_request(offset);
        let stream = self
            .client
            .snapshot(req)
            .await?
            .into_inner()
            .map(map_frame_err);
        Ok(Box::pin(stream))
    }
}
