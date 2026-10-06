//! Detect a degenerate repetition loop at the end of a length-capped generation.
//!
//! Some small models (glm-ocr on Ollama >= 0.30, deepseek-ocr on blank input) answer correctly and
//! then repeat the answer, or a fence, until `num_predict`. The full output stays in the response
//! for audit; the receipt only says where the first copy of the repeated tail ends so a caller can
//! read the useful prefix without guessing.

use serde_json::{Value, json};

/// Longest repeated unit considered, in characters.
const MAX_PERIOD: usize = 4_096;
/// Trailing characters scanned. With `MAX_PERIOD` this bounds the scan at
/// O(`SCAN_WINDOW` * `MAX_PERIOD`) regardless of how long the output is.
const SCAN_WINDOW: usize = 64 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub(super) struct TailLoop {
    /// Characters (Unicode scalar values) up to and including the first copy of the loop unit.
    pub unique_prefix: usize,
    /// Length of the repeated unit, in characters.
    pub period: usize,
    /// Characters after the first copy that only repeat it.
    pub repeated: usize,
}

/// Find the earliest-starting periodic run that extends to the end of `text`.
///
/// A run qualifies when it holds at least one full repeat of a unit and spans 48+ characters, or
/// when 128+ characters repeat a longer unit that the output cap cut mid-copy. Only the last
/// [`SCAN_WINDOW`] characters choose the unit; a winning run that fills the window is then
/// followed back through the earlier text so the prefix still ends after its first copy.
fn detect_tail_loop(text: &str) -> Option<TailLoop> {
    let total = text.chars().count();
    let offset = total.saturating_sub(SCAN_WINDOW);
    let window: Vec<char> = text.chars().skip(offset).collect();
    let mut found = detect_in_window(&window)?;
    if offset > 0 && found.unique_prefix == found.period {
        let chars: Vec<char> = text.chars().collect();
        let mut start = offset;
        while start > 0 && chars[start - 1] == chars[start - 1 + found.period] {
            start -= 1;
        }
        found.repeated += offset - start;
        found.unique_prefix = start + found.period;
    } else {
        found.unique_prefix += offset;
    }
    Some(found)
}

fn detect_in_window(chars: &[char]) -> Option<TailLoop> {
    let n = chars.len();
    let mut best: Option<TailLoop> = None;
    for period in 1..=MAX_PERIOD.min(n.saturating_sub(1)) {
        // A run with this period ends its first copy at `start + period >= period`, so once the
        // period reaches the best prefix no longer unit can start earlier.
        if best
            .as_ref()
            .is_some_and(|current| period >= current.unique_prefix)
        {
            break;
        }
        let repeated = (period..n)
            .rev()
            .take_while(|&index| chars[index] == chars[index - period])
            .count();
        let qualifies = (repeated >= period && repeated + period >= 48) || repeated >= 128;
        if !qualifies {
            continue;
        }
        let start = n - repeated - period;
        if best
            .as_ref()
            .is_none_or(|current| start + period < current.unique_prefix)
        {
            best = Some(TailLoop {
                unique_prefix: start + period,
                period,
                repeated,
            });
        }
    }
    best
}

/// Receipt for a chat response that hit the output cap while looping; `None` otherwise.
pub(super) fn output_check(response: &Value) -> Option<Value> {
    if response.get("done_reason").and_then(Value::as_str) != Some("length") {
        return None;
    }
    let content = response
        .pointer("/message/content")
        .and_then(Value::as_str)?;
    let found = detect_tail_loop(content)?;
    Some(json!({
        "repetition_loop": true,
        "unique_prefix_chars": found.unique_prefix,
        "period_chars": found.period,
        "repeated_chars": found.repeated,
        "note": "the model repeated its output until the num_predict cap; the first unique_prefix_chars characters of message.content hold one copy. Treat the answer as low confidence or retry with a stop sequence."
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_a_fence_loop_after_the_answer() {
        let text = format!("Invoice #48213\nTotal: $1,337.42\n{}", "```\n".repeat(40));
        let found = detect_tail_loop(&text).expect("loop");
        assert_eq!(
            &text[..found.unique_prefix],
            // The earliest phase of the loop wins: one copy of "\n```" closes the prefix.
            "Invoice #48213\nTotal: $1,337.42\n```"
        );
        assert_eq!(found.period, 4);
    }

    #[test]
    fn keeps_one_copy_of_a_long_unit_cut_mid_repeat() {
        let table = "<table><tr><th>Item</th><th>Qty</th></tr><tr><td>Apple</td><td>3</td></tr><tr><td>Pear</td><td>5</td></tr></table>\n";
        let text = format!("{table}{table}{}", &table[..60]);
        let found = detect_tail_loop(&text).expect("loop");
        assert_eq!(&text[..found.unique_prefix], table);
    }

    #[test]
    fn one_copy_plus_a_long_partial_copy_is_a_loop() {
        let table = "<table class=\"t\"><thead><tr><th>Item</th><th>Qty</th><th>Price</th></tr></thead><tbody><tr><td>Apple</td><td>3</td><td>$1.20</td></tr><tr><td>Pear</td><td>5</td><td>$2.50</td></tr></tbody></table>\n";
        let text = format!("{table}{}", &table[..150]);
        let found = detect_tail_loop(&text).expect("loop");
        assert_eq!(&text[..found.unique_prefix], table);
    }

    #[test]
    fn ordinary_text_and_short_repeats_are_not_loops() {
        assert_eq!(
            detect_tail_loop("The quick brown fox jumps over the lazy dog."),
            None
        );
        assert_eq!(detect_tail_loop("ha ha ha ha"), None);
        assert_eq!(detect_tail_loop(""), None);
    }

    #[test]
    fn counts_characters_not_bytes() {
        let text = format!("résumé ✓\n{}", "é✓\n".repeat(30));
        let found = detect_tail_loop(&text).expect("loop");
        let prefix: String = text.chars().take(found.unique_prefix).collect();
        assert_eq!(prefix, "résumé ✓\né");
    }

    #[test]
    fn megabyte_loop_is_found_quickly_with_the_full_prefix() {
        let text = format!("answer\n{}", "abc\n".repeat(262_144));
        let started = std::time::Instant::now();
        let found = detect_tail_loop(&text).expect("loop");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(&text[..found.unique_prefix], "answer\nabc");
        assert_eq!(found.period, 4);
        assert_eq!(found.unique_prefix + found.repeated, text.chars().count());
    }

    #[test]
    fn long_output_with_a_loop_only_in_its_tail_reports_absolute_offsets() {
        // 200k characters of non-periodic text, then a short fence loop.
        let text: String = (0..200_000u32)
            .map(|index| {
                char::from(b'A' + u8::try_from(index.wrapping_mul(2_654_435_761) % 26).unwrap())
            })
            .collect();
        let mut text = text;
        let unique = text.chars().count();
        text.push_str(&"```\n".repeat(40));
        let found = detect_tail_loop(&text).expect("loop");
        assert_eq!(found.period, 4);
        assert!(
            (unique..=unique + 4).contains(&found.unique_prefix),
            "{} vs {unique}",
            found.unique_prefix
        );
        assert_eq!(found.unique_prefix + found.repeated, text.chars().count());
    }

    #[test]
    fn output_check_requires_a_length_stop() {
        let content = format!("answer\n{}", "```\n".repeat(40));
        let capped = json!({"done_reason": "length", "message": {"content": content}});
        assert_eq!(output_check(&capped).unwrap()["unique_prefix_chars"], 10);
        let stopped = json!({"done_reason": "stop", "message": {"content": content}});
        assert_eq!(output_check(&stopped), None);
    }
}
