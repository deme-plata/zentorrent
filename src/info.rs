//! The Info tab: a torrent's .nfo / readme, shown the way it was meant to look.
//!
//! Scene .nfo files are DOS text (code page 437): their logos and borders are
//! box-drawing and block characters that turn into garbage as UTF-8 or Latin-1.
//! They are decoded as CP437 and shown in a monospace font, so the art lines up.
//! README.md is rendered as rich text (headings, lists, bold, code, links); other
//! .txt files are shown as they are.

use eframe::egui::{self, Color32, RichText};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Nfo,
    Markdown,
    Text,
}

pub fn kind_of(name: &str) -> Kind {
    let l = name.to_ascii_lowercase();
    if l.ends_with(".nfo") || l.ends_with(".diz") {
        Kind::Nfo
    } else if l.ends_with(".md") || l.ends_with(".markdown") {
        Kind::Markdown
    } else {
        Kind::Text
    }
}

/// Info files worth showing, best first: .nfo, then readme / .md, then small .txt.
/// `files` = (path in the torrent, size). Returns (file index, path, size).
pub fn candidates(files: &[(String, u64)]) -> Vec<(usize, String, u64)> {
    const MAX: u64 = 512 * 1024;
    let rank = |p: &str| {
        let l = p.to_ascii_lowercase();
        let name = l.rsplit('/').next().unwrap_or(&l).to_string();
        if l.ends_with(".nfo") {
            Some(0)
        } else if l.ends_with(".diz") {
            Some(1)
        } else if name.starts_with("readme") || l.ends_with(".md") {
            Some(2)
        } else if l.ends_with(".txt") {
            Some(3)
        } else {
            None
        }
    };
    let mut out: Vec<(usize, String, u64, u8)> = files
        .iter()
        .enumerate()
        .filter(|(_, (_, len))| *len > 0 && *len <= MAX)
        .filter_map(|(i, (p, len))| rank(p).map(|r| (i, p.clone(), *len, r)))
        .collect();
    out.sort_by(|a, b| a.3.cmp(&b.3).then_with(|| a.1.len().cmp(&b.1.len())));
    out.into_iter().map(|(i, p, l, _)| (i, p, l)).collect()
}

/// Code page 437, bytes 0x80–0xFF (the DOS characters scene art is drawn with).
const CP437_HIGH: [char; 128] = [
    'Ç', 'ü', 'é', 'â', 'ä', 'à', 'å', 'ç', 'ê', 'ë', 'è', 'ï', 'î', 'ì', 'Ä', 'Å', //
    'É', 'æ', 'Æ', 'ô', 'ö', 'ò', 'û', 'ù', 'ÿ', 'Ö', 'Ü', '¢', '£', '¥', '₧', 'ƒ', //
    'á', 'í', 'ó', 'ú', 'ñ', 'Ñ', 'ª', 'º', '¿', '⌐', '¬', '½', '¼', '¡', '«', '»', //
    '░', '▒', '▓', '│', '┤', '╡', '╢', '╖', '╕', '╣', '║', '╗', '╝', '╜', '╛', '┐', //
    '└', '┴', '┬', '├', '─', '┼', '╞', '╟', '╚', '╔', '╩', '╦', '╠', '═', '╬', '╧', //
    '╨', '╤', '╥', '╙', '╘', '╒', '╓', '╫', '╪', '┘', '┌', '█', '▄', '▌', '▐', '▀', //
    'α', 'ß', 'Γ', 'π', 'Σ', 'σ', 'µ', 'τ', 'Φ', 'Θ', 'Ω', 'δ', '∞', 'φ', 'ε', '∩', //
    '≡', '±', '≥', '≤', '⌠', '⌡', '÷', '≈', '°', '∙', '·', '√', 'ⁿ', '²', '■', '\u{a0}',
];

pub fn cp437(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| if b < 0x80 { b as char } else { CP437_HIGH[(b - 0x80) as usize] }).collect()
}

/// Bytes → text: an .nfo is always CP437; anything else is UTF-8 when it is, else CP437.
pub fn decode(bytes: &[u8], kind: Kind) -> String {
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    let text = match (kind, std::str::from_utf8(bytes)) {
        (Kind::Nfo, _) => cp437(bytes),
        (_, Ok(s)) => s.to_string(),
        (_, Err(_)) => cp437(bytes),
    };
    text.replace("\r\n", "\n").replace('\r', "\n").replace('\t', "    ")
}

/// Draw the document.
pub fn show(ui: &mut egui::Ui, kind: Kind, text: &str) {
    match kind {
        Kind::Nfo => art(ui, text),
        Kind::Markdown => markdown(ui, text),
        Kind::Text if looks_like_art(text) => art(ui, text),
        Kind::Text => {
            ui.add(egui::Label::new(text).wrap());
        }
    }
}

/// Box-drawing or block characters mean it is drawn, not written: keep the columns.
fn looks_like_art(text: &str) -> bool {
    text.chars().filter(|c| ('\u{2500}'..='\u{259f}').contains(c)).count() > 10
}

fn art(ui: &mut egui::Ui, text: &str) {
    egui::Frame::new().fill(Color32::from_rgb(12, 14, 18)).corner_radius(6.0).inner_margin(egui::Margin::same(10)).show(ui, |ui| {
        egui::ScrollArea::horizontal().id_salt("nfo-art").show(ui, |ui| {
            ui.add(
                egui::Label::new(RichText::new(text.trim_end()).monospace().size(11.0).color(Color32::from_rgb(190, 215, 235)))
                    .extend(),
            );
        });
    });
}

/// A small Markdown renderer: headings, lists, quotes, code blocks, **bold**, `code`, [links](url).
fn markdown(ui: &mut egui::Ui, text: &str) {
    let mut code: Option<String> = None;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            match code.take() {
                Some(block) => art(ui, &block),
                None => code = Some(String::new()),
            }
            continue;
        }
        if let Some(block) = code.as_mut() {
            block.push_str(line);
            block.push('\n');
            continue;
        }
        let t = line.trim_end();
        if t.trim().is_empty() {
            ui.add_space(6.0);
        } else if let Some((level, rest)) = heading(t) {
            let size = [22.0, 19.0, 16.5, 15.0, 14.0, 13.0][level.min(6) - 1];
            ui.add_space(4.0);
            ui.label(RichText::new(rest).size(size).strong());
        } else if let Some(item) = t.trim_start().strip_prefix("- ").or_else(|| t.trim_start().strip_prefix("* ")) {
            ui.horizontal_wrapped(|ui| {
                ui.label("  •");
                inline(ui, item);
            });
        } else if let Some(q) = t.strip_prefix("> ") {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("| ").strong().color(Color32::from_rgb(90, 120, 160)));
                inline(ui, q);
            });
        } else if t.chars().all(|c| c == '-' || c == '=' || c == '*') && t.len() >= 3 {
            ui.separator();
        } else {
            ui.horizontal_wrapped(|ui| inline(ui, t));
        }
    }
    if let Some(block) = code {
        art(ui, &block);
    }
}

fn heading(line: &str) -> Option<(usize, &str)> {
    let level = line.chars().take_while(|&c| c == '#').count();
    (1..=6).contains(&level).then(|| line[level..].strip_prefix(' ')).flatten().map(|rest| (level, rest.trim()))
}

/// One line of inline Markdown: **bold**, `code`, [text](url); everything else as is.
fn inline(ui: &mut egui::Ui, text: &str) {
    ui.spacing_mut().item_spacing.x = 0.0;
    for (kind, piece) in spans(text) {
        match kind {
            Span::Plain => {
                ui.label(piece);
            }
            Span::Bold => {
                ui.label(RichText::new(piece).strong());
            }
            Span::Italic => {
                ui.label(RichText::new(piece).italics());
            }
            Span::Code => {
                ui.label(RichText::new(piece).monospace().background_color(Color32::from_rgb(40, 44, 52)));
            }
            Span::Link(url) => {
                ui.hyperlink_to(piece, url);
            }
        }
    }
}

#[derive(Debug, PartialEq)]
enum Span {
    Plain,
    Bold,
    Italic,
    Code,
    Link(String),
}

fn spans(text: &str) -> Vec<(Span, String)> {
    let mut out = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let next = [rest.find('*'), rest.find('`'), rest.find('[')].into_iter().flatten().min();
        let Some(at) = next else {
            out.push((Span::Plain, rest.to_string()));
            break;
        };
        if at > 0 {
            out.push((Span::Plain, rest[..at].to_string()));
        }
        let tail = &rest[at..];
        let parsed = if let Some(t) = tail.strip_prefix("**") {
            t.find("**").map(|e| ((Span::Bold, t[..e].to_string()), 2 + e + 2))
        } else if let Some(t) = tail.strip_prefix('*').filter(|t| t.starts_with(|c: char| !c.is_whitespace())) {
            // *italic*: a closing star right after a non-space, and not the start of **
            t.find('*').filter(|&e| e > 0 && !t[..e].ends_with(char::is_whitespace)).map(|e| ((Span::Italic, t[..e].to_string()), 1 + e + 1))
        } else if let Some(t) = tail.strip_prefix('`') {
            t.find('`').map(|e| ((Span::Code, t[..e].to_string()), 1 + e + 1))
        } else {
            // [text](url)
            tail.find("](").and_then(|m| {
                let close = tail[m..].find(')')? + m;
                Some(((Span::Link(tail[m + 2..close].to_string()), tail[1..m].to_string()), close + 1))
            })
        };
        match parsed {
            Some((span, used)) => {
                out.push(span);
                rest = &tail[used..];
            }
            None => {
                // Not markup after all: keep the character and go on.
                let c = tail.chars().next().unwrap();
                out.push((Span::Plain, c.to_string()));
                rest = &tail[c.len_utf8()..];
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nfo_art_is_decoded_as_dos_text() {
        // ╔═══╗ / ║ █ ║ / ╚═══╝ in CP437.
        let bytes = b"\xc9\xcd\xcd\xcd\xbb\r\n\xba \xdb \xba\r\n\xc8\xcd\xcd\xcd\xbc";
        assert_eq!(decode(bytes, Kind::Nfo), "╔═══╗\n║ █ ║\n╚═══╝");
        assert_eq!(cp437(b"\x80\xff"), "Ç\u{a0}");
        assert_eq!(decode("Bj\u{f6}rk".as_bytes(), Kind::Text), "Björk", "UTF-8 text stays UTF-8");
    }

    #[test]
    fn the_best_info_file_comes_first() {
        let files = vec![
            ("Album/notes.txt".to_string(), 300),
            ("Album/01.flac".to_string(), 30_000_000),
            ("Album/README.md".to_string(), 900),
            ("Album/release.nfo".to_string(), 4_000),
            ("Album/huge.txt".to_string(), 5_000_000),
        ];
        let c: Vec<usize> = candidates(&files).into_iter().map(|c| c.0).collect();
        assert_eq!(c, [3, 2, 0], "nfo, then readme, then small txt; no media, nothing huge");
    }

    #[test]
    fn inline_markdown() {
        let s = spans("Get **ZenTorrent** at [GitHub](https://github.com/deme-plata/zentorrent) — `chmod +x` it, 2*3");
        assert_eq!(s[1], (Span::Bold, "ZenTorrent".into()));
        assert_eq!(s[3], (Span::Link("https://github.com/deme-plata/zentorrent".into()), "GitHub".into()));
        assert_eq!(s[5], (Span::Code, "chmod +x".into()));
        assert_eq!(heading("## Install"), Some((2, "Install")));
        assert_eq!(heading("#hashtag"), None);
        assert_eq!(spans("for the *Info* tab")[1], (Span::Italic, "Info".into()));
        assert!(!spans("2*3 and 4 * 5").iter().any(|s| s.0 == Span::Italic), "a lone star is just a star");
    }
}
