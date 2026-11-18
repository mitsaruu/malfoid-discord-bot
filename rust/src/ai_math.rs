//! Формулы для браузера в MathML и запасной текст для Markdown Discord.

use math_core::{LatexToMathML, MathCoreConfig, MathDisplay};
use poise::serenity_prelude as serenity;
use pulldown_cmark::{html, Event, Options, Parser, Tag, TagEnd};

pub const FILE_MESSAGE: &str =
    "📐 Открой `answer.html` в браузере, чтобы прочитать ответ с формулами.";
const MAX_FORMULA_BYTES: usize = 4096;
const MAX_ANSWER_BYTES: usize = 192 * 1024;
const MAX_ATTACHMENT_BYTES: usize = 2 * 1024 * 1024;
static RENDER_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

const HTML_START: &str = r#"<!doctype html>
<html lang="ru"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'">
<title>Ответ с формулами</title>
<style>
:root { color-scheme: light dark; font: 17px/1.65 system-ui, sans-serif; }
main { max-width: 74ch; margin: 2rem auto; padding: 0 1rem; overflow-wrap: anywhere; }
h1, h2, h3 { line-height: 1.2; }
math { font-size: 1.08em; }
math[display="block"], pre, table { margin: 1rem 0; }
math[display="block"], pre { overflow-x: auto; }
table { border-collapse: collapse; }
th, td { border: 1px solid GrayText; padding: .35rem .6rem; text-align: start; }
blockquote { border-inline-start: .2em solid GrayText; margin-inline: 0; padding-inline: 1em; }
</style></head><body><main>
"#;

fn parser(text: &str) -> Parser<'_> {
    Parser::new_ext(text, Options::ENABLE_MATH | Options::ENABLE_TABLES)
}

pub fn contains_math(text: &str) -> bool {
    text.contains('$')
        && parser(text).any(|event| matches!(event, Event::InlineMath(_) | Event::DisplayMath(_)))
}

pub struct MathAnswer {
    pub html: String,
    pub discord: String,
}

impl MathAnswer {
    pub fn attachment(&self) -> serenity::CreateAttachment {
        serenity::CreateAttachment::bytes(self.html.as_bytes().to_vec(), "answer.html")
    }
}

/// Discord может запретить вложения или отклонить файл по размеру.
pub fn attachment_denied(error: &serenity::Error) -> bool {
    matches!(error, serenity::Error::Http(serenity::HttpError::UnsuccessfulRequest(response))
        if matches!(response.error.code, 40005 | 50013) || response.status_code.as_u16() == 413)
}

pub async fn render_for_discord(text: &str) -> Option<MathAnswer> {
    if text.len() > MAX_ANSWER_BYTES || !text.contains('$') {
        return None;
    }
    let slot = RENDER_SLOTS.try_acquire().ok()?;
    let text = text.to_string();
    tokio::task::spawn_blocking(move || {
        let _slot = slot;
        render(&text).filter(|answer| answer.html.len() <= MAX_ATTACHMENT_BYTES)
    }).await.ok().flatten()
}

pub fn render(text: &str) -> Option<MathAnswer> {
    if !contains_math(text) {
        return None;
    }

    let converter = LatexToMathML::new(MathCoreConfig::default(), vec![]).ok()?;
    let mut output = HTML_START.to_string();
    let mut discord = String::with_capacity(text.len());
    let mut offset = 0;

    html::push_html(
        &mut output,
        parser(text)
            .into_offset_iter()
            .filter_map(|(event, range)| {
                let (latex, display) = match event {
                    Event::InlineMath(latex) => (latex, MathDisplay::Inline),
                    Event::DisplayMath(latex) => (latex, MathDisplay::Block),
                    // Ответ модели не может включать активный HTML, ссылки или внешние изображения.
                    Event::Html(raw) | Event::InlineHtml(raw) => return Some(Event::Text(raw)),
                    Event::Start(Tag::Link { .. } | Tag::Image { .. })
                    | Event::End(TagEnd::Link | TagEnd::Image) => return None,
                    event => return Some(event),
                };
                discord.push_str(&text[offset..range.start]);
                offset = range.end;
                let code = latex.replace('`', "｀");
                if matches!(display, MathDisplay::Block) || code.contains('\n') || code.len() > 600
                {
                    discord.push_str(&format!("\n```\n{}\n```\n", code.trim()));
                } else {
                    discord.push_str(&format!("`{code}`"));
                }

                let rendered = (latex.len() <= MAX_FORMULA_BYTES)
                    .then(|| converter.convert_with_local_state(&latex, display).ok())
                    .flatten();
                Some(match rendered {
                    Some(math) => Event::Html(math.mathml.into()),
                    None => Event::Code(latex), // неизвестная команда LaTeX: сохраняем формулу, не теряем весь ответ
                })
            }),
    );
    discord.push_str(&text[offset..]);
    output.push_str("</main></body></html>");
    Some(MathAnswer {
        html: output,
        discord,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_math_from_the_report_and_keeps_the_table() {
        let answer = render(
            r#"Пусть $a_n$ — член ряда.

$$S_n = \sum_{k=1}^{n} a_k$$

$$\lim_{n \to \infty} a_n = \frac{2}{3} \ne 0$$

| Условие | Вывод |
| --- | --- |
| $a_n \ne 0$ | Расходится |"#,
        )
        .unwrap();
        for mark in ["<msub>", "<mfrac>", "∑", "∞", "<table>", "Расходится"] {
            assert!(answer.html.contains(mark), "{mark}: {}", answer.html);
        }
        assert!(answer.discord.contains("`a_n`"));
        assert!(answer
            .discord
            .contains("```\nS_n = \\sum_{k=1}^{n} a_k\n```"));
        assert!(!answer.discord.contains('$'));
    }

    #[test]
    fn leaves_code_alone_and_disables_markup_from_the_model() {
        assert!(render(r"Цена 10 \$; код `$x$`").is_none());
        assert!(render("```sh\necho '$x$'\n```").is_none());
        let answer = render("$x$ <script>alert(1)</script> ![alt](https://example.com/a.png) [ссылка](javascript:alert%281%29)").unwrap();
        assert!(answer.html.contains("&lt;script&gt;"));
        for active in ["<script", "<img", "href=", "https://example.com"] {
            assert!(!answer.html.contains(active), "{active}: {}", answer.html);
        }
    }

    #[test]
    fn unsupported_formula_remains_visible_as_escaped_latex() {
        let answer = render(r"$x$ и $\noSuchMathCommand{<x>}$").unwrap();
        assert!(answer.html.contains("<math"));
        assert!(answer.html.contains(r"\noSuchMathCommand{&lt;x&gt;}"));
    }
}
