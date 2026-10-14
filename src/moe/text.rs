//! How Flux MoE's answers look: the Markdown the model writes (**bold**, `code`,
//! headings, - and 1. lists, ``` blocks) shown as such instead of as stars and
//! backticks, and the follow-ups it suggests ("Organize my music") as buttons.

use std::sync::LazyLock;

use eframe::egui::{self, text::LayoutJob, Color32, FontId, RichText, TextFormat};
use regex::Regex;

#[derive(Debug, PartialEq)]
pub enum Block<'a> {
    Blank,
    Heading(&'a str),
    /// A list item: nesting depth, its marker ("•" or "3."), its text.
    Item(usize, String, &'a str),
    Para(&'a str),
    Code(&'a str),
}

/// "12. rest" or "12) rest" → ("12", "rest").
fn numbered(s: &str) -> Option<(&str, &str)> {
    let digits = s.chars().take_while(char::is_ascii_digit).count();
    if !(1..=3).contains(&digits) {
        return None;
    }
    let rest = &s[digits..];
    let rest = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") "))?;
    Some((&s[..digits], rest.trim_start()))
}

/// The answer, line by line.
pub fn blocks(text: &str) -> Vec<Block<'_>> {
    let mut out = Vec::new();
    let mut fence = false;
    for line in text.lines() {
        let t = line.trim_end();
        if t.trim_start().starts_with("```") {
            fence = !fence;
            continue;
        }
        if fence {
            out.push(Block::Code(t));
            continue;
        }
        let s = t.trim_start();
        let depth = (t.len() - s.len()) / 2;
        out.push(if s.is_empty() {
            Block::Blank
        } else if s.starts_with('#') {
            Block::Heading(s.trim_start_matches('#').trim())
        } else if let Some(rest) = s.strip_prefix("- ").or_else(|| s.strip_prefix("* ")).or_else(|| s.strip_prefix("• ")) {
            Block::Item(depth, "•".into(), rest.trim_start())
        } else if let Some((n, rest)) = numbered(s) {
            Block::Item(depth, format!("{n}."), rest)
        } else {
            Block::Para(s)
        });
    }
    out
}

/// Inline pieces: (text, bold, code). An unclosed marker just runs to the end.
pub fn spans(s: &str) -> Vec<(String, bool, bool)> {
    let mut out = Vec::new();
    let (mut bold, mut code) = (false, false);
    let mut buf = String::new();
    let mut flush = |buf: &mut String, bold: bool, code: bool| {
        if !buf.is_empty() {
            out.push((std::mem::take(buf), bold, code));
        }
    };
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '`' {
            flush(&mut buf, bold, code);
            code = !code;
        } else if c == '*' && !code && it.peek() == Some(&'*') {
            it.next();
            flush(&mut buf, bold, code);
            bold = !bold;
        } else {
            buf.push(c);
        }
    }
    flush(&mut buf, bold, code);
    out
}

/// Show an answer: selectable, wrapped to the width there is.
pub fn show(ui: &mut egui::Ui, text: &str) {
    let body = egui::TextStyle::Body.resolve(ui.style());
    let mono = egui::TextStyle::Monospace.resolve(ui.style());
    let (col, strong) = (ui.visuals().text_color(), ui.visuals().strong_text_color());
    let line = |ui: &mut egui::Ui, marker: &str, depth: usize, text: &str, font: FontId, all_bold: bool| {
        let mut job = LayoutJob::default();
        if !marker.is_empty() {
            job.append(&format!("{marker} "), depth as f32 * 16.0, TextFormat::simple(font.clone(), ui.visuals().weak_text_color()));
        }
        for (s, bold, code) in spans(text) {
            let mut f = TextFormat::simple(if code { mono.clone() } else { font.clone() }, if bold || all_bold { strong } else { col });
            if code {
                f.background = Color32::from_rgb(52, 56, 66);
            }
            job.append(&s, 0.0, f);
        }
        job.wrap.max_width = ui.available_width();
        ui.add(egui::Label::new(job).selectable(true));
    };
    for b in blocks(text) {
        match b {
            Block::Blank => ui.add_space(4.0),
            Block::Heading(t) => line(ui, "", 0, t, FontId::proportional(body.size + 2.0), true),
            Block::Item(depth, marker, t) => line(ui, &marker, depth, t, body.clone(), false),
            Block::Para(t) => line(ui, "", 0, t, body.clone(), false),
            Block::Code(t) => {
                ui.add(egui::Label::new(RichText::new(t).monospace().background_color(Color32::from_rgb(28, 30, 36))).selectable(true));
            }
        }
    }
}

/// Words a suggestion to Flux MoE starts with ("Organize my music", "Make a chill playlist").
const ASKS: [&str; 22] = [
    "organize", "organise", "make", "find", "play", "show", "download", "give", "tidy", "move", "list", "search", "sort", "clean",
    "build", "create", "get", "start", "pick", "open", "what", "which",
];

/// The follow-ups an answer offers: quoted requests ("Organize my music"), at most 4.
/// Quoted genres, playlist names and file names are not requests and stay text.
pub fn follow_ups(answer: &str) -> Vec<String> {
    static QUOTED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"["“]([^"“”\n]{3,60})["”]"#).unwrap());
    let mut out: Vec<String> = Vec::new();
    for c in QUOTED.captures_iter(answer) {
        let s = c[1].trim().trim_end_matches(['.', ',', '!', '?']).trim().to_string();
        let words: Vec<&str> = s.split_whitespace().collect();
        let first = words.first().map(|w| w.to_lowercase()).unwrap_or_default();
        // A file name ("Play it.mkv") is not a request either.
        let file = s.rsplit_once('.').is_some_and(|(_, ext)| (2..=4).contains(&ext.len()) && ext.chars().all(|c| c.is_ascii_alphanumeric()));
        let ok = (2..=8).contains(&words.len()) && ASKS.contains(&first.as_str()) && !s.contains(['/', '\\']) && !file;
        if ok && !out.iter().any(|o| o.eq_ignore_ascii_case(&s)) {
            out.push(s);
            if out.len() == 4 {
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANSWER: &str = "With access to your data, here are **5 killer apps**:\n\n\
        1. **Auto-organize your music**  \n   I read the genre tags (like \"Trance\", \"House\") and move albums into `Music/Trance`.\n\n\
        2. **Build custom playlists**  \n   Like \"Workout Mix\" or \"Chill Vibes\".\n\n\
        - top picks\n  - nested\n\n\
        Want to try one? Just say — e.g., \"Organize my music\" or \"Make a chill playlist.\"";

    #[test]
    fn markdown_blocks_and_spans() {
        let b = blocks(ANSWER);
        assert_eq!(b[2], Block::Item(0, "1.".into(), "**Auto-organize your music**"));
        assert!(matches!(b[3], Block::Para(_)));
        assert_eq!(b[8], Block::Item(0, "•".into(), "top picks"));
        assert_eq!(b[9], Block::Item(1, "•".into(), "nested"));
        assert_eq!(blocks("# Title\n```\nlet x = 1;\n```"), vec![Block::Heading("Title"), Block::Code("let x = 1;")]);
        assert_eq!(
            spans("move into `Music/Trance` with **care** now"),
            vec![
                ("move into ".into(), false, false),
                ("Music/Trance".into(), false, true),
                (" with ".into(), false, false),
                ("care".into(), true, false),
                (" now".into(), false, false)
            ]
        );
        assert_eq!(spans("a ** b"), vec![("a ".into(), false, false), (" b".into(), true, false)], "unclosed runs to the end");
        assert_eq!(numbered("10) ten"), Some(("10", "ten")));
        assert_eq!(numbered("2026 was"), None);
    }

    #[test]
    fn follow_ups_are_quoted_requests_only() {
        assert_eq!(follow_ups(ANSWER), vec!["Organize my music", "Make a chill playlist"]);
        assert!(follow_ups("Saved to \"Movies/Thrillers\" and \"Play it.mkv\"").is_empty(), "paths and file names");
        assert_eq!(follow_ups("Try “Find trance with 50+ seeders”!"), vec!["Find trance with 50+ seeders"]);
    }
}
