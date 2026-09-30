//! Gateway-side emulation of Anthropic `stop_sequences` for Responses upstreams
//! (issue #605).
//!
//! The OpenAI Responses API has no `stop` parameter — only Chat Completions does
//! — so a client's `stop_sequences` would otherwise be silently dropped for every
//! Responses-protocol provider. Claude Code's auto-mode permission classifier
//! relies on them (`["</block>"]`, `["</severity>"]`), and the trailing text past
//! the stop breaks its parser.
//!
//! [`StopScanner`] is the piece the Responses→Anthropic SSE translation
//! ([`crate::model::responses::AnthropicSseMachine`]) drives over *assistant text*
//! only: reasoning summaries and tool-call arguments are never scanned. It holds
//! back at most `max_len - 1` bytes — the longest suffix that could still grow
//! into a stop sequence — so a stop split across two upstream deltas is still
//! caught, and everything else is emitted immediately (streaming is preserved).

/// What one scanned delta yields.
pub(crate) struct StopScan {
    /// Text to emit to the client. May be empty (everything was held back).
    pub emit: String,
    /// The stop sequence that matched, if this delta completed one. Text after
    /// the match is discarded and the scanner is terminal from here on.
    pub matched: Option<String>,
}

/// Scans assistant text for the client's `stop_sequences`, holding back only the
/// bytes that could still be the start of one.
#[derive(Debug, Clone, Default)]
pub(crate) struct StopScanner {
    /// The configured stop strings, in the client's order (ties on the match
    /// position are broken by this order). Never contains an empty string.
    sequences: Vec<String>,
    /// The longest configured sequence in bytes; the holdback never exceeds
    /// `max_len - 1`.
    max_len: usize,
    /// Text seen but not yet emitted, because it is a proper prefix of some
    /// stop sequence.
    holdback: String,
    /// The sequence that matched, once one has.
    matched: Option<String>,
}

impl StopScanner {
    /// A scanner for `sequences`. Empty strings are dropped (they would match
    /// everywhere); an empty list disables the feature entirely.
    pub(crate) fn new(sequences: Vec<String>) -> Self {
        let sequences: Vec<String> = sequences.into_iter().filter(|s| !s.is_empty()).collect();
        let max_len = sequences.iter().map(String::len).max().unwrap_or(0);
        Self {
            sequences,
            max_len,
            holdback: String::new(),
            matched: None,
        }
    }

    /// Whether no stop sequence is configured — the caller's fast path, which
    /// must stay byte-identical to the pre-#605 translation.
    pub(crate) fn is_empty(&self) -> bool {
        self.sequences.is_empty()
    }

    /// The stop sequence that matched, if any.
    pub(crate) fn matched(&self) -> Option<&str> {
        self.matched.as_deref()
    }

    /// Feed one assistant-text delta. Never call this once [`Self::matched`] is
    /// `Some` — the translation stops the whole message at that point.
    pub(crate) fn push(&mut self, delta: &str) -> StopScan {
        self.holdback.push_str(delta);
        if let Some((start, sequence)) = self.earliest_match() {
            let emit = self.holdback[..start].to_string();
            self.holdback.clear();
            self.matched = Some(sequence.clone());
            return StopScan {
                emit,
                matched: Some(sequence),
            };
        }
        let keep = self.pending_prefix_len();
        let split = self.holdback.len() - keep;
        let emit = self.holdback[..split].to_string();
        self.holdback.drain(..split);
        StopScan {
            emit,
            matched: None,
        }
    }

    /// Release the held-back text. Called when the open text block closes for any
    /// reason other than a match — a prefix that never completed is ordinary
    /// output and the client must still receive it.
    pub(crate) fn flush(&mut self) -> String {
        std::mem::take(&mut self.holdback)
    }

    /// The earliest occurrence of any stop sequence in the holdback, as a byte
    /// offset. Earliest start wins; equal starts are broken by the configured
    /// order (so `find`'s first hit at that offset is kept).
    fn earliest_match(&self) -> Option<(usize, String)> {
        let mut best: Option<(usize, &String)> = None;
        for sequence in &self.sequences {
            let Some(start) = self.holdback.find(sequence.as_str()) else {
                continue;
            };
            if best.is_none_or(|(best_start, _)| start < best_start) {
                best = Some((start, sequence));
            }
        }
        best.map(|(start, sequence)| (start, sequence.clone()))
    }

    /// Length in bytes of the longest holdback suffix that is a *proper* prefix
    /// of some stop sequence, i.e. how much must stay buffered. Only char
    /// boundaries are considered, so a multi-byte code point is never split.
    fn pending_prefix_len(&self) -> usize {
        let len = self.holdback.len();
        // A proper prefix is shorter than the sequence itself, so never more
        // than `max_len - 1` bytes need holding back.
        let earliest = len.saturating_sub(self.max_len.saturating_sub(1));
        for start in earliest..len {
            if !self.holdback.is_char_boundary(start) {
                continue;
            }
            let suffix = &self.holdback[start..];
            if self
                .sequences
                .iter()
                .any(|sequence| sequence.len() > suffix.len() && sequence.starts_with(suffix))
            {
                return len - start;
            }
        }
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scanner(sequences: &[&str]) -> StopScanner {
        StopScanner::new(sequences.iter().map(|s| (*s).to_string()).collect())
    }

    #[test]
    fn an_empty_list_disables_the_scanner() {
        assert!(scanner(&[]).is_empty());
        // An empty string would match everywhere; it is dropped, not honoured.
        assert!(scanner(&[""]).is_empty());
    }

    #[test]
    fn emits_text_before_a_match_and_discards_the_rest() {
        let mut scanner = scanner(&["</block>"]);
        let scan = scanner.push("answer</block>garbage");
        assert_eq!(scan.emit, "answer");
        assert_eq!(scan.matched.as_deref(), Some("</block>"));
        assert_eq!(scanner.matched(), Some("</block>"));
    }

    #[test]
    fn holds_back_only_a_proper_prefix_across_deltas() {
        let mut scanner = scanner(&["</block>"]);
        let first = scanner.push("hello </blo");
        assert_eq!(first.emit, "hello ");
        assert!(first.matched.is_none());

        let second = scanner.push("ck> tail");
        assert_eq!(second.emit, "");
        assert_eq!(second.matched.as_deref(), Some("</block>"));
    }

    #[test]
    fn flush_releases_a_prefix_that_never_completed() {
        let mut scanner = scanner(&["</block>"]);
        assert_eq!(scanner.push("abc </blo").emit, "abc ");
        assert_eq!(scanner.flush(), "</blo");
        assert_eq!(scanner.flush(), "");
        assert!(scanner.matched().is_none());
    }

    #[test]
    fn never_splits_a_multibyte_code_point() {
        // '글' is 3 bytes; a byte-wise suffix scan would slice it apart.
        let mut scanner = scanner(&["</block>"]);
        let first = scanner.push("한글</bl");
        assert_eq!(first.emit, "한글");
        let second = scanner.push("ock>");
        assert_eq!(second.emit, "");
        assert_eq!(second.matched.as_deref(), Some("</block>"));
    }

    #[test]
    fn the_earliest_match_wins_over_the_configured_order() {
        // `</severity>` is listed first but occurs later, so `</block>` wins.
        let mut scanner = scanner(&["</severity>", "</block>"]);
        let scan = scanner.push("a</block>b</severity>c");
        assert_eq!(scan.emit, "a");
        assert_eq!(scan.matched.as_deref(), Some("</block>"));
    }

    #[test]
    fn a_tie_on_position_is_broken_by_the_configured_order() {
        let mut scanner = scanner(&["</b", "</block>"]);
        let scan = scanner.push("x</block>");
        assert_eq!(scan.emit, "x");
        assert_eq!(scan.matched.as_deref(), Some("</b"));
    }
}
