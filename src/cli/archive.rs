use console::style;

use crate::config::Config;
use crate::pipeline::archive;

pub async fn list(document: Option<String>) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load()?;
    let root_dir = config.root_dir()?;

    let files = archive::list_archives(&root_dir, document.as_deref())?;

    println!();
    if files.is_empty() {
        println!("  No archived files found.");
    } else {
        println!("  {}", style("Archived Files").bold());
        println!();
        for file in &files {
            println!("  {}", file);
        }
    }
    println!();

    Ok(())
}

pub async fn run(document: String) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load()?;
    let root_dir = config.root_dir()?;

    let result = archive::archive_document_by_name(&root_dir, &document)?;
    println!();
    println!("  {} {}", style("✓").green(), result);
    println!();

    Ok(())
}
