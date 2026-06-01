use nu_parser::{Token, TokenContents, lex};
use nu_protocol::{
    ParseError,
    config::{AbbrPosition, AbbreviationDef},
};
use reedline::{AbbreviationExpansion, AbbreviationTrigger, Abbreviator};
use std::{collections::HashMap, ops::Range};

/// Prefix words after which the next token is treated as command-position.
const DECORATORS: &[&str] = &["exec", "not"];

fn is_abbr_token_boundary(c: char) -> bool {
    c.is_whitespace() || matches!(c, '(' | ')' | '{' | '}' | '[' | ']' | '|' | ';')
}

fn prefix_ends_with_match_arm_arrow(prefix: &[u8]) -> bool {
    let Some(end) = prefix.iter().rposition(|b| !b.is_ascii_whitespace()) else {
        return false;
    };
    if end == 0 || &prefix[(end - 1)..=end] != b"=>" {
        return false;
    }

    let (tokens, _) = lex(&prefix[..(end - 1)], 0, &[], &[], false);
    let mut in_match = false;
    let mut match_block_depth = 0usize;
    for tok in tokens {
        if tok.contents != TokenContents::Item {
            continue;
        }

        let bytes = &prefix[tok.span.start..tok.span.end];
        if bytes == b"match" {
            in_match = true;
            match_block_depth = 0;
            continue;
        }

        if in_match {
            match bytes.first().copied() {
                Some(b'{') => match_block_depth += 1,
                Some(b'}') => match_block_depth = match_block_depth.saturating_sub(1),
                _ => {}
            }
        }
    }

    in_match && match_block_depth > 0
}

fn is_file_redirection_token(contents: &TokenContents) -> bool {
    matches!(
        contents,
        TokenContents::OutGreaterThan
            | TokenContents::OutGreaterGreaterThan
            | TokenContents::ErrGreaterThan
            | TokenContents::ErrGreaterGreaterThan
            | TokenContents::OutErrGreaterThan
            | TokenContents::OutErrGreaterGreaterThan
    )
}

fn is_pipe_redirection_token(contents: &TokenContents) -> bool {
    matches!(
        contents,
        TokenContents::ErrGreaterPipe | TokenContents::OutErrGreaterPipe
    )
}

fn is_segment_boundary(contents: &TokenContents) -> bool {
    matches!(
        contents,
        TokenContents::Pipe
            | TokenContents::Semicolon
            | TokenContents::Eol
            | TokenContents::AssignmentOperator
            | TokenContents::PipePipe
    )
}

struct LexedPrefix<'a> {
    bytes: &'a [u8],
    tokens: Vec<Token>,
    error: Option<ParseError>,
}

impl<'a> LexedPrefix<'a> {
    fn new(line: &'a str, end: usize) -> Self {
        let bytes = &line.as_bytes()[..end];
        let (tokens, error) = lex(bytes, 0, &[], &[], false);
        Self {
            bytes,
            tokens,
            error,
        }
    }

    fn item_bytes(&self, token: &Token) -> &'a [u8] {
        &self.bytes[token.span.start..token.span.end]
    }

    fn is_trailing_unclosed_delimiter(&self, index: usize, token: &Token) -> bool {
        index + 1 == self.tokens.len()
            && matches!(
                self.error,
                Some(ParseError::UnexpectedEof(_, _) | ParseError::Unclosed(_, _))
            )
            && matches!(self.item_bytes(token).first(), Some(b'{' | b'('))
    }

    fn current_segment_start(&self) -> (usize, bool) {
        let mut depth = 0usize;
        for (index, token) in self.tokens.iter().enumerate().rev() {
            match &token.contents {
                TokenContents::Comment => {}
                contents
                    if depth == 0
                        && (is_segment_boundary(contents)
                            || is_pipe_redirection_token(contents)) =>
                {
                    return (index + 1, false);
                }
                contents if depth == 0 && is_file_redirection_token(contents) => {
                    return (index + 1, true);
                }
                TokenContents::Item => match self.item_bytes(token).first().copied() {
                    Some(b'}' | b')') => depth += 1,
                    Some(b'{' | b'(') if self.is_trailing_unclosed_delimiter(index, token) => {
                        if depth == 0 {
                            return (index + 1, false);
                        }
                        depth = depth.saturating_sub(1);
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        (0, false)
    }

    fn segment_words(&self, start: usize) -> Vec<&'a str> {
        self.tokens[start..]
            .iter()
            .filter(|token| token.contents == TokenContents::Item)
            .filter_map(|token| {
                let bytes = self.item_bytes(token);
                let is_delimiter_only =
                    bytes.len() == 1 && matches!(bytes[0], b'{' | b'}' | b'(' | b')');
                (!is_delimiter_only)
                    .then(|| std::str::from_utf8(bytes).ok())
                    .flatten()
            })
            .collect()
    }
}

/// Nu abbreviator implementing shell-style abbreviation expansion.
///
/// Supports:
/// - **Exact-match** abbreviations with `position: command` (default) or
///   `position: anywhere`.
/// - **Cursor placement**: the `cursor_marker` field names the string to
///   locate in the expansion; it is stripped and its byte offset returned
///   to reedline for cursor positioning.
pub struct NuAbbreviator {
    entries: HashMap<String, AbbreviationDef>,
}

impl NuAbbreviator {
    pub fn new(entries: HashMap<String, AbbreviationDef>) -> Self {
        Self { entries }
    }

    /// Extract the cursor marker from `def.expansion`, returning the cleaned
    /// expansion text and an optional byte offset for cursor placement.
    ///
    /// Standard cursor-marker semantics: the first occurrence of
    /// `cursor_marker` is removed; its byte offset within the expansion text
    /// (before removal) is returned.  When `cursor_marker` is `None`, the
    /// expansion is returned as-is with no cursor offset.
    fn compute_expansion(def: &AbbreviationDef) -> (String, Option<usize>) {
        match &def.cursor_marker {
            None => (def.expansion.clone(), None),
            Some(marker) => {
                if let Some(pos) = def.expansion.find(marker.as_str()) {
                    let mut text = def.expansion.clone();
                    text.replace_range(pos..(pos + marker.len()), "");
                    (text, Some(pos))
                } else {
                    // Marker not found — expansion returned unchanged, no cursor.
                    (def.expansion.clone(), None)
                }
            }
        }
    }

    fn is_inside_string_item(&self, line: &str, token_start: usize) -> bool {
        let (full_tokens, _) = lex(line.as_bytes(), 0, &[], &[], false);
        for tok in &full_tokens {
            if tok.contents != TokenContents::Item {
                continue;
            }
            let ts = tok.span.start;
            let te = tok.span.end;
            if ts <= token_start && token_start < te {
                let b = line.as_bytes();
                let first = b.get(ts).copied().unwrap_or(0);
                let second = b.get(ts + 1).copied().unwrap_or(0);
                return matches!(first, b'"' | b'\'' | b'`')
                    || (first == b'$' && matches!(second, b'"' | b'\'' | b'`'))
                    || (first == b'r' && second == b'#');
            }
        }
        false
    }

    fn token_at_cursor(&self, line: &str, cursor: usize) -> Option<(String, Range<usize>)> {
        if !line.is_char_boundary(cursor) {
            return None;
        }

        let (tokens, _) = lex(line.as_bytes(), 0, &[], &[], false);
        for tok in tokens {
            if tok.contents != TokenContents::Item {
                continue;
            }

            if !(tok.span.start < cursor && cursor <= tok.span.end) {
                continue;
            }

            if cursor < tok.span.end
                && line[cursor..]
                    .chars()
                    .next()
                    .is_some_and(|c| !is_abbr_token_boundary(c))
            {
                continue;
            }

            let item_prefix = &line[tok.span.start..cursor];
            let word_start = item_prefix
                .char_indices()
                .rev()
                .find(|(_, c)| is_abbr_token_boundary(*c))
                .map_or(tok.span.start, |(i, c)| tok.span.start + i + c.len_utf8());
            if word_start == cursor {
                continue;
            }

            let range = word_start..cursor;
            return Some((line[range.clone()].to_string(), range));
        }

        None
    }

    fn is_command_position(&self, line: &str, token_range: Range<usize>) -> bool {
        let prefix = LexedPrefix::new(line, token_range.start);
        if prefix_ends_with_match_arm_arrow(prefix.bytes) {
            return true;
        }

        let (segment_start, after_redirection) = prefix.current_segment_start();
        !after_redirection
            && prefix
                .segment_words(segment_start)
                .into_iter()
                .all(|word| DECORATORS.contains(&word))
    }

    fn expand_token(
        &self,
        line: &str,
        token: &str,
        token_range: Range<usize>,
    ) -> Option<(String, Option<usize>)> {
        if self.is_inside_string_item(line, token_range.start) {
            return None;
        }

        if let Some(def) = self.entries.get(token) {
            let should_expand = match def.position {
                AbbrPosition::Anywhere => true,
                AbbrPosition::Command => self.is_command_position(line, token_range.clone()),
            };

            if should_expand {
                return Some(Self::compute_expansion(def));
            }
        }

        None
    }

    #[cfg(test)]
    fn expand(
        &self,
        line: &str,
        token: &str,
        token_range: Range<usize>,
    ) -> Option<(String, Option<usize>)> {
        self.expand_token(line, token, token_range)
    }
}

impl Abbreviator for NuAbbreviator {
    fn expand_at_cursor(
        &self,
        line: &str,
        cursor: usize,
        trigger: AbbreviationTrigger,
    ) -> Option<AbbreviationExpansion> {
        let (token, token_range) = self.token_at_cursor(line, cursor)?;
        let (mut replacement, cursor) = self.expand_token(line, &token, token_range.clone())?;
        if trigger == AbbreviationTrigger::Space && cursor.is_none() {
            replacement.push(' ');
        }

        Some(AbbreviationExpansion {
            replace_range: token_range,
            replacement,
            cursor,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NuHighlighter;
    use nu_protocol::engine::{EngineState, Stack};
    use reedline::{EditCommand, Reedline};
    use std::sync::Arc;

    fn make_abbreviator(position: AbbrPosition, pairs: &[(&str, &str)]) -> NuAbbreviator {
        NuAbbreviator::new(
            pairs
                .iter()
                .map(|(k, v)| {
                    (
                        k.to_string(),
                        AbbreviationDef {
                            expansion: v.to_string(),
                            position,
                            cursor_marker: None,
                        },
                    )
                })
                .collect(),
        )
    }

    fn make_command(pairs: &[(&str, &str)]) -> NuAbbreviator {
        make_abbreviator(AbbrPosition::Command, pairs)
    }

    fn make_anywhere(pairs: &[(&str, &str)]) -> NuAbbreviator {
        make_abbreviator(AbbrPosition::Anywhere, pairs)
    }

    fn make_reedline(position: AbbrPosition, pairs: &[(&str, &str)]) -> Reedline {
        Reedline::create()
            .with_highlighter(Box::new(NuHighlighter::new(
                Arc::new(EngineState::new()),
                Arc::new(Stack::new()),
            )))
            .with_abbreviator(Box::new(make_abbreviator(position, pairs)))
    }

    fn make_with_marker(key: &str, expansion: &str, marker: Option<&str>) -> NuAbbreviator {
        let abbr = [(
            key.to_string(),
            AbbreviationDef {
                expansion: expansion.to_string(),
                position: AbbrPosition::Anywhere,
                cursor_marker: marker.map(str::to_string),
            },
        )]
        .into_iter()
        .collect();
        NuAbbreviator::new(abbr)
    }

    #[test]
    fn command_position_cases() {
        let abbr = make_command(&[("gc", "git checkout")]);
        let expand_cases = [
            ("gc", 0..2),
            ("ls | gc", 5..7),
            ("echo hi; gc", 9..11),
            ("echo (gc)", 6..8),
            ("do { gc", 5..7),
            ("ls | each { gc", 12..14),
            ("match x { _ => gc", 15..17),
            ("not gc", 4..6),
            ("let x = gc", 8..10),
        ];

        for (line, range) in expand_cases {
            assert_eq!(
                abbr.expand(line, "gc", range),
                Some(("git checkout".into(), None)),
                "{line}"
            );
        }

        let no_expand_cases = [
            ("echo gc", 5..7),
            ("sudo gc", 5..7),
            ("env gc", 4..6),
            ("and gc", 4..6),
            ("or gc", 3..5),
            ("time gc", 5..7),
            ("if gc", 3..5),
            ("while gc", 6..8),
            ("overlay use gc", 12..14),
            ("save --force > gc", 15..17),
            ("where name == gc", 14..16),
            ("echo => gc", 8..10),
            ("=> gc", 3..5),
            ("^gc", 0..3),
        ];

        for (line, range) in no_expand_cases {
            assert_eq!(
                abbr.expand(line, &line[range.clone()], range),
                None,
                "{line}"
            );
        }

        let pipe_redirection = "some-command err>| gc";
        assert_eq!(
            abbr.expand(
                pipe_redirection,
                "gc",
                pipe_redirection.find("gc").unwrap()..pipe_redirection.len(),
            ),
            Some(("git checkout".into(), None))
        );
    }

    #[test]
    fn expand_at_cursor_uses_nu_token_ranges() {
        let abbr = make_command(&[("gc", "git checkout")]);
        let expand_cases = [
            ("gc|main", 2, 0..2),
            ("do { gc", "do { gc".len(), 5..7),
            ("ls | each { gc", "ls | each { gc".len(), 12..14),
            ("echo (gc", "echo (gc".len(), 6..8),
            ("match x { _ => gc", "match x { _ => gc".len(), 15..17),
        ];

        for (line, cursor, replace_range) in expand_cases {
            assert_eq!(
                <NuAbbreviator as Abbreviator>::expand_at_cursor(
                    &abbr,
                    line,
                    cursor,
                    AbbreviationTrigger::Space,
                ),
                Some(AbbreviationExpansion {
                    replace_range,
                    replacement: "git checkout ".into(),
                    cursor: None,
                }),
                "{line}"
            );
        }

        assert_eq!(
            <NuAbbreviator as Abbreviator>::expand_at_cursor(
                &abbr,
                "echo (1) gc",
                "echo (1) gc".len(),
                AbbreviationTrigger::Space,
            ),
            None
        );
    }

    #[test]
    fn anywhere_expands_in_arguments_but_not_strings() {
        let abbr = make_anywhere(&[("yin", "yang")]);
        assert_eq!(
            abbr.expand("echo yin", "yin", 5..8),
            Some(("yang".into(), None))
        );

        let abbr = make_anywhere(&[("gc", "git checkout")]);
        for (line, range) in [
            ("echo \"gc\"", 6..8),
            ("$\"(gc)\"", 3..5),
            ("echo r#'gc'#", 8..10),
            ("`gc`", 1..3),
        ] {
            assert_eq!(abbr.expand(line, "gc", range), None, "{line}");
        }
    }

    #[test]
    fn cursor_marker_cases() {
        let cases = [
            (
                "gcm",
                "git commit -m '%'",
                Some("%"),
                "git commit -m ''",
                Some(15),
            ),
            ("tmpl", "a!b!c", Some("!"), "ab!c", Some(1)),
            ("x", "git checkout", Some("!"), "git checkout", None),
            ("x", "hi", Some("--long-marker--"), "hi", None),
            ("pipe", "% | less", None, "% | less", None),
            ("jp", "résumé!", Some("!"), "résumé", Some(8)),
        ];

        for (key, expansion, marker, expected, cursor) in cases {
            let abbr = make_with_marker(key, expansion, marker);
            assert_eq!(
                abbr.expand(key, key, 0..key.len()),
                Some((expected.into(), cursor)),
                "{key}"
            );
        }
    }

    #[test]
    fn unicode_exact_matching() {
        let abbr = make_command(&[("café", "coffee shop")]);
        assert_eq!(
            abbr.expand("café", "café", 0.."café".len()),
            Some(("coffee shop".into(), None))
        );
        assert_eq!(abbr.expand("caf", "caf", 0..3), None);
    }

    #[test]
    fn reedline_highlighter_suppresses_anywhere_abbr_in_external_args() {
        let mut line_editor = make_reedline(AbbrPosition::Anywhere, &[("g", "git")]);

        line_editor.run_edit_commands(&[EditCommand::InsertString("bash -c g".into())]);
        line_editor.run_edit_commands(&[EditCommand::InsertChar(' ')]);

        assert_eq!(line_editor.current_buffer_contents(), "bash -c g ");
    }

    #[test]
    fn reedline_highlighter_allows_anywhere_abbr_in_nu_expression_args() {
        let mut line_editor = make_reedline(AbbrPosition::Anywhere, &[("g", "git")]);

        line_editor.run_edit_commands(&[EditCommand::InsertString("let x = g".into())]);
        line_editor.run_edit_commands(&[EditCommand::InsertChar(' ')]);

        assert_eq!(line_editor.current_buffer_contents(), "let x = git ");
    }
}
