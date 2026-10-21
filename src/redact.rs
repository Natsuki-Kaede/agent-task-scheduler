//! Redaction of secret-looking values before a command is stored or shown
//! (design 13).

const SECRET_WORDS: &[&str] = &[
    "token",
    "secret",
    "password",
    "passwd",
    "apikey",
    "api_key",
    "credential",
    "auth",
];

fn looks_secret(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    SECRET_WORDS.iter().any(|w| lower.contains(w)) || lower.ends_with("_key") || lower == "key"
}

/// Replaces the value of `NAME=value`, `--name=value` and `--name value`
/// words whose name looks like a secret, and the word after `Bearer`.
pub fn command_words(words: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(words.len());
    let mut hide_next = false;
    for word in words {
        if hide_next {
            out.push("[redacted]".to_string());
            hide_next = false;
            continue;
        }
        if word.eq_ignore_ascii_case("bearer") {
            hide_next = true;
            out.push(word.clone());
            continue;
        }
        if let Some((name, _)) = word.split_once('=') {
            if looks_secret(name.trim_start_matches('-')) {
                out.push(format!("{name}=[redacted]"));
                continue;
            }
        } else if let Some(flag) = word.strip_prefix("--")
            && looks_secret(flag)
        {
            hide_next = true;
        }
        out.push(word.clone());
    }
    out
}

/// Redacts a shell source line by applying [`command_words`] to its
/// whitespace-separated words. The result is for display only.
pub fn shell_source(source: &str) -> String {
    let words: Vec<String> = source.split_whitespace().map(str::to_string).collect();
    command_words(&words).join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<String> {
        s.split(' ').map(str::to_string).collect()
    }

    #[test]
    fn redacts_secret_values() {
        let cases = [
            ("cargo build --release", "cargo build --release"),
            ("API_KEY=abc123 npm test", "API_KEY=[redacted] npm test"),
            ("tool --token=abc run", "tool --token=[redacted] run"),
            (
                "tool --password hunter2 run",
                "tool --password [redacted] run",
            ),
            (
                "curl -H Authorization: Bearer xyz",
                "curl -H Authorization: Bearer [redacted]",
            ),
            ("make GITHUB_TOKEN=t1", "make GITHUB_TOKEN=[redacted]"),
            ("env PATH=/bin ls", "env PATH=/bin ls"),
        ];
        for (input, want) in cases {
            assert_eq!(command_words(&w(input)).join(" "), want, "{input}");
        }
    }
}
