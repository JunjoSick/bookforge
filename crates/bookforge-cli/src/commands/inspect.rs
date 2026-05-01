use anyhow::Result;
use bookforge_epub::inspect_epub;
use clap::Args;
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
        println!("Structure: pending Milestone 3");
    }

    if args.segments {
        println!("Segments: pending Milestone 4");
    }

    Ok(())
}

fn status(value: bool) -> &'static str {
    if value { "present" } else { "missing" }
}
