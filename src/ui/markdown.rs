use pulldown_cmark::{CodeBlockKind, Event, Parser, Tag, TagEnd};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

/// High-level function to parse Markdown content and convert it into Ratatui Lines.
pub fn render_markdown(input: &str) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let parser = Parser::new(input);

    let mut in_code_block = false;
    let mut code_lang = String::new();
    let mut code_buffer = String::new();

    let mut in_heading = false;
    let mut heading_level = 1;
    let mut current_spans: Vec<Span<'static>> = Vec::new();

    let mut is_bold = false;
    let mut is_italic = false;
    let mut in_blockquote = false;

    for event in parser {
        match event {
            Event::Start(Tag::CodeBlock(kind)) => {
                // Flush any open inline line
                if !current_spans.is_empty() {
                    lines.push(Line::from(std::mem::take(&mut current_spans)));
                }
                in_code_block = true;
                code_lang = match kind {
                    CodeBlockKind::Fenced(lang) => lang.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                code_buffer.clear();
            }

            Event::End(TagEnd::CodeBlock) => {
                in_code_block = false;
                lines.extend(format_code_block(&code_lang, &code_buffer));
                code_buffer.clear();
                code_lang.clear();
            }

            Event::Start(Tag::Heading { level, .. }) => {
                if !current_spans.is_empty() {
                    lines.push(Line::from(std::mem::take(&mut current_spans)));
                }
                in_heading = true;
                heading_level = level as usize;
                let prefix = match heading_level {
                    1 => "◆ ",
                    2 => "◇ ",
                    _ => "▪ ",
                };
                current_spans.push(Span::styled(
                    prefix.to_string(),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ));
            }

            Event::End(TagEnd::Heading(_)) => {
                in_heading = false;
                if !current_spans.is_empty() {
                    lines.push(Line::from(std::mem::take(&mut current_spans)));
                }
                lines.push(Line::from(""));
            }

            Event::Start(Tag::BlockQuote(_)) => {
                in_blockquote = true;
            }

            Event::End(TagEnd::BlockQuote(_)) => {
                in_blockquote = false;
                if !current_spans.is_empty() {
                    lines.push(Line::from(std::mem::take(&mut current_spans)));
                }
            }

            Event::Start(Tag::Item) => {
                if !current_spans.is_empty() {
                    lines.push(Line::from(std::mem::take(&mut current_spans)));
                }
                current_spans.push(Span::styled(
                    "  • ".to_string(),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ));
            }

            Event::End(TagEnd::Item) => {
                if !current_spans.is_empty() {
                    lines.push(Line::from(std::mem::take(&mut current_spans)));
                }
            }

            Event::Start(Tag::Strong) => {
                is_bold = true;
            }

            Event::End(TagEnd::Strong) => {
                is_bold = false;
            }

            Event::Start(Tag::Emphasis) => {
                is_italic = true;
            }

            Event::End(TagEnd::Emphasis) => {
                is_italic = false;
            }

            Event::Code(code) => {
                if in_code_block {
                    code_buffer.push_str(&code);
                } else {
                    let mut style = Style::default()
                        .fg(Color::LightCyan)
                        .bg(Color::Rgb(35, 42, 54));
                    if is_bold {
                        style = style.add_modifier(Modifier::BOLD);
                    }
                    current_spans.push(Span::styled(format!(" `{}` ", code), style));
                }
            }

            Event::Text(text) => {
                if in_code_block {
                    code_buffer.push_str(&text);
                } else {
                    let mut style = Style::default().fg(Color::White);
                    if in_heading {
                        let color = match heading_level {
                            1 => Color::Yellow,
                            2 => Color::Cyan,
                            _ => Color::LightBlue,
                        };
                        style = style.fg(color).add_modifier(Modifier::BOLD);
                    } else if in_blockquote {
                        style = style.fg(Color::LightYellow).add_modifier(Modifier::ITALIC);
                    }

                    if is_bold {
                        style = style.add_modifier(Modifier::BOLD);
                    }
                    if is_italic {
                        style = style.add_modifier(Modifier::ITALIC);
                    }

                    // Handle multiline text splits
                    let raw_str = text.to_string();
                    let parts: Vec<&str> = raw_str.split('\n').collect();
                    for (i, part) in parts.iter().enumerate() {
                        if i > 0 {
                            if in_blockquote {
                                current_spans.insert(
                                    0,
                                    Span::styled(
                                        " │ ".to_string(),
                                        Style::default().fg(Color::DarkGray),
                                    ),
                                );
                            }
                            lines.push(Line::from(std::mem::take(&mut current_spans)));
                        }
                        if !part.is_empty() {
                            current_spans.push(Span::styled(part.to_string(), style));
                        }
                    }
                }
            }

            Event::SoftBreak | Event::HardBreak if !in_code_block => {
                if in_blockquote {
                    current_spans.insert(
                        0,
                        Span::styled(" │ ".to_string(), Style::default().fg(Color::DarkGray)),
                    );
                }
                lines.push(Line::from(std::mem::take(&mut current_spans)));
            }

            _ => {}
        }
    }

    // Flush any pending inline line
    if !current_spans.is_empty() {
        if in_blockquote {
            current_spans.insert(
                0,
                Span::styled(" │ ".to_string(), Style::default().fg(Color::DarkGray)),
            );
        }
        lines.push(Line::from(current_spans));
    }

    // If still in unclosed code block (e.g. streaming mid-block), render partial code block
    if in_code_block && !code_buffer.is_empty() {
        lines.extend(format_code_block(&code_lang, &code_buffer));
    }

    lines
}

/// Format a code block with language title bar and pure-Rust syntax highlighting.
pub fn format_code_block(lang: &str, code: &str) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let display_lang = if lang.is_empty() { "code" } else { lang };

    // Header bar
    let header_text = format!(
        "   ┌─ {} ──────────────────────────────────────────",
        display_lang
    );
    lines.push(Line::from(Span::styled(
        header_text,
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));

    // Body lines with syntax highlighting
    for line in code.lines() {
        let highlighted = highlight_code_line(display_lang, line);
        let mut spans = vec![Span::styled("   │ ", Style::default().fg(Color::DarkGray))];
        spans.extend(highlighted);
        lines.push(Line::from(spans));
    }

    // Footer bar
    lines.push(Line::from(Span::styled(
        "   └─────────────────────────────────────────────────────".to_string(),
        Style::default().fg(Color::DarkGray),
    )));
    lines.push(Line::from(""));

    lines
}

/// Lightweight pure-Rust keyword and literal highlighter.
fn highlight_code_line(lang: &str, line: &str) -> Vec<Span<'static>> {
    let lang_lower = lang.to_lowercase();
    let is_rust = lang_lower == "rust" || lang_lower == "rs";
    let is_python = lang_lower == "python" || lang_lower == "py";
    let is_shell =
        lang_lower == "bash" || lang_lower == "sh" || lang_lower == "shell" || lang_lower == "zsh";
    let is_c_like =
        lang_lower == "c" || lang_lower == "cpp" || lang_lower == "c++" || lang_lower == "java";

    let trimmed = line.trim_start();
    // Line comments
    if (is_rust || is_c_like) && trimmed.starts_with("//") {
        return vec![Span::styled(
            line.to_string(),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )];
    }
    if (is_python || is_shell) && trimmed.starts_with('#') {
        return vec![Span::styled(
            line.to_string(),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )];
    }

    let mut spans = Vec::new();
    let mut current_token = String::new();
    let mut in_string = false;
    let mut string_quote = ' ';

    for c in line.chars() {
        if in_string {
            current_token.push(c);
            if c == string_quote {
                spans.push(Span::styled(
                    std::mem::take(&mut current_token),
                    Style::default().fg(Color::LightGreen),
                ));
                in_string = false;
            }
            continue;
        }

        if c == '"' || c == '\'' {
            if !current_token.is_empty() {
                spans.push(classify_word(display_token(&current_token), &lang_lower));
                current_token.clear();
            }
            in_string = true;
            string_quote = c;
            current_token.push(c);
            continue;
        }

        if c.is_alphanumeric() || c == '_' {
            current_token.push(c);
        } else {
            if !current_token.is_empty() {
                spans.push(classify_word(display_token(&current_token), &lang_lower));
                current_token.clear();
            }
            // Syntax punctuation
            let punct_style =
                if c == '(' || c == ')' || c == '{' || c == '}' || c == '[' || c == ']' {
                    Style::default().fg(Color::Yellow)
                } else if c == ':' || c == ';' || c == ',' || c == '.' {
                    Style::default().fg(Color::DarkGray)
                } else if c == '+'
                    || c == '-'
                    || c == '*'
                    || c == '/'
                    || c == '='
                    || c == '<'
                    || c == '>'
                    || c == '&'
                    || c == '|'
                    || c == '!'
                {
                    Style::default().fg(Color::Cyan)
                } else {
                    Style::default().fg(Color::White)
                };
            spans.push(Span::styled(c.to_string(), punct_style));
        }
    }

    if !current_token.is_empty() {
        if in_string {
            spans.push(Span::styled(
                current_token,
                Style::default().fg(Color::LightGreen),
            ));
        } else {
            spans.push(classify_word(display_token(&current_token), &lang_lower));
        }
    }

    spans
}

fn display_token(s: &str) -> String {
    s.to_string()
}

fn classify_word(word: String, lang: &str) -> Span<'static> {
    // Number check
    if word.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return Span::styled(word, Style::default().fg(Color::Magenta));
    }

    // Rust keywords
    if is_rust_keyword(&word) && (lang == "rust" || lang == "rs" || lang == "code") {
        return Span::styled(
            word,
            Style::default()
                .fg(Color::LightMagenta)
                .add_modifier(Modifier::BOLD),
        );
    }

    // Python keywords
    if is_python_keyword(&word) && (lang == "python" || lang == "py" || lang == "code") {
        return Span::styled(
            word,
            Style::default()
                .fg(Color::LightMagenta)
                .add_modifier(Modifier::BOLD),
        );
    }

    // Shell keywords
    if is_shell_keyword(&word)
        && (lang == "bash" || lang == "sh" || lang == "shell" || lang == "zsh")
    {
        return Span::styled(
            word,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    }

    // Types
    if is_common_type(&word) {
        return Span::styled(word, Style::default().fg(Color::LightYellow));
    }

    // Default identifier
    Span::styled(word, Style::default().fg(Color::White))
}

fn is_rust_keyword(w: &str) -> bool {
    matches!(
        w,
        "as" | "break"
            | "const"
            | "continue"
            | "crate"
            | "else"
            | "enum"
            | "extern"
            | "false"
            | "fn"
            | "for"
            | "if"
            | "impl"
            | "in"
            | "let"
            | "loop"
            | "match"
            | "mod"
            | "move"
            | "mut"
            | "pub"
            | "ref"
            | "return"
            | "self"
            | "Self"
            | "static"
            | "struct"
            | "super"
            | "trait"
            | "true"
            | "type"
            | "unsafe"
            | "use"
            | "where"
            | "while"
            | "async"
            | "await"
            | "dyn"
            | "Some"
            | "None"
            | "Ok"
            | "Err"
    )
}

fn is_python_keyword(w: &str) -> bool {
    matches!(
        w,
        "False"
            | "None"
            | "True"
            | "and"
            | "as"
            | "assert"
            | "async"
            | "await"
            | "break"
            | "class"
            | "continue"
            | "def"
            | "del"
            | "elif"
            | "else"
            | "except"
            | "finally"
            | "for"
            | "from"
            | "global"
            | "if"
            | "import"
            | "in"
            | "is"
            | "lambda"
            | "nonlocal"
            | "not"
            | "or"
            | "pass"
            | "raise"
            | "return"
            | "try"
            | "while"
            | "with"
            | "yield"
            | "self"
    )
}

fn is_shell_keyword(w: &str) -> bool {
    matches!(
        w,
        "if" | "then"
            | "else"
            | "elif"
            | "fi"
            | "case"
            | "esac"
            | "for"
            | "select"
            | "while"
            | "until"
            | "do"
            | "done"
            | "in"
            | "function"
            | "time"
            | "echo"
            | "export"
            | "source"
            | "cat"
            | "grep"
            | "awk"
            | "sed"
            | "sudo"
            | "cd"
            | "exit"
            | "cargo"
            | "nexus"
    )
}

fn is_common_type(w: &str) -> bool {
    matches!(
        w,
        "u8" | "u16"
            | "u32"
            | "u64"
            | "u128"
            | "usize"
            | "i8"
            | "i16"
            | "i32"
            | "i64"
            | "i128"
            | "isize"
            | "f32"
            | "f64"
            | "bool"
            | "char"
            | "str"
            | "String"
            | "Vec"
            | "Option"
            | "Result"
            | "Box"
            | "Arc"
            | "Rc"
            | "int"
            | "float"
            | "dict"
            | "list"
            | "set"
    )
}
