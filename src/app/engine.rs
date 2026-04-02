use super::events::Event;
use crate::domain::trading::snapshot::WorldSnapshot;
use crate::domain::trading::world::World;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

pub async fn run_shared_cached(
    world: Arc<Mutex<World>>,
    snapshot: Arc<RwLock<WorldSnapshot>>,
    mut rx: tokio::sync::mpsc::Receiver<Event>,
) {
    while let Some(ev) = rx.recv().await {
        let next_snapshot = {
            let mut world = world.lock().await;
            world.apply_event(ev);
            world.refresh_dirty_markets().await;
            world.snapshot()
        };

        *snapshot.write().await = next_snapshot;
    }
}
