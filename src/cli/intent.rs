use chrono::Utc;
use console::style;

use crate::config::Config;
use crate::scheduler::intent::{Intent, IntentOutput, IntentPriority, IntentQueue, IntentSource};

pub async fn list() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load()?;
    let root_dir = config.root_dir()?;
    let queue = IntentQueue::load(&root_dir);

    if queue.is_empty() {
        println!("  No pending intents.");
        return Ok(());
    }

    println!();
    println!("  {}", style("Intent Queue").bold());
    println!();

    for intent in queue.list() {
        let priority = match intent.priority {
            IntentPriority::Low => style("low").dim(),
            IntentPriority::Normal => style("normal").white(),
            IntentPriority::High => style("high").yellow(),
            IntentPriority::Urgent => style("urgent").red().bold(),
        };

        let age = Utc::now() - intent.created_at;
        let age_str = if age.num_hours() > 0 {
            format!("{}h ago", age.num_hours())
        } else {
            format!("{}m ago", age.num_minutes())
        };

        println!(
            "  {} [{}] ({})",
            style(&intent.description).cyan().bold(),
            priority,
            age_str,
        );
        println!("    ID: {}", style(&intent.id).dim());
        if intent.depth > 0 {
            println!("    Depth: {}", intent.depth);
        }
        println!();
    }

    println!("  {} pending intent(s)", queue.len());
    Ok(())
}

pub async fn add(
    description: String,
    prompt: String,
    priority: String,
) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load()?;
    let root_dir = config.root_dir()?;

    let priority = match priority.as_str() {
        "low" => IntentPriority::Low,
        "high" => IntentPriority::High,
        "urgent" => IntentPriority::Urgent,
        _ => IntentPriority::Normal,
    };

    let id = format!("intent-cli-{}", &uuid::Uuid::new_v4().to_string()[..8]);

    let intent = Intent {
        id: id.clone(),
        description: description.clone(),
        prompt,
        source: IntentSource::UserCli,
        priority,
        created_at: Utc::now(),
        chain: None,
        output_routing: IntentOutput::Silent,
        depth: 0,
    };

    let max_size = config.autonomy.max_queue_size;
    let mut pushed = false;
    IntentQueue::save_delta(&root_dir, |q| pushed = q.push(intent, max_size))?;
    if pushed {
        println!(
            "  Intent '{}' added (id: {})",
            style(&description).cyan(),
            id
        );
    } else {
        println!("  {}", style("Queue is full or duplicate detected.").red());
    }

    Ok(())
}

pub async fn remove(id: String) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load()?;
    let root_dir = config.root_dir()?;

    let mut removed = false;
    IntentQueue::save_delta(&root_dir, |q| removed = q.remove(&id))?;
    if removed {
        println!("  Intent '{}' removed.", style(&id).cyan());
    } else {
        println!("  Intent '{}' not found.", style(&id).red());
    }

    Ok(())
}

pub async fn clear() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load()?;
    let root_dir = config.root_dir()?;

    let mut count = 0;
    IntentQueue::save_delta(&root_dir, |q| {
        count = q.len();
        q.clear();
    })?;

    println!("  Cleared {} intent(s).", count);
    Ok(())
}
