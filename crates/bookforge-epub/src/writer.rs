use std::path::Path;

use bookforge_core::{BookforgeError, Result, ir::Book, segment::Segment};

pub fn rebuild_epub(_book: &Book, _translations: &[Segment], _output: &Path) -> Result<()> {
    Err(BookforgeError::NotImplemented("EPUB rebuild"))
}
