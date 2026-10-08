//! `jc:app/sql` and `jc:app/blob` as the host links them (ADR-N-044 §2.2, AP-144, AP-145). Every
//! call goes to the shard's [`Storage`] with the App the instance runs for, so a component can
//! name no other App's schema or prefix. T-3340 implements the stores; until a shard is given
//! them, every call answers `unavailable`.

use std::future::Future;
use std::pin::Pin;

use wasmtime::component::{HasSelf, Linker};

use crate::host::State;
use crate::placement::Placed;

wasmtime::component::bindgen!({
    path: "wit",
    world: "jc:app/app-host",
    imports: { default: async },
    additional_derives: [PartialEq],
});

pub use jc::app::blob::{Error as BlobError, Method};
pub use jc::app::sql::{Error as SqlError, Rows, Value};

pub type Answer<'a, T, E> = Pin<Box<dyn Future<Output = Result<T, E>> + Send + 'a>>;

/// A shard's stores, answering for the App given.
pub trait Storage: Send + Sync {
    fn query<'a>(
        &'a self,
        app: &'a Placed,
        statement: String,
        params: Vec<Value>,
    ) -> Answer<'a, Rows, SqlError>;
    fn execute<'a>(
        &'a self,
        app: &'a Placed,
        statement: String,
        params: Vec<Value>,
    ) -> Answer<'a, u64, SqlError>;
    fn get<'a>(&'a self, app: &'a Placed, key: String) -> Answer<'a, Vec<u8>, BlobError>;
    fn put<'a>(
        &'a self,
        app: &'a Placed,
        key: String,
        data: Vec<u8>,
        content_type: Option<String>,
    ) -> Answer<'a, (), BlobError>;
    fn list<'a>(&'a self, app: &'a Placed, prefix: String) -> Answer<'a, Vec<String>, BlobError>;
    fn delete<'a>(&'a self, app: &'a Placed, key: String) -> Answer<'a, (), BlobError>;
    fn presign<'a>(
        &'a self,
        app: &'a Placed,
        key: String,
        method: Method,
        expires: u32,
    ) -> Answer<'a, String, BlobError>;
}

/// A shard with no stores configured: every call says so.
pub struct Unconfigured;

const NONE: &str = "this shard has no storage configured";

impl Storage for Unconfigured {
    fn query<'a>(&'a self, _: &'a Placed, _: String, _: Vec<Value>) -> Answer<'a, Rows, SqlError> {
        Box::pin(async { Err(SqlError::Unavailable(NONE.into())) })
    }
    fn execute<'a>(&'a self, _: &'a Placed, _: String, _: Vec<Value>) -> Answer<'a, u64, SqlError> {
        Box::pin(async { Err(SqlError::Unavailable(NONE.into())) })
    }
    fn get<'a>(&'a self, _: &'a Placed, _: String) -> Answer<'a, Vec<u8>, BlobError> {
        Box::pin(async { Err(BlobError::Unavailable(NONE.into())) })
    }
    fn put<'a>(
        &'a self,
        _: &'a Placed,
        _: String,
        _: Vec<u8>,
        _: Option<String>,
    ) -> Answer<'a, (), BlobError> {
        Box::pin(async { Err(BlobError::Unavailable(NONE.into())) })
    }
    fn list<'a>(&'a self, _: &'a Placed, _: String) -> Answer<'a, Vec<String>, BlobError> {
        Box::pin(async { Err(BlobError::Unavailable(NONE.into())) })
    }
    fn delete<'a>(&'a self, _: &'a Placed, _: String) -> Answer<'a, (), BlobError> {
        Box::pin(async { Err(BlobError::Unavailable(NONE.into())) })
    }
    fn presign<'a>(
        &'a self,
        _: &'a Placed,
        _: String,
        _: Method,
        _: u32,
    ) -> Answer<'a, String, BlobError> {
        Box::pin(async { Err(BlobError::Unavailable(NONE.into())) })
    }
}

impl jc::app::sql::Host for State {
    async fn query(&mut self, statement: String, params: Vec<Value>) -> Result<Rows, SqlError> {
        let storage = self.storage.clone();
        storage.query(&self.app, statement, params).await
    }
    async fn execute(&mut self, statement: String, params: Vec<Value>) -> Result<u64, SqlError> {
        let storage = self.storage.clone();
        storage.execute(&self.app, statement, params).await
    }
}

impl jc::app::blob::Host for State {
    async fn get(&mut self, key: String) -> Result<Vec<u8>, BlobError> {
        let storage = self.storage.clone();
        storage.get(&self.app, key).await
    }
    async fn put(
        &mut self,
        key: String,
        data: Vec<u8>,
        content_type: Option<String>,
    ) -> Result<(), BlobError> {
        let storage = self.storage.clone();
        storage.put(&self.app, key, data, content_type).await
    }
    async fn list_keys(&mut self, prefix: String) -> Result<Vec<String>, BlobError> {
        let storage = self.storage.clone();
        storage.list(&self.app, prefix).await
    }
    async fn delete(&mut self, key: String) -> Result<(), BlobError> {
        let storage = self.storage.clone();
        storage.delete(&self.app, key).await
    }
    async fn presign(
        &mut self,
        key: String,
        method: Method,
        expires_seconds: u32,
    ) -> Result<String, BlobError> {
        let storage = self.storage.clone();
        storage
            .presign(&self.app, key, method, expires_seconds)
            .await
    }
}

pub fn add_to_linker(linker: &mut Linker<State>) -> wasmtime::Result<()> {
    AppHost::add_to_linker::<State, HasSelf<State>>(linker, |state| state)
}

/// A shard's stores: the apps database and the bucket, each optional, a missing one answering
/// `unavailable`.
pub struct Stores {
    pub sql: Option<crate::sql::PgStore>,
    pub blob: Option<crate::blob::S3Blob>,
}

impl Storage for Stores {
    fn query<'a>(
        &'a self,
        app: &'a Placed,
        statement: String,
        params: Vec<Value>,
    ) -> Answer<'a, Rows, SqlError> {
        Box::pin(async move {
            let store = self
                .sql
                .as_ref()
                .ok_or_else(|| SqlError::Unavailable(NONE.into()))?;
            let (rows, _) = store
                .run(app, statement, params, crate::sql::Kind::Read)
                .await?;
            Ok(rows.unwrap_or(Rows {
                columns: Vec::new(),
                values: Vec::new(),
            }))
        })
    }
    fn execute<'a>(
        &'a self,
        app: &'a Placed,
        statement: String,
        params: Vec<Value>,
    ) -> Answer<'a, u64, SqlError> {
        Box::pin(async move {
            let store = self
                .sql
                .as_ref()
                .ok_or_else(|| SqlError::Unavailable(NONE.into()))?;
            let (_, changed) = store
                .run(app, statement, params, crate::sql::Kind::Write)
                .await?;
            Ok(changed)
        })
    }
    fn get<'a>(&'a self, app: &'a Placed, key: String) -> Answer<'a, Vec<u8>, BlobError> {
        Box::pin(async move {
            self.blob
                .as_ref()
                .ok_or_else(|| BlobError::Unavailable(NONE.into()))?
                .get(app, &key)
                .await
        })
    }
    fn put<'a>(
        &'a self,
        app: &'a Placed,
        key: String,
        data: Vec<u8>,
        content_type: Option<String>,
    ) -> Answer<'a, (), BlobError> {
        Box::pin(async move {
            self.blob
                .as_ref()
                .ok_or_else(|| BlobError::Unavailable(NONE.into()))?
                .put(app, &key, data, content_type)
                .await
        })
    }
    fn list<'a>(&'a self, app: &'a Placed, prefix: String) -> Answer<'a, Vec<String>, BlobError> {
        Box::pin(async move {
            self.blob
                .as_ref()
                .ok_or_else(|| BlobError::Unavailable(NONE.into()))?
                .list(app, &prefix)
                .await
        })
    }
    fn delete<'a>(&'a self, app: &'a Placed, key: String) -> Answer<'a, (), BlobError> {
        Box::pin(async move {
            self.blob
                .as_ref()
                .ok_or_else(|| BlobError::Unavailable(NONE.into()))?
                .delete(app, &key)
                .await
        })
    }
    fn presign<'a>(
        &'a self,
        app: &'a Placed,
        key: String,
        method: Method,
        expires: u32,
    ) -> Answer<'a, String, BlobError> {
        Box::pin(async move {
            self.blob
                .as_ref()
                .ok_or_else(|| BlobError::Unavailable(NONE.into()))?
                .presign(app, &key, method, expires)
        })
    }
}
