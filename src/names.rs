//! Agent name rules, identical to v0's `sanitize()` / `unique_name()`:
//! names are `[A-Za-z0-9._-]`, at most 32 chars; collisions get -2, -3, …

pub const NAME_MAX: usize = 32;

/// Replace every byte outside `[A-Za-z0-9._-]` with '-', truncate to 32.
pub fn sanitize(raw: &str) -> String {
    raw.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-' {
                b as char
            } else {
                '-'
            }
        })
        .take(NAME_MAX)
        .collect()
}

/// First of `base`, `base-2`, `base-3`, … not present in `taken`.
pub fn unique<'a, I>(base: &str, taken: I) -> String
where
    I: IntoIterator<Item = &'a str>,
{
    let taken: std::collections::HashSet<&str> = taken.into_iter().collect();
    if !taken.contains(base) {
        return base.to_string();
    }
    for n in 2.. {
        let candidate = format!("{base}-{n}");
        if !taken.contains(candidate.as_str()) {
            return candidate;
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_replaces_and_truncates() {
        assert_eq!(sanitize("My Agent!"), "My-Agent-");
        assert_eq!(sanitize("a.b_c-d"), "a.b_c-d");
        assert_eq!(sanitize("Ünïcode"), "--n--code"); // multibyte: every byte mapped
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
}
