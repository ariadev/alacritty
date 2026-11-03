//! Display-only Unicode bidirectional layout. Grid storage always remains logical.
//!
//! Resolve whole soft-wrapped paragraphs with `BidiLine::from_rows`; reordering
//! each physical row separately loses embedding and neutral-character context.
//! Each terminal cell (including its combining characters) is one reorderable
//! object, and a wide character and its spacer remain adjacent in screen order.

use std::ops::Range;

use alacritty_terminal::grid::{Dimensions, Grid};
use alacritty_terminal::index::Line;
use alacritty_terminal::term::Term;
use alacritty_terminal::term::cell::{Cell, Flags};
use unicode_bidi::{BidiInfo, Level, ParagraphInfo};

/// A directional run in visual order, with logical grid column coordinates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BidiRun {
    pub columns: Range<usize>,
    pub visual_start: usize,
    pub rtl: bool,
}

#[derive(Clone, Debug, Default)]
pub struct BidiLine {
    logical_to_visual: Vec<usize>,
    visual_to_logical: Vec<usize>,
    levels: Vec<Level>,
    runs: Vec<BidiRun>,
}

struct Atom {
    column: usize,
    width: usize,
    byte: usize,
}

impl BidiLine {
    /// Resolve a grid row with the context of its complete soft-wrapped paragraph.
    pub fn for_grid(grid: &Grid<Cell>, line: Line) -> Self {
        if line < grid.topmost_line() || line > grid.bottommost_line() {
            return Self::identity(grid.columns());
        }
        let (start, end) = Self::paragraph_bounds(grid, line);
        let rows =
            (start.0..=end.0).map(|i| grid[Line(i)][..].iter().collect()).collect::<Vec<_>>();
        Self::from_rows(&rows).into_iter().nth((line.0 - start.0) as usize).unwrap()
    }

    pub fn for_term<T>(term: &Term<T>, line: Line) -> Self {
        Self::for_grid(term.grid(), line)
    }

    /// Resolve each paragraph touching the viewport once, returning viewport rows.
    pub fn viewport<T>(term: &Term<T>) -> Vec<Self> {
        let grid = term.grid();
        let top = Line(-(grid.display_offset() as i32));
        let bottom = top + grid.screen_lines() - 1usize;
        let mut current = top;
        let mut lines = Vec::with_capacity(grid.screen_lines());
        while current <= bottom {
            let (start, end) = Self::paragraph_bounds(grid, current);
            let rows =
                (start.0..=end.0).map(|i| grid[Line(i)][..].iter().collect()).collect::<Vec<_>>();
            lines.extend(
                Self::from_rows(&rows)
                    .into_iter()
                    .skip((current.0 - start.0) as usize)
                    .take((bottom.0.min(end.0) - current.0 + 1) as usize),
            );
            current = end + 1i32;
        }
        lines
    }

    fn paragraph_bounds(grid: &Grid<Cell>, line: Line) -> (Line, Line) {
        let mut start = line;
        while start > grid.topmost_line()
            && grid[start - 1i32][grid.last_column()].flags.contains(Flags::WRAPLINE)
        {
            start -= 1;
        }
        let mut end = line;
        while end < grid.bottommost_line()
            && grid[end][grid.last_column()].flags.contains(Flags::WRAPLINE)
        {
            end += 1;
        }
        (start, end)
    }

    /// Lay out a single physical line using the terminal's LTR paragraph base.
    #[cfg(test)]
    pub fn new<'a>(cells: impl IntoIterator<Item = &'a Cell>) -> Self {
        Self::from_rows(&[cells.into_iter().collect()]).pop().unwrap()
    }

    /// Lay out the rows of one logical paragraph, without inserting line breaks.
    ///
    /// Rows must include blank cells and wide-character spacers. Callers split
    /// paragraphs at hard line breaks, and may include offscreen rows to retain
    /// the context of a paragraph crossing the viewport boundary.
    pub fn from_rows(rows: &[Vec<&Cell>]) -> Vec<Self> {
        let mut text = String::new();
        let mut line_data = Vec::with_capacity(rows.len());
        for row in rows {
            let start = text.len();
            let mut atoms: Vec<Atom> = Vec::with_capacity(row.len());
            for (column, cell) in row.iter().enumerate() {
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER)
                    && column > 0
                    && row[column - 1].flags.contains(Flags::WIDE_CHAR)
                {
                    atoms.last_mut().unwrap().width += 1;
                    continue;
                }
                atoms.push(Atom { column, width: 1, byte: text.len() });
                text.push(cell.c);
                if let Some(zerowidth) = cell.zerowidth() {
                    text.extend(zerowidth);
                }
            }
            line_data.push((start..text.len(), atoms, row.len()));
        }

        // Most terminal output needs no Unicode analysis or reordering.
        if text.is_ascii() {
            return rows.iter().map(|row| Self::identity(row.len())).collect();
        }

        let bidi = BidiInfo::new(&text, Some(Level::ltr()));
        if !bidi.has_rtl() {
            return rows.iter().map(|row| Self::identity(row.len())).collect();
        }
        line_data
            .into_iter()
            .map(|(bytes, atoms, columns)| {
                if bytes.is_empty() {
                    return Self::identity(columns);
                }

                // `reordered_levels` clones its entire input. Restrict that
                // input to this row, retaining the paragraph-resolved levels,
                // to avoid quadratic work for long soft-wrapped paragraphs.
                let local = BidiInfo {
                    text: &text[bytes.clone()],
                    original_classes: bidi.original_classes[bytes.clone()].to_vec(),
                    levels: bidi.levels[bytes.clone()].to_vec(),
                    paragraphs: Vec::new(),
                };
                let mut levels = local.levels.clone();
                for para in &bidi.paragraphs {
                    let start = bytes.start.max(para.range.start);
                    let end = bytes.end.min(para.range.end);
                    if start < end {
                        let range = start - bytes.start..end - bytes.start;
                        let paragraph = ParagraphInfo { range: range.clone(), level: para.level };
                        let adjusted = local.reordered_levels(&paragraph, range.clone());
                        levels[range.clone()].copy_from_slice(&adjusted[range]);
                    }
                }
                let atom_levels: Vec<_> =
                    atoms.iter().map(|atom| levels[atom.byte - bytes.start]).collect();
                let order = BidiInfo::reorder_visual(&atom_levels);
                let mut line = Self {
                    logical_to_visual: vec![0; columns],
                    visual_to_logical: vec![0; columns],
                    levels: vec![Level::ltr(); columns],
                    runs: Vec::new(),
                };
                let mut visual = 0;
                let mut previous_atom = None;
                for index in order {
                    let atom = &atoms[index];
                    let level = atom_levels[index];
                    let rtl = level.is_rtl();
                    for offset in 0..atom.width {
                        line.logical_to_visual[atom.column + offset] = visual + offset;
                        line.visual_to_logical[visual + offset] = atom.column + offset;
                        line.levels[atom.column + offset] = level;
                    }

                    // Equal levels alone are insufficient: discontinuous logical
                    // text must not be submitted to a shaper as one buffer.
                    let adjacent = previous_atom.is_some_and(|previous: usize| {
                        atom_levels[previous] == level
                            && if rtl { index + 1 == previous } else { previous + 1 == index }
                    });
                    if adjacent {
                        let run = line.runs.last_mut().unwrap();
                        run.columns.start = run.columns.start.min(atom.column);
                        run.columns.end = run.columns.end.max(atom.column + atom.width);
                    } else {
                        line.runs.push(BidiRun {
                            columns: atom.column..atom.column + atom.width,
                            visual_start: visual,
                            rtl,
                        });
                    }
                    visual += atom.width;
                    previous_atom = Some(index);
                }
                line
            })
            .collect()
    }

    pub fn identity(columns: usize) -> Self {
        Self {
            logical_to_visual: (0..columns).collect(),
            visual_to_logical: (0..columns).collect(),
            levels: vec![Level::ltr(); columns],
            runs: if columns == 0 {
                Vec::new()
            } else {
                vec![BidiRun { columns: 0..columns, visual_start: 0, rtl: false }]
            },
        }
    }

    pub fn visual_col(&self, logical: usize) -> usize {
        self.logical_to_visual.get(logical).copied().unwrap_or(logical)
    }

    pub fn logical_col(&self, visual: usize) -> usize {
        self.visual_to_logical.get(visual).copied().unwrap_or(visual)
    }

    pub fn is_rtl(&self, logical: usize) -> bool {
        self.levels.get(logical).is_some_and(Level::is_rtl)
    }

    /// Mirror only for the legacy character renderer; RTL shapers mirror glyphs.
    pub fn mirrored(&self, logical: usize, character: char) -> char {
        if self.is_rtl(logical) {
            unicode_bidi_mirroring::get_mirrored(character).unwrap_or(character)
        } else {
            character
        }
    }

    #[cfg(test)]
    pub fn visual_runs(&self) -> &[BidiRun] {
        &self.runs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cells(text: &str) -> Vec<Cell> {
        text.chars().map(|c| Cell { c, ..Cell::default() }).collect()
    }

    fn visual(text: &str) -> String {
        let cells = cells(text);
        let line = BidiLine::new(&cells);
        (0..cells.len())
            .map(|i| {
                let logical = line.logical_col(i);
                line.mirrored(logical, cells[logical].c)
            })
            .collect()
    }

    #[test]
    fn ascii_is_identity() {
        assert_eq!(visual("hello [123]"), "hello [123]");
        let line = BidiLine::new(&cells(""));
        assert_eq!(line.visual_col(7), 7);
        assert_eq!(line.logical_col(7), 7);
    }

    #[test]
    fn mixed_persian_numbers_and_mirroring() {
        assert_eq!(visual("abc سلام 123"), "abc 123 مالس");
        assert_eq!(visual("abc (سلام) xyz"), "abc (مالس) xyz");
        assert_eq!(visual("سلام ۱۲۳"), "۱۲۳ مالس");
    }

    #[test]
    fn maps_are_inverse_including_blank_cells() {
        let cells = cells("$ echo سلام 123   ");
        let line = BidiLine::new(&cells);
        for column in 0..cells.len() {
            assert_eq!(line.logical_col(line.visual_col(column)), column);
        }
        assert_eq!(line.visual_col(cells.len() - 1), cells.len() - 1);
    }

    #[test]
    fn combining_marks_and_zwnj_stay_with_cell() {
        let mut cells = cells("بب");
        cells[0].push_zerowidth('\u{64e}');
        cells[0].push_zerowidth('\u{200c}');
        let line = BidiLine::new(&cells);
        assert_eq!(line.visual_col(0), 1);
        assert_eq!(line.visual_col(1), 0);
    }

    #[test]
    fn wide_cell_and_spacer_remain_in_screen_order() {
        let mut cells = cells("אב界 ג");
        cells[2].flags.insert(Flags::WIDE_CHAR);
        cells[3].flags.insert(Flags::WIDE_CHAR_SPACER);
        let line = BidiLine::new(&cells);
        assert_eq!(line.visual_col(3), line.visual_col(2) + 1);
        for column in 0..cells.len() {
            assert_eq!(line.logical_col(line.visual_col(column)), column);
        }
    }

    #[test]
    fn soft_wrap_preserves_embedding_context() {
        let first = cells("x\u{202b}אב");
        let second = cells("(cd)\u{202c}x");
        let rows = vec![first.iter().collect(), second.iter().collect()];
        let lines = BidiLine::from_rows(&rows);
        assert!(lines[1].is_rtl(0));
        assert!(!BidiLine::new(&second).is_rtl(0));
    }

    #[test]
    fn grid_lookup_uses_wrapped_context_and_stops_at_hard_breaks() {
        use alacritty_terminal::index::Column;

        let mut grid = Grid::<Cell>::new(3, 4, 0);
        for (column, cell) in cells("xאב ").into_iter().enumerate() {
            grid[Line(0)][Column(column)] = cell;
        }
        grid[Line(0)][Column(0)].push_zerowidth('\u{202b}');
        grid[Line(0)][Column(3)].flags.insert(Flags::WRAPLINE);
        for (column, cell) in cells("(cd)").into_iter().enumerate() {
            grid[Line(1)][Column(column)] = cell;
        }
        assert!(BidiLine::for_grid(&grid, Line(1)).is_rtl(0));
        assert!(!BidiLine::for_grid(&grid, Line(2)).is_rtl(0));
        grid[Line(0)][Column(3)].flags.remove(Flags::WRAPLINE);
        assert!(!BidiLine::for_grid(&grid, Line(1)).is_rtl(0));
        assert_eq!(BidiLine::for_grid(&grid, Line(-1)).logical_col(2), 2);
    }

    /// Run with `cargo test -p alacritty bidi_timing -- --ignored --nocapture`.
    /// Timings are diagnostic, without machine-dependent pass/fail thresholds.
    #[test]
    #[ignore]
    fn bidi_timing() {
        use std::hint::black_box;
        use std::time::Instant;

        for (name, text) in [
            ("ascii", "user@host:~/project $ cargo test --all-targets "),
            ("persian", "user@host:~/project $ سلام دنیا ۱۲۳ abc "),
        ] {
            let row = cells(&text.repeat(3));
            let begin = Instant::now();
            for _ in 0..10_000 {
                black_box(BidiLine::new(black_box(&row)));
            }
            eprintln!("{name}: 10000 rows, {} columns: {:?}", row.len(), begin.elapsed());

            let rows: Vec<_> = (0..1000).map(|_| row.iter().collect()).collect();
            let begin = Instant::now();
            black_box(BidiLine::from_rows(black_box(&rows)));
            eprintln!("{name}: one 1000-row paragraph: {:?}", begin.elapsed());
        }
    }
}
