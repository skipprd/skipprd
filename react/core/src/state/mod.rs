use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::session::ThreadStore;

pub trait ThreadStateArtifact: Sized + Serialize + DeserializeOwned {
    const ARTIFACT_ID: &'static str;
    const SCHEMA_VERSION: u32;

    fn schema_version(&self) -> u32;
}

pub async fn load_thread_state_artifact<T: ThreadStateArtifact>(
    store: &ThreadStore,
    thread_id: &str,
) -> Option<T> {
    store.get_thread_artifact_typed::<T>(thread_id, T::ARTIFACT_ID).await
}

pub async fn save_thread_state_artifact<T: ThreadStateArtifact>(
    store: &ThreadStore,
    thread_id: &str,
    state: &T,
) -> Result<(), String> {
    store
        .put_thread_artifact_typed::<T>(thread_id, T::ARTIFACT_ID, state)
        .await
}

