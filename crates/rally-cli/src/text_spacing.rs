//! Advisory check for words run together in agent-written fact text.
//!
//! Long agent sessions sometimes drop the spaces between words and ids
//! ("Livegitconfirms", "at2257f1c0", "Readfact_164eb"). Rally stores the text
//! exactly as sent, so the only place to catch it is the write. The check is
//! advisory: the fact still commits, and the sender gets a warning naming the
//! tokens so it can resend readable text.
//!
//! Tuned on 4,998 facts from a live Easy Terminal room (2026-10-17): it flagged
//! 99, every sampled hit was glued text, and no Claude-authored fact tripped it.
//! A single odd token is common in technical prose, so it needs two distinct
//! suspicious tokens before it warns.

/// Distinct suspicious tokens needed before warning.
const MIN_DISTINCT_HITS: usize = 2;
/// Tokens named in the warning.
const MAX_EXAMPLES: usize = 3;
/// Lowercase-only runs at least this long are almost never one English word.
const LONG_LOWER_RUN: usize = 18;
/// Minimum hex tail for a word glued to a commit hash.
const MIN_HEX_TAIL: usize = 6;
/// Minimum lowercase tail after an acronym ("ACKcodeproposal").
const MIN_ACRONYM_TAIL: usize = 10;

/// Returns the distinct glued tokens when there are enough to warn about.
pub(crate) fn glued_tokens(text: &str) -> Option<Vec<String>> {
    let mut hits: Vec<String> = Vec::new();
    for raw in text.split_whitespace() {
        let token = raw.trim_matches(|c: char| ".,;:()[]{}\"'`!?*".contains(c));
        if skip_token(token) || !is_glued(token) {
            continue;
        }
        if !hits.iter().any(|h| h == token) {
            hits.push(token.to_string());
        }
    }
    (hits.len() >= MIN_DISTINCT_HITS).then_some(hits)
}

/// Warning text for `rally say`, or `None` when the text reads normally.
pub(crate) fn glued_text_warning(subject: &str, summary: Option<&str>) -> Option<String> {
    let mut text = subject.to_string();
    if let Some(summary) = summary {
        text.push(' ');
        text.push_str(summary);
    }
    let hits = glued_tokens(&text)?;
    let examples: Vec<&str> = hits.iter().take(MAX_EXAMPLES).map(String::as_str).collect();
    Some(format!(
        "{} words look run together ({}). The fact was saved as written; if that was not \
         intended, resend it with spaces between words and ids so the receiver can read it.",
        hits.len(),
        examples.join(", ")
    ))
}

/// Paths, flags, URLs, assignments and code spans are not prose.
fn skip_token(token: &str) -> bool {
    token.is_empty()
        || token.starts_with('-')
        || token.contains("http")
        || token.chars().any(|c| "/.=:$\\`@#<>".contains(c))
}

fn is_glued(token: &str) -> bool {
    word_then_hash(token)
        || word_then_rally_id(token)
        || long_lower_run(token)
        || acronym_then_words(token)
}

/// "at2257f1c0", "commit7817b6ec". The letters before the digits must include
/// one outside a-f, so a bare hash such as "be19e0f2" is not flagged.
fn word_then_hash(token: &str) -> bool {
    let Some(split) = token.find(|c: char| c.is_ascii_digit()) else {
        return false;
    };
    let (head, tail) = token.split_at(split);
    head.len() >= 2
        && head.chars().all(|c| c.is_ascii_alphabetic())
        && head
            .chars()
            .any(|c| matches!(c.to_ascii_lowercase(), 'g'..='z'))
        && tail.len() >= MIN_HEX_TAIL
        && tail
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

/// "Readfact_164eb": a word directly followed by a Rally event id.
fn word_then_rally_id(token: &str) -> bool {
    ["fact_", "wake_", "room_"].iter().any(|marker| {
        token.find(marker).is_some_and(|at| {
            let head = &token[..at];
            let tail = &token[at + marker.len()..];
            head.len() >= 2
                && head.chars().all(|c| c.is_ascii_alphabetic())
                && tail.chars().next().is_some_and(|c| c.is_ascii_hexdigit())
        })
    })
}

/// "currentcanonicalworktree".
fn long_lower_run(token: &str) -> bool {
    token.len() >= LONG_LOWER_RUN && token.chars().all(|c| c.is_ascii_lowercase())
}

/// "ACKcodeproposal": an acronym followed by a long lowercase run. Short tails
/// such as "ETModels" or "IOSurface" are real identifiers and do not match.
fn acronym_then_words(token: &str) -> bool {
    let upper = token.chars().take_while(|c| c.is_ascii_uppercase()).count();
    let rest = &token[upper..];
    upper >= 2 && rest.len() >= MIN_ACRONYM_TAIL && rest.chars().all(|c| c.is_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_the_mangled_handoff_seen_live() {
        // Stored text of fact_17a8b (2026-10-17), byte for byte.
        let summary = "Readfact_164eb. Livegitconfirms currentcanonicalworktree \
                       branchfix/daemon-guardrail-channels-1017 at2257f1c0, clean";
        let hits = glued_tokens(summary).expect("should warn");
        assert!(hits.contains(&"Readfact_164eb".to_string()));
        assert!(hits.contains(&"currentcanonicalworktree".to_string()));
        assert!(hits.contains(&"at2257f1c0".to_string()));
    }

    #[test]
    fn normal_technical_prose_passes() {
        let text = "Merged be19e0f2 into main at 2257f1c0; fact_164eb acked. \
                    ETModels and IOSurface build on sha256 checks, see crates/rally-cli/src/lib.rs \
                    and --timeout-ms 20000. snake_case_identifier stays fine.";
        assert_eq!(glued_tokens(text), None);
    }

    #[test]
    fn a_single_odd_token_does_not_warn() {
        assert_eq!(glued_tokens("Checked commit7817b6ec against main."), None);
    }

    #[test]
    fn repeated_token_counts_once() {
        assert_eq!(glued_tokens("at2257f1c0 then at2257f1c0 again"), None);
    }

    #[test]
    fn each_pattern_matches_its_example() {
        assert!(word_then_hash("exact31e3107d"));
        assert!(!word_then_hash("be19e0f2"));
        assert!(!word_then_hash("sha256"));
        assert!(word_then_rally_id("Readartifactfact_11bfe"));
        assert!(!word_then_rally_id("fact_11bfe"));
        assert!(long_lower_run("explicitlyunavailable"));
        assert!(!long_lower_run("responsibilities"));
        assert!(acronym_then_words("ACKcodeproposal"));
        assert!(!acronym_then_words("ETArtifacts"));
    }

    #[test]
    fn warning_names_examples_and_says_the_fact_was_saved() {
        let warning = glued_text_warning(
            "ACK availability",
            Some("at2257f1c0 currentcanonicalworktree"),
        )
        .expect("should warn");
        assert!(warning.starts_with("2 words look run together"));
        assert!(warning.contains("currentcanonicalworktree"));
        assert!(warning.contains("saved as written"));
    }
}
