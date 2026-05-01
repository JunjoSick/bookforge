use anyhow::Result;
use bookforge_core::ir::{BlockKind, Book};
use bookforge_epub::{inspect_epub, read_epub};
use clap::Args;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Args)]
pub struct InspectArgs {
    pub input: PathBuf,

    #[arg(long)]
    pub structure: bool,

    #[arg(long)]
    pub segments: bool,
}

pub async fn run(args: InspectArgs) -> Result<()> {
    let inspection = inspect_epub(&args.input)?;

    println!("Input: {}", args.input.display());
    println!(
        "Title: {}",
        inspection.title.as_deref().unwrap_or("(untitled)")
    );
    println!("Package: {}", inspection.package_path);
    println!("Spine count: {}", inspection.spine_count);
    println!("Manifest count: {}", inspection.manifest_count);
    println!("XHTML count: {}", inspection.xhtml_count);
    println!("XHTML spine count: {}", inspection.xhtml_spine_count);
    println!(
        "Nav/TOC status: nav={}, toc={}",
        status(inspection.has_nav),
        status(inspection.has_toc)
    );
    println!("Resource count: {}", inspection.resource_count);

    if args.structure {
        print_structure(&read_epub(&args.input)?);
    }

    if args.segments {
        println!("Segments: pending Milestone 4");
    }

    Ok(())
}

fn status(value: bool) -> &'static str {
    if value { "present" } else { "missing" }
}

fn print_structure(book: &Book) {
    let mut counts = BTreeMap::<&'static str, usize>::new();
    let total_tokens = book
        .blocks
        .iter()
        .map(|block| {
            *counts.entry(block_kind_label(block.kind)).or_default() += 1;
            block.token_estimate
        })
        .sum::<usize>();

    println!("Section count: {}", book.sections.len());
    println!("Block count: {}", book.blocks.len());
    println!("Block count by kind:");
    for (kind, count) in counts {
        println!("  {kind}: {count}");
    }
    println!("Estimated token count: {total_tokens}");
}

fn block_kind_label(kind: BlockKind) -> &'static str {
    match kind {
        BlockKind::Heading(_) => "heading",
        BlockKind::Paragraph => "paragraph",
        BlockKind::ListItem => "list_item",
        BlockKind::Quote => "quote",
        BlockKind::TableCell => "table_cell",
        BlockKind::TableRow => "table_row",
        BlockKind::Footnote => "footnote",
        BlockKind::Caption => "caption",
        BlockKind::Code => "code",
        BlockKind::Unknown => "unknown",
    }
}
