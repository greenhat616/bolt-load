//! Every way to hand an adapter implemented outside the crate to the builder.

use std::sync::Arc;

use bolt_load::{
    adapter::{
        AdapterError, AnyAdapter, AnyBytesStream, BoltLoadAdapter, BoltLoadAdapterMeta,
        DynBoltLoadAdapter,
    },
    task::TaskBuilder,
};

const SIZE: u64 = 42;

struct FixedMeta;

impl BoltLoadAdapter for FixedMeta {
    async fn retrieve_meta(&self) -> Result<BoltLoadAdapterMeta, AdapterError> {
        Ok(BoltLoadAdapterMeta {
            content_size: SIZE,
            filename: None,
        })
    }

    async fn full_stream(&self) -> Result<AnyBytesStream, AdapterError> {
        Ok(Box::pin(futures::stream::empty()))
    }
}

async fn content_size(mut builder: TaskBuilder) -> u64 {
    builder.retrieve_meta().await.unwrap().content_size
}

#[tokio::test]
async fn concrete_adapter() {
    assert_eq!(
        content_size(TaskBuilder::default().adapter(FixedMeta)).await,
        SIZE
    );
}

#[tokio::test]
async fn boxed_erased_adapter() {
    let boxed: Box<AnyAdapter> = DynBoltLoadAdapter::new_box(FixedMeta);
    assert_eq!(
        content_size(TaskBuilder::default().adapter(boxed)).await,
        SIZE
    );
}

#[tokio::test]
async fn shared_erased_adapter() {
    let shared: Arc<AnyAdapter> = DynBoltLoadAdapter::new_arc(FixedMeta);
    let builder = TaskBuilder::default().adapter_arc(shared.clone());
    assert_eq!(content_size(builder).await, SIZE);
    assert!(!shared.is_range_stream_available().await);
}
