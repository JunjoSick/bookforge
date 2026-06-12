use anyhow::Result;
use bookforge_core::{
    config::SegmentationConfig,
    ir::{BlockKind, Book},
    segment::{Segment, build_segments},
};
use bookforge_epub::{inspect_epub, read_epub, text_coverage};
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

    #[arg(long, default_value_t = 1_200)]
    pub max_segment_tokens: usize,
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

    let coverage = text_coverage(&args.input)?;
    println!(
        "Text coverage: {:.1}% ({} of {} visible characters in translatable blocks)",
        coverage.percent(),
        coverage.captured_chars,
        coverage.total_chars
    );
    let mut low = coverage
        .files
        .iter()
        .filter(|file| file.uncaptured_chars() > 0 && file.percent() < 99.0)
        .collect::<Vec<_>>();
    low.sort_by(|left, right| {
        left.percent()
            .partial_cmp(&right.percent())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    if !low.is_empty() {
        println!(
            "Low-coverage files (text the translator will never see, {} total):",
            low.len()
        );
        for file in low.iter().take(10) {
            println!(
                "  {}: {:.1}% ({} uncaptured characters)",
                file.href,
                file.percent(),
                file.uncaptured_chars()
            );
        }
        if low.len() > 10 {
            println!("  ... and {} more", low.len() - 10);
        }
    }

    if args.structure || args.segments {
        let book = read_epub(&args.input)?;

        if args.structure {
            print_structure(&book);
        }

        if args.segments {
            let config = SegmentationConfig {
                max_segment_tokens: args.max_segment_tokens,
                ..SegmentationConfig::default()
            };
            let segments = build_segments(&book, &config)?;
            print_segments(&segments);
        }
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

fn print_segments(segments: &[Segment]) {
    println!("Segment count: {}", segments.len());
    for segment in segments {
        let block_ids = segment
            .block_ids
            .iter()
            .map(|block_id| block_id.0.as_str())
            .collect::<Vec<_>>()
            .join(",");
        println!(
            "{} ordinal={} section={} blocks={} tokens={} checksum={}",
            segment.id.0,
            segment.ordinal,
            segment.section_id.0,
            block_ids,
            segment.source.token_estimate,
            &segment.checksum[..12]
        );
    }
}
