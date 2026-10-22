//! Random identifiers. Submission IDs must not be guessable.

use rand::RngExt;

/// Returns `prefix` followed by 20 random lowercase alphanumeric characters.
pub fn new(prefix: &str) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::rng();
    let tail: String = (0..20)
        .map(|_| ALPHABET[rng.random_range(0..ALPHABET.len())] as char)
        .collect();
    format!("{prefix}_{tail}")
}

#[cfg(test)]
mod tests {
    #[test]
    fn ids_have_prefix_and_differ() {
        let a = super::new("s");
        let b = super::new("s");
        assert!(a.starts_with("s_"));
        assert_eq!(a.len(), 22);
        assert_ne!(a, b);
    }
}
