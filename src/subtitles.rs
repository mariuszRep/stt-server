//! SubRip (`srt`) and WebVTT (`vtt`) rendering for `/v1/audio/transcriptions` and
//! `/v1/audio/translations`. Pure: takes timed segments, returns the file body.

/// One timed piece of the transcript, in milliseconds from the start of the audio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cue {
    pub start_ms: i64,
    pub end_ms: i64,
    pub text: String,
}

/// A cue's text as subtitle lines: lines trimmed, blank lines dropped (a blank line ends a cue in
/// both formats). `None` when nothing is left, so the cue is skipped.
fn cue_lines(text: &str) -> Option<Vec<&str>> {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    (!lines.is_empty()).then_some(lines)
}

/// `HH:MM:SS<separator>mmm`; hours are not capped (`100:00:00` stays valid for a long recording).
fn timestamp(ms: i64, separator: char) -> String {
    let ms = ms.max(0);
    format!(
        "{:02}:{:02}:{:02}{separator}{:03}",
        ms / 3_600_000,
        ms / 60_000 % 60,
        ms / 1000 % 60,
        ms % 1000
    )
}

/// WebVTT cue text may not contain `&`, `<` or `>` unescaped (they start entities and tags, and
/// `>` would allow a `-->` inside the payload).
fn escape_vtt(line: &str) -> String {
    line.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The body of an `.srt` file: numbered cues, `HH:MM:SS,mmm --> HH:MM:SS,mmm`, text lines and a
/// blank line after each. Text is written verbatim (SubRip has no escaping). Empty when there is
/// no cue.
pub fn render_srt(cues: &[Cue]) -> String {
    let mut out = String::new();
    let mut index = 0;
    for cue in cues {
        let Some(lines) = cue_lines(&cue.text) else {
            continue;
        };
        index += 1;
        out.push_str(&format!(
            "{index}\n{} --> {}\n{}\n\n",
            timestamp(cue.start_ms, ','),
            timestamp(cue.end_ms.max(cue.start_ms), ','),
            lines.join("\n")
        ));
    }
    out
}

/// The body of a `.vtt` file: the `WEBVTT` header, a blank line, then cues timed
/// `HH:MM:SS.mmm --> HH:MM:SS.mmm` with escaped text. Just the header when there is no cue.
pub fn render_vtt(cues: &[Cue]) -> String {
    let mut out = String::from("WEBVTT\n\n");
    for cue in cues {
        let Some(lines) = cue_lines(&cue.text) else {
            continue;
        };
        let text: Vec<String> = lines.into_iter().map(escape_vtt).collect();
        out.push_str(&format!(
            "{} --> {}\n{}\n\n",
            timestamp(cue.start_ms, '.'),
            timestamp(cue.end_ms.max(cue.start_ms), '.'),
            text.join("\n")
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cue(start_ms: i64, end_ms: i64, text: &str) -> Cue {
        Cue {
            start_ms,
            end_ms,
            text: text.to_string(),
        }
    }

    #[test]
    fn srt_numbers_cues_and_uses_comma_milliseconds() {
        let out = render_srt(&[cue(0, 1500, " Hello there."), cue(1500, 3250, "Second.")]);
        assert_eq!(
            out,
            "1\n00:00:00,000 --> 00:00:01,500\nHello there.\n\n\
             2\n00:00:01,500 --> 00:00:03,250\nSecond.\n\n"
        );
    }

    #[test]
    fn vtt_has_the_header_and_dot_milliseconds() {
        let out = render_vtt(&[cue(0, 1500, " Hello there."), cue(1500, 3250, "Second.")]);
        assert_eq!(
            out,
            "WEBVTT\n\n\
             00:00:00.000 --> 00:00:01.500\nHello there.\n\n\
             00:00:01.500 --> 00:00:03.250\nSecond.\n\n"
        );
    }

    #[test]
    fn timestamps_roll_over_into_hours() {
        assert_eq!(timestamp(0, ','), "00:00:00,000");
        assert_eq!(timestamp(59_999, '.'), "00:00:59.999");
        assert_eq!(timestamp(3_600_000, ','), "01:00:00,000");
        assert_eq!(timestamp(3_723_456, '.'), "01:02:03.456");
        assert_eq!(timestamp(100 * 3_600_000, ','), "100:00:00,000");
    }

    #[test]
    fn cues_past_one_hour_render_in_both_formats() {
        let cues = [cue(3_661_001, 3_662_999, "Late.")];
        assert!(render_srt(&cues).contains("01:01:01,001 --> 01:01:02,999\n"));
        assert!(render_vtt(&cues).contains("01:01:01.001 --> 01:01:02.999\n"));
    }

    #[test]
    fn a_zero_length_cue_at_zero_seconds_is_kept() {
        assert_eq!(
            render_srt(&[cue(0, 0, "Hm")]),
            "1\n00:00:00,000 --> 00:00:00,000\nHm\n\n"
        );
    }

    #[test]
    fn an_end_before_the_start_and_a_negative_start_are_clamped() {
        assert_eq!(
            render_srt(&[cue(2000, 1000, "Odd"), cue(-5, 10, "Early")]),
            "1\n00:00:02,000 --> 00:00:02,000\nOdd\n\n2\n00:00:00,000 --> 00:00:00,010\nEarly\n\n"
        );
    }

    #[test]
    fn multi_line_text_keeps_its_lines_and_drops_blank_ones() {
        let cues = [cue(0, 2000, "First line.\r\n\r\n  Second line.  \n\n")];
        assert_eq!(
            render_srt(&cues),
            "1\n00:00:00,000 --> 00:00:02,000\nFirst line.\nSecond line.\n\n"
        );
        assert_eq!(
            render_vtt(&cues),
            "WEBVTT\n\n00:00:00.000 --> 00:00:02.000\nFirst line.\nSecond line.\n\n"
        );
    }

    #[test]
    fn vtt_escapes_markup_characters_but_srt_does_not() {
        let cues = [cue(0, 1000, "Tom & Jerry <b>a --> b</b>")];
        assert_eq!(
            render_vtt(&cues),
            "WEBVTT\n\n00:00:00.000 --> 00:00:01.000\nTom &amp; Jerry &lt;b&gt;a --&gt; b&lt;/b&gt;\n\n"
        );
        assert!(render_srt(&cues).contains("\nTom & Jerry <b>a --> b</b>\n"));
    }

    #[test]
    fn empty_cues_are_skipped_and_the_numbering_stays_dense() {
        let out = render_srt(&[
            cue(0, 1000, "  "),
            cue(1000, 2000, "Kept"),
            cue(2000, 3000, "\n"),
        ]);
        assert_eq!(out, "1\n00:00:01,000 --> 00:00:02,000\nKept\n\n");
    }

    #[test]
    fn an_empty_transcript_is_an_empty_srt_and_a_header_only_vtt() {
        assert_eq!(render_srt(&[]), "");
        assert_eq!(render_vtt(&[]), "WEBVTT\n\n");
        assert_eq!(render_vtt(&[cue(0, 1000, " ")]), "WEBVTT\n\n");
    }
}
