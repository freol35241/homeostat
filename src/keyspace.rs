use std::fmt;

/// `home/{class}/...` — the classes the core owns.
pub const CLASSES: &[&str] = &["state", "cmd", "config", "meta", "health", "clock", "history"];

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
    /// `{room}` — expanded per bound entity, adapters only.
    RoomTemplate,
    /// `{entity}` — expanded per bound entity, adapters only.
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
    /// `state`/`cmd`) or `home/{class}/...` (other classes).
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
            _ => return Err(format!("\"{raw}\" needs a literal class segment after \"home/\"")),
        };
        let min_len = if matches!(class, "state" | "cmd") { 5 } else { 3 };
        if !self.has_any_rec() && self.0.len() < min_len {
            if min_len == 5 {
                return Err(format!(
                    "\"{raw}\" needs room/entity/aspect segments after the class"
                ));
            }
            return Err(format!("\"{raw}\" needs a segment after the class"));
        }
        Ok(())
    }

    /// The room slot exists only for `state`/`cmd` keys.
    pub fn room_slot(&self) -> Option<&Segment> {
        if matches!(self.class(), Some("state") | Some("cmd")) {
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
        ] {
            expr(s).check_schema(s).unwrap();
        }
    }

    #[test]
    fn schema_rejects() {
        assert!(expr("house/state/a/b/c").check_schema("house/state/a/b/c").is_err());
        assert!(expr("home/telemetry/a/b/c").check_schema("home/telemetry/a/b/c").is_err());
        assert!(expr("home/state/kitchen").check_schema("home/state/kitchen").is_err());
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
