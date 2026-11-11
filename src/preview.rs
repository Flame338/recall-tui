use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use crate::{
    lux::{MARK_END, MARK_START},
    shelf::{self, Request, Source},
    view::sanitize_for_display,
};

pub(super) fn build_preview(source: &Source, key: &str, request: &Request) -> Vec<Line<'static>> {
    match source.read(key) {
        Ok(body) => {
            let marked = shelf::snippet(source, &body, request, body.len().max(1)).unwrap_or(body);
            preview_lines(&marked)
        }
        Err(why) => vec![Line::from(format!("({why})"))],
    }
}

fn preview_lines(marked: &str) -> Vec<Line<'static>> {
    let numbered = marked.lines().enumerate();
    let matching: Vec<(usize, &str)> = numbered
        .clone()
        .filter(|(_, line)| line.contains(MARK_START))
        .take(8)
        .collect();
    let chosen: Vec<(usize, &str)> = if matching.is_empty() {
        numbered
            .filter(|(_, line)| !line.trim().is_empty())
            .take(8)
            .collect()
    } else {
        matching
    };
    if chosen.is_empty() {
        return vec![Line::from("(No preview text)")];
    }
    chosen
        .into_iter()
        .map(|(number, line)| {
            let line = sanitize_for_display(line);
            let excerpt = truncate_utf8(&line, 400);
            let mut spans = vec![Span::styled(
                format!("{:>4} │ ", number + 1),
                Style::default().fg(Color::DarkGray),
            )];
            spans.extend(marked_spans(excerpt));
            Line::from(spans)
        })
        .collect()
}

fn marked_spans(line: &str) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut text = String::new();
    let mut marked = false;
    for ch in line.chars() {
        if ch == MARK_START || ch == MARK_END {
            if !text.is_empty() {
                spans.push(match_span(std::mem::take(&mut text), marked));
            }
            marked = ch == MARK_START;
        } else {
            text.push(ch);
        }
    }
    if !text.is_empty() {
        spans.push(match_span(text, marked));
    }
    spans
}

fn match_span(text: String, marked: bool) -> Span<'static> {
    if marked {
        Span::styled(
            text,
            Style::default()
                .fg(Color::Cyan)
                .bg(Color::Rgb(36, 53, 68))
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::raw(text)
    }
}

fn truncate_utf8(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut boundary = max_bytes;
    while !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    &text[..boundary]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        lux::{self, LuxIndex},
        shelf::{Mode, Project, Reach, parse_request},
        testing::picker_dir,
    };
    use ratatui::{
        buffer::Buffer,
        layout::Rect,
        widgets::{Paragraph, Widget, Wrap},
    };

    fn rendered_symbols(text: &str) -> Vec<String> {
        let area = Rect::new(0, 0, 40, 6);
        let mut buffer = Buffer::empty(area);
        Paragraph::new(preview_lines(text))
            .wrap(Wrap { trim: true })
            .render(area, &mut buffer);
        buffer
            .content
            .iter()
            .map(|cell| cell.symbol().to_owned())
            .collect()
    }

    fn marked(text: &str) -> String {
        text.replace('[', &MARK_START.to_string())
            .replace(']', &MARK_END.to_string())
    }

    fn texts(line: &Line) -> Vec<(String, bool)> {
        line.spans
            .iter()
            .skip(1)
            .map(|span| (span.content.to_string(), span.style.bg.is_some()))
            .collect()
    }

    #[test]
    fn marker_characters_become_highlighted_spans() {
        let spans = marked_spans(&marked("a[lo]b"));
        let seen: Vec<(String, bool)> = spans
            .iter()
            .map(|span| (span.content.to_string(), span.style.bg.is_some()))
            .collect();
        assert_eq!(
            seen,
            [
                ("a".into(), false),
                ("lo".into(), true),
                ("b".into(), false)
            ]
        );
    }

    #[test]
    fn preview_shows_matching_lines_with_their_numbers_and_falls_back_to_the_top() {
        let hit = preview_lines(&marked("one\ntwo [lo]\nthree\nfour [lo]"));
        assert_eq!(hit.len(), 2);
        assert_eq!(hit[0].spans[0].content.trim(), "2 │");
        assert_eq!(hit[1].spans[0].content.trim(), "4 │");
        let none = preview_lines("\n\nalpha\nbeta");
        assert_eq!(none.len(), 2);
        assert_eq!(none[0].spans[0].content.trim(), "3 │");
    }

    #[test]
    fn luxcore_snippets_mark_prefix_completions_and_keep_line_numbers() {
        let mut index =
            LuxIndex::load(&lux::library_path()).expect("build libluxcore with `make shared`");
        index
            .add(
                "note.md",
                "note.md",
                "first line\n\twhile we are testing grounds\nlast line\n",
            )
            .unwrap();
        let root = picker_dir("preview-note");
        std::fs::write(
            root.join("note.md"),
            "first line\n\twhile we are testing grounds\nlast line\n",
        )
        .unwrap();
        let project = Project::at(&root);
        let source = Source {
            project: &project,
            index: &index,
            here: true,
        };
        let request = parse_request("testin", Mode::Fuzzy, Reach::Here);
        let lines = build_preview(&source, "note.md", &request);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans[0].content.trim(), "2 │");
        let seen = texts(&lines[0]);
        assert!(
            seen.iter().any(|(text, hot)| *hot && text == "testing"),
            "expected `testing` highlighted, got {seen:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_hit_whose_file_is_gone_previews_as_one_line_that_says_so() {
        let root = picker_dir("preview-gone");
        let mut index = LuxIndex::load(&lux::library_path()).expect("build libluxcore");
        index.add("note.md", "note.md", "testing grounds").unwrap();
        let project = Project::at(&root);
        let source = Source {
            project: &project,
            index: &index,
            here: true,
        };
        let request = parse_request("testing", Mode::Fuzzy, Reach::Here);
        let gone = build_preview(&source, "note.md", &request);
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(gone.len(), 1);
        assert!(
            gone[0].to_string().contains("no longer on disk"),
            "{gone:?}"
        );
    }

    #[test]
    fn preview_never_writes_control_characters_to_cells() {
        let body = "\twhile (lo < hi) {\n\t\tlo = 1;\x1b[31m\x08\x07\u{7f}\n\u{85}x lo\r\n";
        for symbol in rendered_symbols(body) {
            assert!(
                !symbol.chars().any(char::is_control),
                "control character reached the terminal buffer: {symbol:?}"
            );
        }
    }
}
