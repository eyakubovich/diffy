//! Parse a Patch

use super::{Hunk, HunkRange, Line, ESCAPED_CHARS_BYTES, NO_NEWLINE_AT_EOF};
use crate::{
    patch::Patch,
    utils::{LineIter, Text},
};
use std::{borrow::Cow, fmt};

type Result<T, E = ParsePatchError> = std::result::Result<T, E>;

/// An error returned when parsing a `Patch` using [`Patch::from_str`] fails
///
/// [`Patch::from_str`]: struct.Patch.html#method.from_str
// TODO use a custom error type instead of a Cow
#[derive(Debug)]
pub struct ParsePatchError(Cow<'static, str>);

impl ParsePatchError {
    fn new<E: Into<Cow<'static, str>>>(e: E) -> Self {
        Self(e.into())
    }
}

impl fmt::Display for ParsePatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "error parsing patch: {}", self.0)
    }
}

impl std::error::Error for ParsePatchError {}

struct Parser<'a, T: Text + ?Sized> {
    lines: std::iter::Peekable<LineIter<'a, T>>,
}

impl<'a, T: Text + ?Sized> Parser<'a, T> {
    fn new(input: &'a T) -> Self {
        Self {
            lines: LineIter::new(input).peekable(),
        }
    }

    fn peek(&mut self) -> Option<&&'a T> {
        self.lines.peek()
    }

    fn next(&mut self) -> Result<&'a T> {
        let line = self
            .lines
            .next()
            .ok_or_else(|| ParsePatchError::new("unexpected EOF"))?;
        Ok(line)
    }
}

pub fn parse<'a>(input: &'a str) -> Result<Patch<'a, str>> {
    let mut parser = Parser::new(input);
    let header = patch_header(&mut parser)?;
    let hunks = hunks(&mut parser)?;

    Ok(Patch::new(
        convert_cow_to_str(header.0),
        convert_cow_to_str(header.1),
        hunks,
    ))
}

pub fn parse_bytes<'a>(input: &'a [u8]) -> Result<Patch<'a, [u8]>> {
    let mut parser = Parser::new(input);
    let header = patch_header(&mut parser)?;
    let hunks = hunks(&mut parser)?;

    Ok(Patch::new(header.0, header.1, hunks))
}

// This is only used when the type originated as a utf8 string
fn convert_cow_to_str(cow: Cow<'_, [u8]>) -> Cow<'_, str> {
    match cow {
        Cow::Borrowed(b) => std::str::from_utf8(b).unwrap().into(),
        Cow::Owned(o) => String::from_utf8(o).unwrap().into(),
    }
}

#[allow(clippy::type_complexity)]
fn patch_header<'a, T: Text + ToOwned + ?Sized>(
    parser: &mut Parser<'a, T>,
) -> Result<(Cow<'a, [u8]>, Cow<'a, [u8]>)> {
    skip_header_preamble(parser)?;
    let filename1 = parse_filename("--- ", parser.next()?)?;
    let filename2 = parse_filename("+++ ", parser.next()?)?;
    Ok((filename1, filename2))
}

// Skip to the first "--- " line, skipping any preamble lines like "diff --git", etc.
fn skip_header_preamble<'a, T: Text + ?Sized>(parser: &mut Parser<'a, T>) -> Result<()> {
    while let Some(line) = parser.peek() {
        if line.starts_with("--- ") {
            break;
        }
        parser.next()?;
    }

    Ok(())
}

fn parse_filename<'a, T: Text + ToOwned + ?Sized>(
    prefix: &str,
    line: &'a T,
) -> Result<Cow<'a, [u8]>> {
    let line = line
        .strip_prefix(prefix)
        .ok_or_else(|| ParsePatchError::new("unable to parse filename"))?;

    let filename = if let Some((filename, _)) = line.split_at_exclusive("\t") {
        filename
    } else if let Some((filename, _)) = line.split_at_exclusive("\n") {
        filename
    } else {
        return Err(ParsePatchError::new("filename unterminated"));
    };

    let filename = if let Some(quoted) = is_quoted(filename) {
        escaped_filename(quoted)?
    } else {
        unescaped_filename(filename)?
    };

    Ok(filename)
}

fn is_quoted<T: Text + ?Sized>(s: &T) -> Option<&T> {
    s.strip_prefix("\"").and_then(|s| s.strip_suffix("\""))
}

fn unescaped_filename<'a, T: Text + ToOwned + ?Sized>(filename: &'a T) -> Result<Cow<'a, [u8]>> {
    let bytes = filename.as_bytes();

    if bytes.iter().any(|b| ESCAPED_CHARS_BYTES.contains(b)) {
        return Err(ParsePatchError::new("invalid char in unquoted filename"));
    }

    Ok(bytes.into())
}

fn escaped_filename<T: Text + ToOwned + ?Sized>(escaped: &T) -> Result<Cow<'_, [u8]>> {
    let mut filename = Vec::new();

    let mut chars = escaped.as_bytes().iter().copied();
    while let Some(c) = chars.next() {
        if c == b'\\' {
            match chars
                .next()
                .ok_or_else(|| ParsePatchError::new("expected escaped character"))?
            {
                b'n' | b't' | b'0' | b'r' | b'\"' | b'\\' => filename.push(c),
                _ => return Err(ParsePatchError::new("invalid escaped character")),
            }
        } else if ESCAPED_CHARS_BYTES.contains(&c) {
            return Err(ParsePatchError::new("invalid unescaped character"));
        } else {
            filename.push(c);
        }
    }

    Ok(filename.into())
}

fn verify_hunks_in_order<T: ?Sized>(hunks: &[Hunk<'_, T>]) -> bool {
    for hunk in hunks.windows(2) {
        if hunk[0].old_range.end() >= hunk[1].old_range.start()
            || hunk[0].new_range.end() >= hunk[1].new_range.start()
        {
            return false;
        }
    }
    true
}

fn hunks<'a, T: Text + ?Sized>(parser: &mut Parser<'a, T>) -> Result<Vec<Hunk<'a, T>>> {
    let mut hunks = Vec::new();
    while parser.peek().is_some() {
        hunks.push(hunk(parser)?);
    }

    // check and verify that the Hunks are in sorted order and don't overlap
    if !verify_hunks_in_order(&hunks) {
        return Err(ParsePatchError::new("Hunks not in order or overlap"));
    }

    Ok(hunks)
}

fn hunk<'a, T: Text + ?Sized>(parser: &mut Parser<'a, T>) -> Result<Hunk<'a, T>> {
    let (range1, range2, function_context) = hunk_header(parser.next()?)?;
    let lines = hunk_lines(parser)?;

    // check counts of lines to see if they match the ranges in the hunk header
    let (len1, len2) = super::hunk_lines_count(&lines);
    if len1 != range1.len || len2 != range2.len {
        return Err(ParsePatchError::new("Hunk header does not match hunk"));
    }

    Ok(Hunk::new(range1, range2, function_context, lines))
}

fn hunk_header<T: Text + ?Sized>(input: &T) -> Result<(HunkRange, HunkRange, Option<&T>)> {
    let input = input
        .strip_prefix("@@ ")
        .ok_or_else(|| ParsePatchError::new("unable to parse hunk header"))?;

    let (ranges, function_context) = input
        .split_at_exclusive(" @@")
        .ok_or_else(|| ParsePatchError::new("hunk header unterminated"))?;
    let function_context = function_context.strip_prefix(" ");

    let (range1, range2) = ranges
        .split_at_exclusive(" ")
        .ok_or_else(|| ParsePatchError::new("unable to parse hunk header"))?;
    let range1 = range(
        range1
            .strip_prefix("-")
            .ok_or_else(|| ParsePatchError::new("unable to parse hunk header"))?,
    )?;
    let range2 = range(
        range2
            .strip_prefix("+")
            .ok_or_else(|| ParsePatchError::new("unable to parse hunk header"))?,
    )?;
    Ok((range1, range2, function_context))
}

fn range<T: Text + ?Sized>(s: &T) -> Result<HunkRange> {
    let (start, len) = if let Some((start, len)) = s.split_at_exclusive(",") {
        (
            start
                .parse()
                .ok_or_else(|| ParsePatchError::new("can't parse range"))?,
            len.parse()
                .ok_or_else(|| ParsePatchError::new("can't parse range"))?,
        )
    } else {
        (
            s.parse()
                .ok_or_else(|| ParsePatchError::new("can't parse range"))?,
            1,
        )
    };

    Ok(HunkRange::new(start, len))
}

fn hunk_lines<'a, T: Text + ?Sized>(parser: &mut Parser<'a, T>) -> Result<Vec<Line<'a, T>>> {
    let mut lines: Vec<Line<'a, T>> = Vec::new();
    let mut no_newline_context = false;
    let mut no_newline_delete = false;
    let mut no_newline_insert = false;

    while let Some(line) = parser.peek() {
        let line = if line.starts_with("@") {
            break;
        } else if no_newline_context {
            return Err(ParsePatchError::new("expected end of hunk"));
        } else if let Some(line) = line.strip_prefix(" ") {
            Line::Context(line)
        } else if line.starts_with("\n") {
            Line::Context(*line)
        } else if let Some(line) = line.strip_prefix("-") {
            if no_newline_delete {
                return Err(ParsePatchError::new("expected no more deleted lines"));
            }
            Line::Delete(line)
        } else if let Some(line) = line.strip_prefix("+") {
            if no_newline_insert {
                return Err(ParsePatchError::new("expected no more inserted lines"));
            }
            Line::Insert(line)
        } else if line.starts_with(NO_NEWLINE_AT_EOF) {
            let last_line = lines.pop().ok_or_else(|| {
                ParsePatchError::new("unexpected 'No newline at end of file' line")
            })?;
            match last_line {
                Line::Context(line) => {
                    no_newline_context = true;
                    Line::Context(strip_newline(line)?)
                }
                Line::Delete(line) => {
                    no_newline_delete = true;
                    Line::Delete(strip_newline(line)?)
                }
                Line::Insert(line) => {
                    no_newline_insert = true;
                    Line::Insert(strip_newline(line)?)
                }
            }
        } else {
            return Err(ParsePatchError::new("unexpected line in hunk body"));
        };

        lines.push(line);
        parser.next()?;
    }

    Ok(lines)
}

fn strip_newline<T: Text + ?Sized>(s: &T) -> Result<&T> {
    if let Some(stripped) = s.strip_suffix("\n") {
        Ok(stripped)
    } else {
        Err(ParsePatchError::new("missing newline"))
    }
}
