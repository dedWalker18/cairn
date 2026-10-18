//! Split extracted text into chunks that keep their line numbers, so a hit
//! can point at `file:line`.

use serde::{Deserialize, Serialize};

pub const TARGET_CHARS: usize = 1000;
pub const MAX_CHARS: usize = 1600;
pub const MAX_CHUNKS_PER_FILE: usize = 400;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Chunk {
    /// 1-based, inclusive.
    pub line_start: u32,
    pub line_end: u32,
    pub text: String,
}

fn is_definition_start(line: &str) -> bool {
    if line.starts_with([' ', '\t']) || line.is_empty() {
        return false;
    }
    const KW: &[&str] = &[
        "fn ", "pub ", "def ", "class ", "function ", "func ", "async ", "export ", "impl ", "struct ", "enum ", "trait ",
        "interface ", "type ", "module ", "resource ", "data ", "variable ", "output ", "provider ", "locals ", "#!", "##",
        "# ", "static ", "const ", "let ", "var ", "public ", "private ", "protected ", "package ", "sub ", "proc ",
    ];
    KW.iter().any(|k| line.starts_with(k))
}

/// Chunk text. Breaks prefer blank lines and top-level definitions; no chunk
/// exceeds MAX_CHARS (very long lines are hard-split).
pub fn chunk_text(text: &str) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let mut cur = String::new();
    let mut start_line = 1u32;
    let mut line_no = 0u32;
    let mut cur_lines = 0u32;

    let flush = |chunks: &mut Vec<Chunk>, cur: &mut String, start: u32, end: u32| {
        let trimmed = cur.trim();
        if trimmed.chars().filter(|c| c.is_alphanumeric()).count() >= 8 {
            chunks.push(Chunk { line_start: start, line_end: end.max(start), text: trimmed.to_string() });
        }
        cur.clear();
    };

    for line in text.lines() {
        line_no += 1;
        if chunks.len() >= MAX_CHUNKS_PER_FILE {
            break;
        }
        let boundary = line.trim().is_empty() || is_definition_start(line);
        if cur.len() >= TARGET_CHARS && boundary || cur.len() + line.len() > MAX_CHARS && !cur.is_empty() {
            flush(&mut chunks, &mut cur, start_line, line_no - 1);
            start_line = line_no;
            cur_lines = 0;
        }
        if line.len() > MAX_CHARS {
            // Minified or binary-ish line: split hard on char boundaries.
            let mut rest = line;
            while !rest.is_empty() && chunks.len() < MAX_CHUNKS_PER_FILE {
                let mut cut = rest.len().min(MAX_CHARS);
                while !rest.is_char_boundary(cut) {
                    cut -= 1;
                }
                cur.push_str(&rest[..cut]);
                flush(&mut chunks, &mut cur, line_no, line_no);
                rest = &rest[cut..];
            }
            start_line = line_no + 1;
            cur_lines = 0;
            continue;
        }
        if cur_lines == 0 {
            start_line = line_no;
        }
        cur.push_str(line);
        cur.push('\n');
        cur_lines += 1;
    }
    if !cur.is_empty() && chunks.len() < MAX_CHUNKS_PER_FILE {
        flush(&mut chunks, &mut cur, start_line, line_no);
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_numbers_are_tracked() {
        let mut src = String::new();
        for i in 0..200 {
            src.push_str(&format!("fn function_number_{i}() {{\n    do_something_useful({i});\n}}\n\n"));
        }
        let chunks = chunk_text(&src);
        assert!(chunks.len() > 3);
        assert_eq!(chunks[0].line_start, 1);
        for w in chunks.windows(2) {
            assert!(w[1].line_start > w[0].line_end);
        }
        let total: usize = src.lines().count();
        assert!(chunks.last().unwrap().line_end as usize <= total);
        let c = chunks.iter().find(|c| c.text.contains("function_number_100")).unwrap();
        let line = src.lines().position(|l| l.contains("function_number_100()")).unwrap() as u32 + 1;
        assert!(c.line_start <= line && line <= c.line_end);
    }

    #[test]
    fn long_lines_and_unicode() {
        let s = "é".repeat(5000);
        let chunks = chunk_text(&s);
        assert!(chunks.iter().all(|c| c.text.len() <= MAX_CHARS));
        assert!(!chunks.is_empty());
    }

    #[test]
    fn tiny_or_empty() {
        assert!(chunk_text("").is_empty());
        assert!(chunk_text("\n\n\n").is_empty());
        assert_eq!(chunk_text("hello world, this is a note").len(), 1);
    }
}
