//! Column-layout engine for tabular `show interfaces` output.
//!
//! Every table in the family is a sequence of fixed-width columns; the
//! last cell of a row runs to end-of-line. Lines never carry trailing
//! whitespace (EOS pads between columns, not after the last one).

/// Cell alignment within a column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
}

/// One column: total field width (content + padding) and alignment.
#[derive(Debug, Clone, Copy)]
pub struct Col {
    pub width: usize,
    pub align: Align,
}

impl Col {
    pub const fn left(width: usize) -> Self {
        Self {
            width,
            align: Align::Left,
        }
    }

    pub const fn right(width: usize) -> Self {
        Self {
            width,
            align: Align::Right,
        }
    }
}

/// Pad `text` into a field of `width`. Content wider than the field is
/// emitted unclipped (truncation policy belongs to the caller).
pub fn pad(text: &str, col: Col) -> String {
    let len = text.chars().count();
    if len >= col.width {
        return text.to_string();
    }
    let fill = " ".repeat(col.width - len);
    match col.align {
        Align::Left => format!("{text}{fill}"),
        Align::Right => format!("{fill}{text}"),
    }
}

/// Render one row. Cells beyond the column list are appended verbatim
/// (the customary rest-of-line final column); the result is
/// right-trimmed, so left-aligned last columns cost nothing while
/// right-aligned ones keep their leading fill.
pub fn row(cols: &[Col], cells: &[&str]) -> String {
    let mut out = String::new();
    for (i, cell) in cells.iter().enumerate() {
        match cols.get(i) {
            Some(col) => out.push_str(&pad(cell, *col)),
            None => out.push_str(cell),
        }
    }
    out.trim_end().to_string()
}

/// Accumulates output lines, right-trimming each one.
#[derive(Debug, Default)]
pub struct Text {
    buf: String,
}

impl Text {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn line(&mut self, line: impl AsRef<str>) {
        self.buf.push_str(line.as_ref().trim_end());
        self.buf.push('\n');
    }

    pub fn blank(&mut self) {
        self.buf.push('\n');
    }

    pub fn row(&mut self, cols: &[Col], cells: &[&str]) {
        let rendered = row(cols, cells);
        self.buf.push_str(&rendered);
        self.buf.push('\n');
    }

    pub fn finish(self) -> String {
        self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_pad_and_trim() {
        let cols = [Col::left(11), Col::left(30), Col::left(13)];
        assert_eq!(
            row(&cols, &["Et1", "host rack3-u12 nic1", "connected"]),
            "Et1        host rack3-u12 nic1           connected"
        );
        // Empty middle cell keeps downstream columns aligned.
        assert_eq!(
            row(&cols, &["Et3", "", "disabled"]),
            "Et3                                      disabled"
        );
        // Trailing empties vanish entirely.
        assert_eq!(row(&cols, &["Et3", "", ""]), "Et3");
    }

    #[test]
    fn right_alignment() {
        let cols = [Col::left(4), Col::right(25), Col::right(16)];
        assert_eq!(
            row(&cols, &["Et1", "4816030792344", "4294811034"]),
            "Et1             4816030792344      4294811034"
        );
    }

    #[test]
    fn overwide_cells_are_not_clipped() {
        let cols = [Col::left(3), Col::left(4)];
        assert_eq!(row(&cols, &["Ethernet49", "x"]), "Ethernet49x");
    }

    #[test]
    fn text_builder_trims_lines() {
        let mut t = Text::new();
        t.line("hello   ");
        t.blank();
        t.line("world");
        assert_eq!(t.finish(), "hello\n\nworld\n");
    }
}
