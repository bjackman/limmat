// This module provides a way to render styled text, either as HTML or using terminal
// escape codes. It originally resembled ratatui's text rendering system
// (https://docs.rs/ratatui/latest/ratatui/text/struct.Text.html) so
// that if ratatui gains support for hyperlinks
// (https://github.com/ratatui/ratatui/issues/1028) maybe I can easily port this
// code to use that. But, it dieberged.
//
// I discovered that I absolutely hate terminal hacking, I don't really know
// if that's a reason not to contribute to ratatui. Anyway, the real reason I'm
// not contributing to ratatui is that I wanna race to finish this project
// instead of getting bogged down in side-quests!

use std::{
    borrow::Cow,
    fmt::{self, Display, Formatter},
};

use colored::{ColoredString, Colorize as _};
use indoc::indoc;
use unicode_segmentation::UnicodeSegmentation as _;

// Represents a block of text, potentially with styling. Note this always
// represents a block, you can't represent a string without a newline at the
// end (I'm not sure if that's the same in Ratatui).
pub struct Text<'a> {
    pub lines: Vec<Line<'a>>,
}

impl<'a, T> FromIterator<T> for Text<'a>
where
    T: Into<Line<'a>>,
{
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        Self {
            lines: iter.into_iter().map(|i| i.into()).collect(),
        }
    }
}

// Shorthand for constructing a Text with a single Line.
impl<'a, T: Into<Line<'a>>> From<T> for Text<'a> {
    fn from(t: T) -> Self {
        Self {
            lines: vec![t.into()],
        }
    }
}
impl<'a> Text<'a> {
    // Render the text with style applied using ANSI commands. Use Display on the returned value
    // to write it out.
    pub fn ansi(&self) -> RenderAnsi<'_> {
        RenderAnsi { text: self }
    }

    // Render to an HTML <pre> element.
    pub fn html_pre(&self) -> RenderHtmlPre<'_> {
        RenderHtmlPre { text: self }
    }

    pub fn into_lines(self) -> impl Iterator<Item = Line<'a>> {
        self.lines.into_iter()
    }
}

pub struct RenderAnsi<'a> {
    text: &'a Text<'a>,
}

impl Display for RenderAnsi<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        for line in self.text.lines.iter() {
            writeln!(f, "{}", RenderAnsiLine { line })?;
        }
        Ok(())
    }
}

pub struct RenderHtmlPre<'a> {
    text: &'a Text<'a>,
}

impl RenderHtmlPre<'_> {
    pub const CSS: &'static str = indoc! { "
        .error {
            background: rgba(255, 0, 0, 0.3);
        }

        .failure {
            background: rgba(242, 121, 53, 0.3);
        }

        .success {
            background: rgba(0, 255, 0, 0.3);
        }

        .test-name {
            font-weight: bold;
        }

        :root {
            color-scheme: light dark;
        }
    "};
}

impl Display for RenderHtmlPre<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        writeln!(f, "<pre>")?;
        for line in self.text.lines.iter() {
            writeln!(f, "{}", RenderHtmlLine { line })?;
        }
        writeln!(f, "</pre>")
    }
}

pub struct Line<'a> {
    pub spans: Vec<Span<'a>>,
}

impl<'a, T> FromIterator<T> for Line<'a>
where
    T: Into<Span<'a>>,
{
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        Self {
            spans: Vec::from_iter(iter.into_iter().map(|i| i.into())),
        }
    }
}

impl Line<'_> {
    // Truncate to the given number of unicode graphemej clusters, disregarding styling.
    pub fn truncate_graphemes(mut self, length: usize) -> Self {
        let mut remaining_length = length;
        let mut last_span_idx = None;
        for (i, span) in self.spans.iter_mut().enumerate() {
            let n = span.num_graphemes();
            if n >= remaining_length {
                span.truncate_graphemes(remaining_length);
                last_span_idx = Some(i);
                break;
            }
            remaining_length -= n;
        }
        if let Some(i) = last_span_idx {
            self.spans.truncate(i + 1);
        }
        self
    }
}

struct RenderAnsiLine<'a> {
    line: &'a Line<'a>,
}

impl Display for RenderAnsiLine<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        for span in self.line.spans.iter() {
            write!(f, "{}", RenderAnsiSpan { span })?;
        }
        Ok(())
    }
}

// TODO: Deduplicate with generics?
struct RenderHtmlLine<'a> {
    line: &'a Line<'a>,
}

impl Display for RenderHtmlLine<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        for span in self.line.spans.iter() {
            write!(f, "{}", RenderHtmlSpan { span })?;
        }
        Ok(())
    }
}

// Shorthand for constructing a Line with a single Span.
impl<'a, T: Into<Span<'a>>> From<T> for Line<'a> {
    fn from(t: T) -> Self {
        Self {
            spans: vec![t.into()],
        }
    }
}

pub struct Span<'a> {
    pub class: Option<Class>,
    // The cow is copied from Ratatui. My understanding is that this is there to
    // be generic across ownership or reference.
    pub content: Cow<'a, str>,
    pub url: Option<Cow<'a, str>>,
}

impl<'a, T: Into<Cow<'a, str>>> From<T> for Span<'a> {
    fn from(t: T) -> Span<'a> {
        Span::new(t)
    }
}

impl<'a> Span<'a> {
    pub fn new(content: impl Into<Cow<'a, str>>) -> Self {
        Self {
            content: content.into(),
            class: None,
            url: None,
        }
    }

    pub fn with_class(mut self, class: Class) -> Self {
        self.class = Some(class);
        self
    }

    pub fn with_url(mut self, url: impl Into<Cow<'a, str>>) -> Self {
        self.url = Some(url.into());
        self
    }

    fn num_graphemes(&self) -> usize {
        self.content.graphemes(true).count()
    }

    // If this was to become public, the API should be made consistent with
    // Lines::truncate_graphemes. But at the moment it just has whatever is most convenient.
    fn truncate_graphemes(&mut self, len: usize) {
        if let Some((byte_idx, _)) = self.content.grapheme_indices(true).nth(len) {
            match self.content {
                Cow::Borrowed(s) => {
                    self.content = Cow::Borrowed(&s[..byte_idx]);
                }
                Cow::Owned(ref mut s) => {
                    s.truncate(byte_idx);
                }
            };
        }
    }
}

struct RenderAnsiSpan<'a> {
    span: &'a Span<'a>,
}

impl Display for RenderAnsiSpan<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let output = self.span.content.as_ref();
        let output = match self.span.class {
            // TODO: ColoredString is not very useful here any more.
            None => ColoredString::from(output),
            Some(Class::Failure) => output.on_red(),
            Some(Class::Success) => output.on_green(),
            Some(Class::Error) => output.on_bright_red(),
            Some(Class::TestName) => output.bold(),
        };
        // Renders a hyperlink like in
        // https://gist.github.com/egmontkob/eb114294efbcd5adb1944c9f3cb5feda.
        if let Some(ref url) = &self.span.url {
            write!(f, "\u{1b}]8;;{}\u{1b}\\{}\u{1b}]8;;\u{1b}\\", url, output)
        } else {
            write!(f, "{}", &output.to_string())
        }
    }
}

struct RenderHtmlSpan<'a> {
    span: &'a Span<'a>,
}

impl Display for RenderHtmlSpan<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        if let Some(ref url) = &self.span.url {
            write!(f, r#"<a href="{}">"#, url)?;
        }
        write!(
            f,
            r#"<span class="{}">{}</span>"#,
            match self.span.class {
                None => "",
                Some(Class::Error) => "error",
                Some(Class::Success) => "success",
                Some(Class::Failure) => "failure",
                Some(Class::TestName) => "test-name",
            },
            self.span.content.as_ref()
        )?;
        if self.span.url.is_some() {
            write!(f, "</a>")?;
        }
        Ok(())
    }
}

// This is like a CSS class. For ANSI output this will produce a hard-coded
// style. For HTML it outputs a CSS class name, some CSS is provided  to
// make use of these classes.
pub enum Class {
    Error,
    Success,
    Failure,
    TestName,
}
