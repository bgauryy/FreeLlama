//! Token counts read from a raw response as it streams through, without buffering or changing it.
//!
//! Ollama's native API, its OpenAI-compatible API and the decision API each report what a
//! request used in their own field names, in the last chunk of a stream or in the one JSON body.
//! The scanner looks for those fields in the bytes passing by and keeps the last value of each.
//! Generated text cannot fake a field: inside a JSON string its quotes are escaped.

/// What a raw response reported using.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RawUsage {
    pub prompt_tokens: Option<u64>,
    pub cached_prompt_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    /// Ollama's `load_duration`, in nanoseconds.
    pub load_duration_ns: Option<u64>,
}

#[derive(Clone, Copy)]
enum Field {
    Prompt,
    Cached,
    Output,
    Load,
}

const FIELDS: &[(&[u8], Field)] = &[
    (b"\"prompt_eval_count\":", Field::Prompt),
    (b"\"prompt_tokens\":", Field::Prompt),
    (b"\"input_tokens\":", Field::Prompt),
    (b"\"prompt_eval_cached_count\":", Field::Cached),
    (b"\"cached_tokens\":", Field::Cached),
    (b"\"eval_count\":", Field::Output),
    (b"\"completion_tokens\":", Field::Output),
    (b"\"output_tokens\":", Field::Output),
    (b"\"load_duration\":", Field::Load),
];

/// Bytes kept from the end of one chunk so a field split across chunks is still found.
const CARRY: usize = 64;

#[derive(Debug, Default)]
pub struct UsageScanner {
    carry: Vec<u8>,
    usage: RawUsage,
}

impl UsageScanner {
    pub fn feed(&mut self, chunk: &[u8]) {
        let mut window = std::mem::take(&mut self.carry);
        window.extend_from_slice(chunk);
        for (name, field) in FIELDS {
            let mut from = 0;
            while let Some(found) = find(&window[from..], name) {
                let start = from + found + name.len();
                from = start;
                let digits_start = start
                    + window[start..]
                        .iter()
                        .take_while(|byte| **byte == b' ')
                        .count();
                let digits = window[digits_start..]
                    .iter()
                    .take_while(|byte| byte.is_ascii_digit())
                    .count();
                // A number that reaches the end of the window may continue in the next chunk;
                // the carried bytes read it again whole.
                if digits == 0 || digits_start + digits == window.len() {
                    continue;
                }
                let value = std::str::from_utf8(&window[digits_start..digits_start + digits])
                    .ok()
                    .and_then(|text| text.parse::<u64>().ok());
                let slot = match field {
                    Field::Prompt => &mut self.usage.prompt_tokens,
                    Field::Cached => &mut self.usage.cached_prompt_tokens,
                    Field::Output => &mut self.usage.output_tokens,
                    Field::Load => &mut self.usage.load_duration_ns,
                };
                if value.is_some() {
                    *slot = value;
                }
            }
        }
        let keep = window.len().saturating_sub(CARRY);
        self.carry = window.split_off(keep);
    }

    pub fn usage(&self) -> RawUsage {
        self.usage
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_streams_report_their_final_counts() {
        let mut scanner = UsageScanner::default();
        scanner.feed(br#"{"message":{"content":"say \"eval_count\":9"},"done":false}"#);
        scanner.feed(b"\n");
        scanner.feed(br#"{"done":true,"load_duration":1200000000,"prompt_eval_count":31,"prompt_eval_cached_count":24,"eval_c"#);
        scanner.feed(br#"ount":7}"#);
        scanner.feed(b"\n");
        assert_eq!(
            scanner.usage(),
            RawUsage {
                prompt_tokens: Some(31),
                cached_prompt_tokens: Some(24),
                output_tokens: Some(7),
                load_duration_ns: Some(1_200_000_000),
            }
        );
    }

    #[test]
    fn openai_and_decision_bodies_use_their_own_names() {
        let mut scanner = UsageScanner::default();
        scanner.feed(br#"{"usage":{"prompt_tokens": 12,"completion_tokens":3,"prompt_tokens_details":{"cached_tokens":8}}}"#);
        let usage = scanner.usage();
        assert_eq!(usage.prompt_tokens, Some(12));
        assert_eq!(usage.output_tokens, Some(3));
        assert_eq!(usage.cached_prompt_tokens, Some(8));
        let mut scanner = UsageScanner::default();
        scanner
            .feed(br#"{"model":"d","answers":[],"usage":{"input_tokens":40,"output_tokens":0}}"#);
        assert_eq!(scanner.usage().prompt_tokens, Some(40));
        assert_eq!(scanner.usage().output_tokens, Some(0));
    }

    #[test]
    fn a_number_split_across_chunks_is_read_whole() {
        let mut scanner = UsageScanner::default();
        scanner.feed(br#"{"eval_count":12"#);
        assert_eq!(scanner.usage().output_tokens, None);
        scanner.feed(br"34}");
        assert_eq!(scanner.usage().output_tokens, Some(1234));
    }
}
