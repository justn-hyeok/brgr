//! Renders a byte stream through both candidate emulators and writes each screen
//! as text. Usage: read-screen ROWS COLS OUT_PREFIX [--text] < STREAM
//! `--text` turns the bare newlines of a captured text fixture into CRLF.
use alacritty_terminal::{
    index::{Column, Line},
    term::{Config, Term, cell::Flags, test::TermSize},
    vte::ansi::{Processor, StdSyncHandler},
};
use std::{
    env, fs,
    io::{self, Read},
};

fn alacritty(bytes: &[u8], rows: usize, cols: usize) -> String {
    let size = TermSize::new(cols, rows);
    let mut term = Term::new(
        Config::default(),
        &size,
        alacritty_terminal::event::VoidListener,
    );
    let mut processor = Processor::<StdSyncHandler>::new();
    processor.advance(&mut term, bytes);
    let grid = term.grid();
    let mut text = String::new();
    for line in 0..rows as i32 {
        let mut row = String::new();
        for column in 0..cols {
            let cell = &grid[Line(line)][Column(column)];
            if !cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                row.push(cell.c);
            }
        }
        text.push_str(row.trim_end());
        text.push('\n');
    }
    text
}

fn main() -> io::Result<()> {
    let args = env::args().collect::<Vec<_>>();
    let (rows, cols): (usize, usize) = (args[1].parse().unwrap(), args[2].parse().unwrap());
    let mut bytes = Vec::new();
    io::stdin().read_to_end(&mut bytes)?;
    if args.get(4).map(String::as_str) == Some("--text") {
        bytes = String::from_utf8_lossy(&bytes)
            .replace('\n', "\r\n")
            .into_bytes();
    }
    let mut parser = vt100::Parser::new(rows as u16, cols as u16, 0);
    parser.process(&bytes);
    fs::write(format!("{}.vt100.txt", args[3]), parser.screen().contents())?;
    fs::write(
        format!("{}.alacritty.txt", args[3]),
        alacritty(&bytes, rows, cols),
    )?;
    Ok(())
}
