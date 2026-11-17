//! Replay after a core restart (docs/design.md#replay-after-a-core-restart).
//! The mirror lives in memory, so a core restart empties it. Once the
//! recorder is running, the core reads the newest recorded value of every
//! state series and publishes it again, stamped with the time it was
//! recorded. It then gives each arbiter back its family holds and its
//! arbitration document, and says the replay is done.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use zenoh::time::{Timestamp, TimestampId, NTP64};

use crate::bus::{self, HealthStatus};
use crate::grants::Grant;
use crate::supervisor::mirror::MirrorCache;
use crate::supervisor::Core;

/// The timestamp ID replays carry. It differs from every router's ID, which
/// is random, so a consumer can tell a replay from a sample the router
/// stamped on arrival.
pub const REPLAY_ID: u8 = 1;

/// How long the core waits for the recorder's answer.
const QUERY_TIMEOUT: Duration = Duration::from_secs(30);

/// How often the core checks whether the recorder is running.
const POLL: Duration = Duration::from_millis(250);

/// One entry of the recorder's `home/history/latest` reply.
#[derive(Debug, Deserialize, PartialEq)]
pub struct Latest {
    pub key: String,
    pub value: serde_json::Value,
    /// When the recorder received it, in integer µs UTC.
    pub ts: u64,
}

/// The `(room, entity)` pairs the applied grant table binds.
pub fn bound_entities(grants: &[Grant]) -> BTreeSet<(String, String)> {
    grants
        .iter()
        .flat_map(|g| &g.entities)
        .map(|e| (e.room.clone(), e.name.clone()))
        .collect()
}

/// The mirrors a replay checks, so it never overwrites a value published
/// since the core started.
pub struct Mirrors {
    pub state: MirrorCache,
    pub holds: MirrorCache,
    pub arbitration: MirrorCache,
}

/// The hold records to replay: each arbiter's arbitration document, and the
/// holds under that arbiter's own name, which are the family's. A unit's own
/// holds are not replayed. The unit restarts with the core and asserts its
/// holds again before it is ready, and its `ready` would not know to release
/// a hold the replay brought back after it started. Keys the mirror holds are
/// skipped.
pub fn holds_to_replay(recorded: Vec<Latest>, held: &BTreeSet<String>) -> Vec<Latest> {
    let arbiters: BTreeSet<String> = recorded
        .iter()
        .filter_map(|entry| entry.key.strip_prefix("home/arbitration/"))
        .filter(|unit| !unit.contains('/'))
        .map(str::to_string)
        .collect();
    recorded
        .into_iter()
        .filter(|entry| {
            let parts: Vec<&str> = entry.key.split('/').collect();
            let own = match parts.as_slice() {
                ["home", "arbitration", unit] | ["home", "hold", unit, _, _, _] => {
                    arbiters.contains(*unit)
                }
                _ => false,
            };
            own && !held.contains(&entry.key)
        })
        .collect()
}

/// The entries to replay: state keys whose entity is still bound, in the room
/// it is bound in, and which the mirror does not hold. An entity removed or
/// moved since it was recorded does not come back under its old key, and a
/// value published since the core started is never overwritten.
pub fn to_replay(
    latest: Vec<Latest>,
    bound: &BTreeSet<(String, String)>,
    held: &BTreeSet<String>,
) -> Vec<Latest> {
    latest
        .into_iter()
        .filter(|entry| {
            let parts: Vec<&str> = entry.key.split('/').collect();
            parts.len() >= 5
                && parts[0] == "home"
                && parts[1] == "state"
                && bound.contains(&(parts[2].to_string(), parts[3].to_string()))
                && !held.contains(&entry.key)
        })
        .collect()
}

/// The stamp a replayed value carries: the time it was recorded, with
/// [`REPLAY_ID`].
pub fn replay_stamp(ts_us: u64) -> Timestamp {
    Timestamp::new(
        NTP64::from(Duration::from_micros(ts_us)),
        TimestampId::try_from(REPLAY_ID).expect("non-zero id"),
    )
}

/// Waits for `recorder` to reach `running`, then replays once, and marks the
/// replay done either way. Gives up quietly when the supervisor shuts down
/// first. A failed read is logged and not retried: the house then starts as
/// it would without a recorder.
pub async fn replay_when_running(core: Arc<Core>, recorder: String, mirrors: Mirrors) {
    loop {
        if core.shutting_down() {
            return;
        }
        let running = core
            .health
            .lock()
            .expect("health map lock")
            .get(&recorder)
            .is_some_and(|h| h.status == HealthStatus::Running);
        if running {
            break;
        }
        tokio::time::sleep(POLL).await;
    }
    let keys = |cache: &MirrorCache| -> BTreeSet<String> {
        cache
            .lock()
            .expect("mirror cache lock")
            .keys()
            .cloned()
            .collect()
    };
    match read_latest(&core, bus::HISTORY_LATEST_KEY).await {
        Ok(latest) => {
            let bound = bound_entities(&core.world.lock().expect("world meta lock").grants);
            let held = keys(&mirrors.state);
            let entries = to_replay(latest, &bound, &held);
            let count = entries.len();
            for entry in entries {
                let _ = core
                    .session
                    .put(entry.key, entry.value.to_string())
                    .timestamp(replay_stamp(entry.ts))
                    .await;
            }
            println!("[homeostat] replayed {count} state values from {recorder}");
        }
        Err(reason) => {
            println!("[homeostat] no replay from {recorder}: {reason}");
        }
    }
    match read_latest(&core, bus::HISTORY_HOLDS_KEY).await {
        Ok(recorded) => {
            let mut held = keys(&mirrors.holds);
            held.extend(keys(&mirrors.arbitration));
            let entries = holds_to_replay(recorded, &held);
            let count = entries.len();
            for entry in entries {
                let _ = core
                    .session
                    .put(entry.key, entry.value.to_string())
                    .timestamp(replay_stamp(entry.ts))
                    .await;
            }
            println!("[homeostat] replayed {count} hold records from {recorder}");
        }
        Err(reason) => {
            println!("[homeostat] no hold replay from {recorder}: {reason}");
        }
    }
    core.replay_done().await;
}

async fn read_latest(core: &Core, key: &str) -> Result<Vec<Latest>, String> {
    let replies = core
        .session
        .get(key)
        .timeout(QUERY_TIMEOUT)
        .await
        .map_err(|e| format!("query failed: {e}"))?;
    let Ok(reply) = replies.recv_async().await else {
        return Err("no reply".to_string());
    };
    match reply.result() {
        Ok(sample) => serde_json::from_slice(&sample.payload().to_bytes())
            .map_err(|e| format!("malformed reply: {e}")),
        Err(err) => Err(format!(
            "error reply: {}",
            String::from_utf8_lossy(&err.payload().to_bytes())
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn latest(key: &str) -> Latest {
        Latest {
            key: key.to_string(),
            value: serde_json::json!(21.5),
            ts: 1_700_000_000_000_000,
        }
    }

    fn bound() -> BTreeSet<(String, String)> {
        [("kitchen".to_string(), "temp".to_string())].into()
    }

    #[test]
    fn a_bound_entity_absent_from_the_mirror_is_replayed() {
        let out = to_replay(
            vec![latest("home/state/kitchen/temp/temperature")],
            &bound(),
            &BTreeSet::new(),
        );
        assert_eq!(out, vec![latest("home/state/kitchen/temp/temperature")]);
    }

    #[test]
    fn a_key_the_mirror_holds_is_not_replayed() {
        let held = ["home/state/kitchen/temp/temperature".to_string()].into();
        let out = to_replay(
            vec![latest("home/state/kitchen/temp/temperature")],
            &bound(),
            &held,
        );
        assert!(out.is_empty());
    }

    #[test]
    fn a_removed_or_moved_entity_is_not_replayed() {
        let out = to_replay(
            vec![
                latest("home/state/kitchen/gone/temperature"),
                latest("home/state/hallway/temp/temperature"),
            ],
            &bound(),
            &BTreeSet::new(),
        );
        assert!(out.is_empty());
    }

    #[test]
    fn only_state_keys_are_replayed() {
        let out = to_replay(
            vec![
                latest("home/cmd/kitchen/temp/temperature"),
                latest("home/state/kitchen"),
            ],
            &bound(),
            &BTreeSet::new(),
        );
        assert!(out.is_empty());
    }

    #[test]
    fn the_replay_stamp_is_the_recorded_time_with_the_replay_id() {
        let stamp = replay_stamp(1_700_000_000_123_456);
        let since_epoch = stamp.get_time().to_duration();
        assert!(
            since_epoch.abs_diff(Duration::from_micros(1_700_000_000_123_456))
                < Duration::from_micros(1)
        );
        assert_eq!(*stamp.get_id(), TimestampId::try_from(REPLAY_ID).unwrap());
    }

    #[test]
    fn an_arbiters_own_records_are_replayed_and_a_units_holds_are_not() {
        let entry = |key: &str| Latest {
            key: key.to_string(),
            value: serde_json::json!({}),
            ts: 1,
        };
        let recorded = vec![
            entry("home/arbitration/arbiter"),
            entry("home/hold/arbiter/hallway/front_door/locked"),
            entry("home/hold/arbiter/hallway/front_door/auto_relock"),
            entry("home/hold/heating/utility/stove/on"),
            entry("home/hold/stray"),
        ];
        let held = BTreeSet::from(["home/hold/arbiter/hallway/front_door/auto_relock".to_string()]);
        let keys: Vec<String> = holds_to_replay(recorded, &held)
            .into_iter()
            .map(|e| e.key)
            .collect();
        assert_eq!(
            keys,
            [
                "home/arbitration/arbiter",
                "home/hold/arbiter/hallway/front_door/locked"
            ],
            "the unit's hold, the malformed key and the one the mirror holds stay out"
        );
    }
}
