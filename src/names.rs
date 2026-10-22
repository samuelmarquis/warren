//! Agent name rules: names are `[A-Za-z0-9._-]`, at most 32 chars, and
//! never start with `-` or `.`; collisions get -2, -3, …

pub const NAME_MAX: usize = 32;

/// Replace every character outside `[A-Za-z0-9._-]` with '-', run dashes
/// together, drop what the name would start with if that is `-` or `.`, and
/// truncate to 32.
///
/// A title is often an emoji first (OMP's are): `🎚️ LIMIT: …` mapped byte
/// by byte was eight dashes and then the name, and a name that starts with
/// `-` is read as a flag by everything that is handed it as an argument —
/// `warren new` named the agent after its directory instead, and the
/// dashboard waited forever for a daemon that never came up under that name.
/// A leading `.` would hide its socket.
pub fn sanitize(raw: &str) -> String {
    let mut out = String::new();
    for c in raw.chars() {
        let c = if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '-' };
        if c == '-' && out.ends_with('-') {
            continue;
        }
        if out.is_empty() && matches!(c, '-' | '.') {
            continue;
        }
        out.push(c);
    }
    out.truncate(NAME_MAX); // ASCII by now, so any length is a boundary
    out
}

/// First of `base`, `base-2`, `base-3`, … not present in `taken`.
///
/// The suffix is made *room* for rather than appended: a 32-char base plus
/// `-2` would be a 34-char name, and every path that looks an agent up by
/// name trims to 32 — so the longer name would quietly resolve to the
/// shorter one, and two agents would share a daemon.
pub fn unique<'a, I>(base: &str, taken: I) -> String
where
    I: IntoIterator<Item = &'a str>,
{
    let taken: std::collections::HashSet<&str> = taken.into_iter().collect();
    if !taken.contains(base) {
        return base.to_string();
    }
    for n in 2.. {
        let suffix = format!("-{n}");
        let room = NAME_MAX.saturating_sub(suffix.len());
        let candidate = format!("{}{suffix}", &base[..base.len().min(room)]);
        if !taken.contains(candidate.as_str()) {
            return candidate;
        }
    }
    unreachable!()
}

/// Is this a name an agent could actually have?
///
/// The lookup paths check rather than rewrite. Sanitizing a name you were
/// asked to *find* is how you end up holding a different agent: the
/// characters it replaces and the length it trims both map two names onto
/// one. What has to stay true is only what keeps a socket path inside the
/// run directory — no separators, nothing outside the name charset.
pub fn valid(raw: &str) -> bool {
    !raw.is_empty()
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_replaces_and_truncates() {
        assert_eq!(sanitize("My Agent!"), "My-Agent-");
        assert_eq!(sanitize("a.b_c-d"), "a.b_c-d");
        assert_eq!(sanitize("Ünïcode"), "n-code"); // a character is one dash, not one per byte
        assert_eq!(sanitize("🎚️ LIMIT: Review new TEAM A report"), "LIMIT-Review-new-TEAM-A-report");
        assert_eq!(sanitize(".hidden"), "hidden");
        assert_eq!(sanitize("🦠"), "", "nothing left is the caller's to refuse");
        let long = "x".repeat(40);
        assert_eq!(sanitize(&long).len(), NAME_MAX);
    }

    #[test]
    fn unique_suffixes() {
        assert_eq!(unique("a", []), "a");
        assert_eq!(unique("a", ["a"]), "a-2");
        assert_eq!(unique("a", ["a", "a-2", "a-3"]), "a-4");
        assert_eq!(unique("a", ["a-2"]), "a");
    }

    #[test]
    fn a_suffix_never_pushes_a_name_past_the_limit() {
        // A name at the limit is where this bites: `<32 chars>-2` is 34, and
        // a 34-char name trims back onto the 32-char one it was meant to be
        // distinct from.
        let full = "x".repeat(NAME_MAX);
        let second = unique(&full, [full.as_str()]);
        assert_eq!(second.len(), NAME_MAX);
        assert_ne!(sanitize(&second), sanitize(&full), "the two stay distinct once trimmed");
        // And it keeps counting past the first collision.
        let third = unique(&full, [full.as_str(), second.as_str()]);
        assert_eq!(third.len(), NAME_MAX);
        assert_ne!(third, second);
    }

    #[test]
    fn a_name_is_checked_not_rewritten_when_it_is_looked_up() {
        assert!(valid("Explore-type-systems-and-TEE-par-2"), "length is not a lookup's business");
        assert!(valid("a.b_c-d"));
        assert!(!valid(""));
        // Nothing that could walk out of the run directory.
        assert!(!valid("../../etc/passwd"));
        assert!(!valid("two words"));
    }
}
