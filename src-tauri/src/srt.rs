//! Turning whisper.cpp's token stream into subtitle captions.
//!
//! Everything here is pure: JSON in, SRT text out, no I/O and no Tauri. The
//! grouping rules are the whole point of the feature, so they are unit-tested
//! against the shapes whisper actually emits rather than exercised only through
//! the app.
//!
//! whisper.cpp reports *tokens*, not words. Tokens are BPE fragments — `" coun"`,
//! `"try"`, `","` — and the convention that matters is that a token beginning a
//! new word carries a leading space. Reassembling words from that, rather than
//! splitting the segment text on whitespace, is what keeps each word's start and
//! end timestamps attached to it.

use serde::Deserialize;

// ---------------------------------------------------------------------------
// whisper.cpp's --output-json-full shape
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Offsets {
    /// Milliseconds from the start of the audio.
    pub from: i64,
    pub to: i64,
}

#[derive(Debug, Deserialize)]
pub struct Token {
    pub text: String,
    pub offsets: Offsets,
    /// Token probability. Averaged over a word to give a confidence.
    #[serde(default)]
    pub p: f32,
}

#[derive(Debug, Deserialize)]
pub struct Segment {
    #[serde(default)]
    pub tokens: Vec<Token>,
    #[serde(default)]
    pub text: String,
    pub offsets: Offsets,
}

#[derive(Debug, Default, Deserialize)]
pub struct ResultInfo {
    #[serde(default)]
    pub language: String,
}

#[derive(Debug, Deserialize)]
pub struct Transcript {
    #[serde(default)]
    pub transcription: Vec<Segment>,
    #[serde(default)]
    pub result: ResultInfo,
}

// ---------------------------------------------------------------------------
// Words and captions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Word {
    pub text: String,
    pub start_ms: i64,
    pub end_ms: i64,
    /// Mean token probability, 0–1. Surfaced so the UI can flag shaky passages.
    pub confidence: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Caption {
    pub start_ms: i64,
    pub end_ms: i64,
    pub text: String,
    pub confidence: f32,
}

/// Silence longer than this ends a caption early.
///
/// Without it a caption can span a pause — the first half of a sentence, two
/// seconds of nothing, then a new speaker — and sit on screen through all of it.
const GAP_BREAK_MS: i64 = 900;

/// A caption never sits on screen for longer than this, even mid-sentence.
const MAX_DURATION_MS: i64 = 7_000;

/// Shortest caption we will emit. Below this the reader cannot follow it, so a
/// too-brief caption is stretched rather than flashed.
const MIN_DURATION_MS: i64 = 700;

/// Sentence-final punctuation, in Latin and CJK forms.
fn ends_sentence(word: &str) -> bool {
    word.chars()
        .last()
        .is_some_and(|c| matches!(c, '.' | '!' | '?' | '…' | '。' | '！' | '？'))
}

/// Whether a token is one of whisper's bracketed control markers.
///
/// `[_BEG_]`, `[_TT_190]`, and friends carry timing but no text; `[BLANK_AUDIO]`
/// and `(upbeat music)` are annotations rather than speech. None belong in a
/// caption.
fn is_special(text: &str) -> bool {
    let t = text.trim();
    (t.starts_with('[') && t.ends_with(']')) || (t.starts_with('(') && t.ends_with(')'))
}

/// Rebuild words from whisper's BPE tokens, carrying timings across.
pub fn words_from(transcript: &Transcript) -> Vec<Word> {
    let mut words: Vec<Word> = Vec::new();
    // Probability sum and count for the word currently being assembled.
    let mut acc: Option<(f32, u32)> = None;

    for segment in &transcript.transcription {
        // A segment with no token detail at all (plain `--output-json`) still
        // has text and bounds; treat the whole thing as one timed word so the
        // caller degrades rather than silently producing nothing.
        if segment.tokens.is_empty() {
            let text = segment.text.trim();
            if !text.is_empty() && !is_special(text) {
                words.push(Word {
                    text: text.to_string(),
                    start_ms: segment.offsets.from,
                    end_ms: segment.offsets.to,
                    confidence: 1.0,
                });
                acc = None;
            }
            continue;
        }

        for token in &segment.tokens {
            if is_special(&token.text) {
                continue;
            }
            let starts_word = token.text.starts_with(' ');
            let piece = token.text.trim();
            if piece.is_empty() {
                continue;
            }

            match words.last_mut() {
                // A token without a leading space continues the previous word —
                // this is how `" coun" + "try" + ","` becomes `country,`.
                Some(last) if !starts_word => {
                    last.text.push_str(piece);
                    last.end_ms = token.offsets.to.max(last.end_ms);
                    let (sum, n) = acc.get_or_insert((0.0, 0));
                    *sum += token.p;
                    *n += 1;
                    last.confidence = *sum / *n as f32;
                }
                _ => {
                    if let Some((sum, n)) = acc.take() {
                        if let Some(previous) = words.last_mut() {
                            previous.confidence = sum / n.max(1) as f32;
                        }
                    }
                    acc = Some((token.p, 1));
                    words.push(Word {
                        text: piece.to_string(),
                        start_ms: token.offsets.from,
                        end_ms: token.offsets.to,
                        confidence: token.p,
                    });
                }
            }
        }
    }

    // Whisper occasionally emits a zero-width or inverted span; a caption built
    // from one would be invalid SRT, so clamp as we go.
    for word in &mut words {
        if word.end_ms <= word.start_ms {
            word.end_ms = word.start_ms + 1;
        }
    }
    words
}

/// Group words into captions of at most `max_words`.
///
/// `max_words` is a ceiling rather than a quota. A caption also ends at a
/// sentence boundary once it is at least half full, at a silence longer than
/// [`GAP_BREAK_MS`], and at [`MAX_DURATION_MS`] — because a subtitle that runs
/// past the sentence it belongs to, or hangs through a pause, reads worse than a
/// short one. Nothing ever exceeds `max_words`.
pub fn captions(words: &[Word], max_words: usize) -> Vec<Caption> {
    let limit = max_words.max(1);
    let mut out: Vec<Caption> = Vec::new();
    let mut current: Vec<&Word> = Vec::new();

    let flush = |group: &mut Vec<&Word>, out: &mut Vec<Caption>| {
        if group.is_empty() {
            return;
        }
        let start = group[0].start_ms;
        let end = group[group.len() - 1].end_ms;
        let text = group
            .iter()
            .map(|w| w.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        let confidence =
            group.iter().map(|w| w.confidence).sum::<f32>() / group.len() as f32;

        out.push(Caption {
            start_ms: start,
            end_ms: end.max(start + 1),
            text,
            confidence,
        });
        group.clear();
    };

    for (i, word) in words.iter().enumerate() {
        current.push(word);

        let full = current.len() >= limit;
        let half_full = current.len() * 2 >= limit;
        let sentence_end = half_full && ends_sentence(&word.text);
        let too_long = word.end_ms - current[0].start_ms >= MAX_DURATION_MS;
        let gap_next = words
            .get(i + 1)
            .is_some_and(|next| next.start_ms - word.end_ms >= GAP_BREAK_MS);

        if full || sentence_end || too_long || gap_next {
            flush(&mut current, &mut out);
        }
    }
    flush(&mut current, &mut out);

    // Stretch anything too brief to read, but never into the next caption.
    for i in 0..out.len() {
        if out[i].end_ms - out[i].start_ms >= MIN_DURATION_MS {
            continue;
        }
        let ceiling = out
            .get(i + 1)
            .map(|next| next.start_ms)
            .unwrap_or(i64::MAX);
        let wanted = out[i].start_ms + MIN_DURATION_MS;
        out[i].end_ms = wanted.min(ceiling).max(out[i].end_ms);
    }

    out
}

/// `HH:MM:SS,mmm` — SRT uses a comma before the milliseconds, not a dot.
pub fn timestamp(ms: i64) -> String {
    let ms = ms.max(0);
    let hours = ms / 3_600_000;
    let minutes = (ms % 3_600_000) / 60_000;
    let seconds = (ms % 60_000) / 1_000;
    format!("{hours:02}:{minutes:02}:{seconds:02},{:03}", ms % 1_000)
}

/// Render captions as an SRT document.
///
/// CRLF line endings: SRT's de-facto spec is a Windows format and several
/// players (older VLC builds, most hardware players) mis-parse LF-only files.
pub fn render(captions: &[Caption]) -> String {
    let mut out = String::new();
    for (i, caption) in captions.iter().enumerate() {
        out.push_str(&format!("{}\r\n", i + 1));
        out.push_str(&format!(
            "{} --> {}\r\n",
            timestamp(caption.start_ms),
            timestamp(caption.end_ms)
        ));
        out.push_str(&caption.text);
        out.push_str("\r\n\r\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(text: &str, from: i64, to: i64) -> Token {
        Token {
            text: text.to_string(),
            offsets: Offsets { from, to },
            p: 0.9,
        }
    }

    fn transcript(tokens: Vec<Token>) -> Transcript {
        let to = tokens.last().map(|t| t.offsets.to).unwrap_or(0);
        Transcript {
            transcription: vec![Segment {
                tokens,
                text: String::new(),
                offsets: Offsets { from: 0, to },
            }],
            result: ResultInfo::default(),
        }
    }

    #[test]
    fn bpe_fragments_are_reassembled_into_words() {
        // Exactly how whisper splits it: a word can arrive in three pieces, and
        // the trailing comma is its own token with no leading space.
        let t = transcript(vec![
            token("[_BEG_]", 0, 0),
            token(" your", 100, 300),
            token(" coun", 300, 500),
            token("try", 500, 700),
            token(",", 700, 720),
            token(" ask", 900, 1100),
        ]);

        let words = words_from(&t);
        let text: Vec<_> = words.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(text, vec!["your", "country,", "ask"]);

        // The reassembled word must span the whole run of its fragments.
        assert_eq!(words[1].start_ms, 300);
        assert_eq!(words[1].end_ms, 720);
        // The control marker contributed no word.
        assert!(!words.iter().any(|w| w.text.contains("BEG")));
    }

    #[test]
    fn captions_never_exceed_the_requested_word_count() {
        let words: Vec<Word> = (0..20)
            .map(|i| Word {
                text: format!("w{i}"),
                start_ms: i * 200,
                end_ms: i * 200 + 150,
                confidence: 0.9,
            })
            .collect();

        for limit in [1usize, 3, 5, 7] {
            let caps = captions(&words, limit);
            assert!(
                caps.iter().all(|c| c.text.split(' ').count() <= limit),
                "limit {limit} was exceeded",
            );
            // Every word survives the grouping.
            let total: usize = caps.iter().map(|c| c.text.split(' ').count()).sum();
            assert_eq!(total, 20, "words were dropped at limit {limit}");
        }
    }

    #[test]
    fn a_silence_ends_the_caption_early() {
        let words = vec![
            Word { text: "one".into(), start_ms: 0, end_ms: 200, confidence: 0.9 },
            Word { text: "two".into(), start_ms: 200, end_ms: 400, confidence: 0.9 },
            // Two seconds of nothing: a caption spanning this would hang on
            // screen through the pause.
            Word { text: "three".into(), start_ms: 2400, end_ms: 2600, confidence: 0.9 },
            Word { text: "four".into(), start_ms: 2600, end_ms: 2800, confidence: 0.9 },
        ];

        let caps = captions(&words, 10);
        assert_eq!(caps.len(), 2);
        assert_eq!(caps[0].text, "one two");
        assert_eq!(caps[1].text, "three four");
    }

    #[test]
    fn a_sentence_ending_breaks_once_the_caption_is_half_full() {
        let words = vec![
            Word { text: "Hello".into(), start_ms: 0, end_ms: 200, confidence: 0.9 },
            Word { text: "there.".into(), start_ms: 200, end_ms: 400, confidence: 0.9 },
            Word { text: "Next".into(), start_ms: 400, end_ms: 600, confidence: 0.9 },
            Word { text: "one".into(), start_ms: 600, end_ms: 800, confidence: 0.9 },
        ];

        let caps = captions(&words, 4);
        assert_eq!(caps[0].text, "Hello there.");
        assert_eq!(caps[1].text, "Next one");
    }

    #[test]
    fn timestamps_use_srt_comma_form_and_pad() {
        assert_eq!(timestamp(0), "00:00:00,000");
        assert_eq!(timestamp(10_500), "00:00:10,500");
        assert_eq!(timestamp(3_661_007), "01:01:01,007");
        // Negative offsets would render as garbage; they clamp instead.
        assert_eq!(timestamp(-5), "00:00:00,000");
    }

    #[test]
    fn rendered_srt_is_well_formed() {
        let caps = captions(
            &[
                Word { text: "And".into(), start_ms: 320, end_ms: 400, confidence: 0.9 },
                Word { text: "so".into(), start_ms: 400, end_ms: 520, confidence: 0.9 },
            ],
            2,
        );
        let srt = render(&caps);

        assert!(srt.starts_with("1\r\n00:00:00,320 --> "));
        assert!(srt.contains("And so"));
        assert!(srt.ends_with("\r\n\r\n"));
        // Blocks are CRLF-separated throughout, not just at the join.
        assert!(!srt.contains("\n\n\n"));
    }

    #[test]
    fn a_too_brief_caption_is_stretched_but_never_overlaps_the_next() {
        let words = vec![
            Word { text: "hi".into(), start_ms: 0, end_ms: 80, confidence: 0.9 },
            Word { text: "there".into(), start_ms: 300, end_ms: 380, confidence: 0.9 },
        ];
        let caps = captions(&words, 1);

        assert_eq!(caps.len(), 2);
        // Stretched toward the readable minimum, but stopped at the next start.
        assert_eq!(caps[0].end_ms, 300);
        assert!(caps[0].end_ms <= caps[1].start_ms, "captions overlap");
        // The last one has nothing after it, so it gets the full minimum.
        assert_eq!(caps[1].end_ms, 300 + MIN_DURATION_MS);
    }

    /// Real `--output-json-full` output from the vendored whisper.cpp build,
    /// captured by transcribing the project's sample clip. Synthetic tokens can
    /// drift from what whisper actually emits; this cannot.
    const JFK: &str = include_str!("../tests/fixtures/jfk-whisper.json");

    #[test]
    fn real_whisper_output_becomes_well_formed_captions() {
        let transcript: Transcript = serde_json::from_str(JFK).expect("fixture parses");
        let words = words_from(&transcript);

        let sentence = words
            .iter()
            .map(|w| w.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(
            sentence,
            "And so my fellow Americans ask not what your country can do for you, \
             ask what you can do for your country."
        );

        for limit in [1usize, 3, 5, 7, 12] {
            let caps = captions(&words, limit);

            // The requested ceiling holds on real data, not just tidy inputs.
            for cap in &caps {
                assert!(
                    cap.text.split(' ').count() <= limit,
                    "limit {limit} exceeded by {:?}",
                    cap.text
                );
            }

            // Every word survives, in order.
            let rebuilt = caps
                .iter()
                .map(|c| c.text.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            assert_eq!(rebuilt, sentence, "words lost or reordered at limit {limit}");

            // Captions must advance and never overlap, or players misbehave.
            for pair in caps.windows(2) {
                assert!(pair[0].end_ms <= pair[1].start_ms, "overlap at limit {limit}");
            }
            for cap in &caps {
                assert!(cap.end_ms > cap.start_ms, "empty span at limit {limit}");
            }

            // And the rendered document is parseable as SRT.
            let srt = render(&caps);
            let blocks: Vec<&str> = srt.trim_end().split("\r\n\r\n").collect();
            assert_eq!(blocks.len(), caps.len());
            for (i, block) in blocks.iter().enumerate() {
                let mut lines = block.split("\r\n");
                assert_eq!(lines.next().unwrap(), (i + 1).to_string());
                assert!(lines.next().unwrap().contains(" --> "));
            }
        }
    }

    #[test]
    fn words_per_caption_actually_changes_the_grouping() {
        let transcript: Transcript = serde_json::from_str(JFK).expect("fixture parses");
        let words = words_from(&transcript);

        // The setting is the point of the feature: a smaller ceiling must
        // produce strictly more captions on the same audio.
        let counts: Vec<usize> = [1usize, 3, 7, 12]
            .iter()
            .map(|n| captions(&words, *n).len())
            .collect();

        assert_eq!(counts[0], words.len(), "one word per caption should split every word");
        assert!(
            counts.windows(2).all(|w| w[0] >= w[1]),
            "caption count should fall as the limit rises: {counts:?}"
        );
        assert!(counts[0] > counts[3], "the setting had no effect: {counts:?}");
    }

    #[test]
    fn segments_without_token_detail_still_produce_captions() {
        let t = Transcript {
            transcription: vec![Segment {
                tokens: vec![],
                text: " Plain segment text.".into(),
                offsets: Offsets { from: 500, to: 2000 },
            }],
            result: ResultInfo::default(),
        };

        let words = words_from(&t);
        assert_eq!(words.len(), 1);
        assert_eq!(words[0].text, "Plain segment text.");
        assert_eq!(words[0].start_ms, 500);
    }
}
