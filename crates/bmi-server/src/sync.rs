//! Index git sync (filled out in a later task; the status type is needed by the API now).
use serde::Serialize;

#[derive(Clone, Debug, Default, Serialize)]
pub struct SyncStatus {
    pub running: bool,
    pub last_error: Option<String>,
}

pub struct IndexSync {
    status: parking_lot::Mutex<SyncStatus>,
}

impl IndexSync {
    pub fn status(&self) -> SyncStatus {
        self.status.lock().clone()
    }
}
