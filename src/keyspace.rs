use std::fmt;

/// `home/{class}/...` — the classes the core owns.
pub const CLASSES: &[&str] = &[
    "state",
    "cmd",
    "arbiter",
    // A series' future, keyed like its present: same room/entity/aspect, so
    // a forecast is the same series extended forward (docs/design.md,
    // Forecasts). The core no more knows what one means than it knows what
    // `motion` means.
    "forecast",
    "config",
    "meta",
    "health",
    "clock",
    "history",
    "discovery",
];

/// The classes addressed per entity — `home/{class}/{room}/{entity}/{aspect}`
/// — rather than by some other shape under the class. `forecast` is one of
/// them and takes a sixth segment, its source; see `check_schema`.
const ENTITY_ADDRESSED: &[&str] = &["state", "cmd", "arbiter", "forecast"];

/// Reserved pseudo-rooms for non-spatial entities.
pub const PSEUDO_ROOMS: &[&str] = &["global", "person"];

/// Words that may not be used as room or zone names (pseudo-rooms are the
/// exception for entity rooms).
pub fn is_reserved_word(word: &str) -> bool {
    word == "home" || CLASSES.contains(&word) || PSEUDO_ROOMS.contains(&word)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Literal(String),
    /// `*` — exactly one segment.
    Any,
    /// `**` — zero or more segments.
    AnyRec,
    /// `{room}` — expanded per bound entity, entity-binding units only.
    RoomTemplate,
    /// `{entity}` — expanded per bound entity, entity-binding units only.
    EntityTemplate,
}

impl fmt::Display for Segment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Segment::Literal(s) => f.write_str(s),
            Segment::Any => f.write_str("*"),
            Segment::AnyRec => f.write_str("**"),
            Segment::RoomTemplate => f.write_str("{room}"),
            Segment::EntityTemplate => f.write_str("{entity}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyExpr(pub Vec<Segment>);

impl KeyExpr {
    pub fn parse(raw: &str) -> Result<KeyExpr, String> {
        if raw.is_empty() {
            return Err("key is empty".to_string());
        }
        let mut segments = Vec::new();
        for seg in raw.split('/') {
            segments.push(match seg {
                "" => return Err(format!("\"{raw}\" has an empty segment")),
                "*" => Segment::Any,
                "**" => Segment::AnyRec,
                "{room}" => Segment::RoomTemplate,
                "{entity}" => Segment::EntityTemplate,
                other => Segment::Literal(other.to_string()),
            });
        }
        Ok(KeyExpr(segments))
    }

    pub fn class(&self) -> Option<&str> {
        match self.0.get(1) {
            Some(Segment::Literal(c)) => Some(c.as_str()),
            _ => None,
        }
    }

    pub fn has_template(&self) -> bool {
        self.0
            .iter()
            .any(|s| matches!(s, Segment::RoomTemplate | Segment::EntityTemplate))
    }

    fn has_any_rec(&self) -> bool {
        self.0.iter().any(|s| matches!(s, Segment::AnyRec))
    }

    /// Checks conformance with `home/{class}/{room}/{entity}/{aspect}` (for
    /// the entity-addressed classes) or `home/{class}/...` (the others).
    pub fn check_schema(&self, raw: &str) -> Result<(), String> {
        match self.0.first() {
            Some(Segment::Literal(h)) if h == "home" => {}
            _ => return Err(format!("\"{raw}\" does not start with \"home/\"")),
        }
        let class = match self.0.get(1) {
            Some(Segment::Literal(c)) if CLASSES.contains(&c.as_str()) => c.as_str(),
            Some(Segment::Literal(c)) => {
                return Err(format!("\"{raw}\" has unknown class \"{c}\""));
            }
            _ => {
                return Err(format!(
                    "\"{raw}\" needs a literal class segment after \"home/\""
                ))
            }
        };
        // A forecast carries one more: WHO says so. Every forecast has a
        // source — the unit publishing it — and several may speak about
        // one series, so the slot is required rather than optional: two
        // shapes would mean a consumer wildcarding the class could not
        // write one expression that matched every opinion
        // (docs/design.md, Sources).
        let min_len = match class {
            "forecast" => 6,
            c if ENTITY_ADDRESSED.contains(&c) => 5,
            _ => 3,
        };
        if !self.has_any_rec() && self.0.len() < min_len {
            return Err(match min_len {
                6 => format!(
                    "\"{raw}\" needs room/entity/aspect/source segments after the class"
                ),
                5 => format!("\"{raw}\" needs room/entity/aspect segments after the class"),
                _ => format!("\"{raw}\" needs a segment after the class"),
            });
        }
        Ok(())
    }

    /// The room slot exists only on the entity-addressed classes.
    pub fn room_slot(&self) -> Option<&Segment> {
        if self.class().is_some_and(|c| ENTITY_ADDRESSED.contains(&c)) {
            self.0.get(2)
        } else {
            None
        }
    }

    /// Returns a copy with the room slot replaced by a literal room name.
    pub fn with_room(&self, room: &str) -> KeyExpr {
        let mut segs = self.0.clone();
        segs[2] = Segment::Literal(room.to_string());
        KeyExpr(segs)
    }

    /// Returns a copy with templates substituted for a concrete entity.
    pub fn substitute(&self, room: &str, entity: &str) -> KeyExpr {
        KeyExpr(
            self.0
                .iter()
                .map(|s| match s {
                    Segment::RoomTemplate => Segment::Literal(room.to_string()),
                    Segment::EntityTemplate => Segment::Literal(entity.to_string()),
                    other => other.clone(),
                })
                .collect(),
        )
    }

    /// Whether this expression can match keys under the given literal prefix
    /// (an entity's subtree: `home/{class}/{room}/{name}/...`). Aspect
    /// segments beyond the prefix are assumed satisfiable.
    pub fn matches_prefix(&self, prefix: &[&str]) -> bool {
        fn rec(segs: &[Segment], prefix: &[&str]) -> bool {
            if prefix.is_empty() {
                return true;
            }
            match segs.first() {
                None => false,
                Some(Segment::Literal(l)) => l == prefix[0] && rec(&segs[1..], &prefix[1..]),
                Some(Segment::Any | Segment::RoomTemplate | Segment::EntityTemplate) => {
                    rec(&segs[1..], &prefix[1..])
                }
                Some(Segment::AnyRec) => rec(&segs[1..], prefix) || rec(segs, &prefix[1..]),
            }
        }
        rec(&self.0, prefix)
    }
}

impl fmt::Display for KeyExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, seg) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str("/")?;
            }
            write!(f, "{seg}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expr(s: &str) -> KeyExpr {
        KeyExpr::parse(s).unwrap()
    }

    #[test]
    fn parse_roundtrip() {
        for s in [
            "home/state/kitchen/lamp/on",
            "home/state/*/that_lamp/**",
            "home/cmd/{room}/{entity}/**",
            "home/arbiter/{room}/{entity}/**",
        ] {
            assert_eq!(expr(s).to_string(), s);
        }
    }

    #[test]
    fn schema_accepts_design_examples() {
        for s in [
            "home/state/downstairs/**/presence",
            "home/clock/minute",
            "home/cmd/downstairs/**/light",
            "home/state/**",
            "home/state/{room}/{entity}/**",
            "home/arbiter/{room}/{entity}/**",
            "home/arbiter/hallway/front_door_lock/lock",
        ] {
            expr(s).check_schema(s).unwrap();
        }
    }

    #[test]
    fn schema_rejects() {
        assert!(expr("house/state/a/b/c")
            .check_schema("house/state/a/b/c")
            .is_err());
        assert!(expr("home/telemetry/a/b/c")
            .check_schema("home/telemetry/a/b/c")
            .is_err());
        assert!(expr("home/state/kitchen")
            .check_schema("home/state/kitchen")
            .is_err());
        assert!(expr("home/arbiter/hallway")
            .check_schema("home/arbiter/hallway")
            .is_err());
    }

    #[test]
    fn forecast_is_addressed_like_state_plus_its_source() {
        // The series it extends, plus WHO says so: room/entity/aspect and
        // then the source. It keeps state's room slot, and `forecast` is a
        // reserved word so no room can be called one.
        let s = "home/forecast/global/spot_price/price/nordpool";
        expr(s).check_schema(s).unwrap();
        assert_eq!(expr(s).to_string(), s);
        assert_eq!(
            expr(s).room_slot(),
            Some(&Segment::Literal("global".to_string()))
        );
        expr("home/forecast/{room}/{entity}/**")
            .check_schema("home/forecast/{room}/{entity}/**")
            .unwrap();
        assert!(is_reserved_word("forecast"));
        // Short of an aspect it is refused, as state is.
        assert!(expr("home/forecast/global")
            .check_schema("home/forecast/global")
            .is_err());
        // And short of a SOURCE it is refused, which state is not: every
        // forecast has an author, and leaving the slot optional would mean
        // no single expression matched every opinion about one series.
        let bare = "home/forecast/global/spot_price/price";
        assert!(expr(bare).check_schema(bare).is_err());
        let state = "home/state/global/spot_price/price";
        expr(state).check_schema(state).unwrap();
    }

    #[test]
    fn arbiter_room_slot_and_reserved_word() {
        assert_eq!(
            expr("home/arbiter/hallway/front_door_lock/lock").room_slot(),
            Some(&Segment::Literal("hallway".to_string()))
        );
        assert!(is_reserved_word("arbiter"));
    }

    #[test]
    fn prefix_matching() {
        let prefix = &["home", "cmd", "kitchen", "ceiling"];
        assert!(expr("home/cmd/kitchen/**/light").matches_prefix(prefix));
        assert!(expr("home/cmd/kitchen/ceiling/light").matches_prefix(prefix));
        assert!(expr("home/cmd/*/ceiling/**").matches_prefix(prefix));
        assert!(expr("home/cmd/**").matches_prefix(prefix));
        assert!(!expr("home/cmd/hallway/**/light").matches_prefix(prefix));
        assert!(!expr("home/cmd/kitchen/other/light").matches_prefix(prefix));
        assert!(!expr("home/state/kitchen/ceiling/on").matches_prefix(prefix));
    }
}
