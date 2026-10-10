//! The core's last-value mirrors (docs/design.md#the-last-value-mirror): one
//! cache per mirrored key space, filled by a subscriber and served by a
//! queryable on the same expression.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use zenoh::time::Timestamp;
use zenoh::Session;

/// A mirrored value.
pub struct Mirrored {
    pub payload: Vec<u8>,
    /// The sample's timestamp. The core's router stamps every sample that
    /// arrives without one, so this is `None` only for a publisher that
    /// reached the mirror without passing the router.
    pub stamp: Option<Timestamp>,
    /// When the mirror received it, on the monotonic clock.
    received: Instant,
    /// How old the value already was when it arrived. Near zero for a sample
    /// the router stamped on arrival. For a replay it is the time since the
    /// stamp its publisher set.
    age_on_arrival: Duration,
}

impl Mirrored {
    /// Seconds since the value was true: the time since it arrived, on the
    /// monotonic clock, plus its age on arrival.
    pub fn age(&self) -> Duration {
        self.received.elapsed() + self.age_on_arrival
    }
}

/// The last value per key of one mirrored key space.
pub type MirrorCache = Arc<Mutex<BTreeMap<String, Mirrored>>>;

/// Whether a sample stamped `new` replaces a value stamped `old`. An older
/// stamp never replaces a newer one, so a replayed value cannot overwrite a
/// live one however the two arrive. A missing stamp on either side replaces,
/// which is the order of arrival.
pub fn replaces(old: Option<&Timestamp>, new: Option<&Timestamp>) -> bool {
    match (old, new) {
        (Some(old), Some(new)) => new > old,
        _ => true,
    }
}

/// How old a value stamped `stamp` already is at `now`. Zero for a stamp at
/// or after `now`.
pub fn age_at(stamp: Option<&Timestamp>, now: SystemTime) -> Duration {
    stamp
        .and_then(|s| now.duration_since(s.get_time().to_system_time()).ok())
        .unwrap_or_default()
}

/// Mirrors a published key space into a last-value cache served by a
/// queryable, and returns the cache.
///
/// A late joiner then sees the current value without waiting for the next
/// publish: the current minute and date for the clock, every entity's state
/// for a subscriber or a bus read such as the MCP surface's `read_state`, and
/// a forecast that is published once a day.
///
/// The value with the newest stamp is kept. Every reply carries that stamp
/// and, in the attachment, the value's age in seconds as a decimal string.
/// The age counts on the mirror's monotonic clock from the moment the value
/// arrived, plus how old it was then, so a wall-clock step after arrival
/// does not change it. The SDK's `subscribe` feeds the age to `Freshness`.
pub async fn mirror(session: &Session, keyexpr: &'static str) -> Result<MirrorCache, String> {
    let cache: MirrorCache = Arc::default();
    let sub = session
        .declare_subscriber(keyexpr)
        .await
        .map_err(|e| format!("failed to subscribe to {keyexpr}: {e}"))?;
    let queryable = session
        .declare_queryable(keyexpr)
        .await
        .map_err(|e| format!("failed to declare {keyexpr} queryable: {e}"))?;
    {
        let cache = cache.clone();
        tokio::spawn(async move {
            while let Ok(sample) = sub.recv_async().await {
                let key = sample.key_expr().as_str();
                let mut cache = cache.lock().expect("mirror cache lock");
                match sample.kind() {
                    zenoh::sample::SampleKind::Put => {
                        let stamp = sample.timestamp().copied();
                        if !replaces(
                            cache.get(key).and_then(|m| m.stamp.as_ref()),
                            stamp.as_ref(),
                        ) {
                            continue;
                        }
                        cache.insert(
                            key.to_string(),
                            Mirrored {
                                payload: sample.payload().to_bytes().to_vec(),
                                age_on_arrival: age_at(stamp.as_ref(), SystemTime::now()),
                                stamp,
                                received: Instant::now(),
                            },
                        );
                    }
                    zenoh::sample::SampleKind::Delete => {
                        cache.remove(key);
                    }
                }
            }
        });
    }
    {
        let cache = cache.clone();
        tokio::spawn(async move {
            while let Ok(query) = queryable.recv_async().await {
                let entries: Vec<(String, Vec<u8>, Option<Timestamp>, f64)> = cache
                    .lock()
                    .expect("mirror cache lock")
                    .iter()
                    .filter(|(key, _)| super::intersects(&query, key))
                    .map(|(key, m)| {
                        (
                            key.clone(),
                            m.payload.clone(),
                            m.stamp,
                            m.age().as_secs_f64(),
                        )
                    })
                    .collect();
                for (key, payload, stamp, age_s) in entries {
                    let _ = query
                        .reply(key, payload)
                        .timestamp(stamp)
                        .attachment(age_s.to_string())
                        .await;
                }
            }
        });
    }
    Ok(cache)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zenoh::time::{TimestampId, NTP64};

    fn stamp_at(since_epoch: Duration, id: u8) -> Timestamp {
        Timestamp::new(
            NTP64::from(since_epoch),
            TimestampId::try_from(id).expect("non-zero id"),
        )
    }

    #[test]
    fn a_newer_stamp_replaces_and_an_older_or_equal_one_does_not() {
        let old = stamp_at(Duration::from_secs(100), 1);
        let new = stamp_at(Duration::from_secs(200), 1);
        assert!(replaces(Some(&old), Some(&new)));
        assert!(!replaces(Some(&new), Some(&old)));
        assert!(!replaces(Some(&new), Some(&new)));
    }

    #[test]
    fn a_missing_stamp_replaces_in_arrival_order() {
        let stamp = stamp_at(Duration::from_secs(100), 1);
        assert!(replaces(None, Some(&stamp)));
        assert!(replaces(Some(&stamp), None));
        assert!(replaces(None, None));
    }

    #[test]
    fn age_is_the_time_since_the_stamp_and_never_negative() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let past = stamp_at(Duration::from_secs(400), 1);
        let future = stamp_at(Duration::from_secs(1_100), 1);
        let age = age_at(Some(&past), now);
        assert!(
            age.abs_diff(Duration::from_mins(10)) < Duration::from_millis(1),
            "{age:?}"
        );
        assert_eq!(age_at(Some(&future), now), Duration::ZERO);
        assert_eq!(age_at(None, now), Duration::ZERO);
    }
}
