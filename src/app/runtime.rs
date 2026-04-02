use super::engine;
use super::events::Event;
use crate::domain::trading::snapshot::WorldSnapshot;
use crate::domain::trading::world::World;
use crate::infra::market_data;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

pub struct BotRuntime {
    snapshot_cache: Arc<RwLock<WorldSnapshot>>,
    tx: mpsc::Sender<Event>,
    engine_handle: JoinHandle<()>,
    live_handles: Vec<JoinHandle<()>>,
    timer_handle: JoinHandle<()>,
}

impl BotRuntime {
    pub fn spawn() -> Self {
        let (tx, rx) = mpsc::channel(1024);
        let seeded = World::new();
        let snapshot_cache = Arc::new(RwLock::new(seeded.snapshot()));
        let world = Arc::new(Mutex::new(seeded));

        let engine_handle = tokio::spawn(engine::run_shared_cached(
            world.clone(),
            snapshot_cache.clone(),
            rx,
        ));
        let live_handles = market_data::spawn_live_feeds(world, tx.clone());
        let timer_handle = tokio::spawn(live_timers(tx.clone()));

        Self {
            snapshot_cache,
            tx,
            engine_handle,
            live_handles,
            timer_handle,
        }
    }

    pub fn snapshot_cache(&self) -> Arc<RwLock<WorldSnapshot>> {
        self.snapshot_cache.clone()
    }

    pub async fn shutdown(self) {
        for handle in self.live_handles {
            handle.abort();
        }
        self.timer_handle.abort();
        drop(self.tx);
        let _ = self.engine_handle.await;
    }
}

async fn live_timers(tx: mpsc::Sender<Event>) {
    let mut fast = tokio::time::interval(std::time::Duration::from_millis(250));
    let mut slow = tokio::time::interval(std::time::Duration::from_secs(2));
    fast.set_missed_tick_behavior(MissedTickBehavior::Skip);
    slow.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = fast.tick() => {
                if tx.send(Event::TimerFast).await.is_err() {
                    return;
                }
            }
            _ = slow.tick() => {
                if tx.send(Event::TimerSlow).await.is_err() {
                    return;
                }
            }
        }
    }
}
