use crate::config::Config;

const LOGO: &str = r#"
  ╔═╗╦ ╦╦  ╔═╗╔═╗   ╔╗╔╦ ╦╦  ╦
  ╠═╝║ ║║  ╚═╗║╣    ║║║║ ║║  ║
  ╩  ╚═╝╩═╝╚═╝╚═╝───╝╚╝╚═╝╩═╝╩═╝"#;

const SEPARATOR: &str = "  ─────────────────────────────────────────────────────────────";

/// Render the startup banner to stdout.
pub fn render(config: &Config, plugin_count: usize) {
    let version = env!("CARGO_PKG_VERSION");

    // Logo lines (skip first empty line)
    let logo_lines: Vec<&str> = LOGO.lines().skip(1).collect();

    // Metadata lines aligned to the right of the logo
    let meta_lines = [
        format!("entity  {}", config.entity.name),
        format!("user    {}", config.entity.owner_alias),
        format!("model   {}", config.llm.model),
        format!(
            "plugins {}",
            if plugin_count > 0 {
                format!("{} configured", plugin_count)
            } else {
                "none".to_string()
            }
        ),
    ];

    println!();

    // Print logo + metadata side by side
    let logo_width = 40;
    for (i, logo_line) in logo_lines.iter().enumerate() {
        if i < meta_lines.len() {
            println!("{:<width$}{}", logo_line, meta_lines[i], width = logo_width);
        } else {
            println!("{}", logo_line);
        }
    }

    // Version line below logo
    println!("  v{}", version);

    println!("{}", SEPARATOR);
    println!();
}
