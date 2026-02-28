use std::str::FromStr;

use cron::Schedule as CronSchedule;

use super::{OutputRouting, ScheduledTask, TaskCreator};

/// Parse a [SCHEDULE: {...}] JSON marker into a ScheduledTask.
pub fn create_task_from_marker(
    json_str: &str,
) -> Result<ScheduledTask, Box<dyn std::error::Error + Send + Sync>> {
    let value: serde_json::Value = serde_json::from_str(json_str)?;

    let name = value["name"]
        .as_str()
        .ok_or("Missing 'name' in schedule marker")?
        .to_string();

    let cron = value["cron"]
        .as_str()
        .ok_or("Missing 'cron' in schedule marker")?
        .to_string();

    let prompt = value["prompt"]
        .as_str()
        .ok_or("Missing 'prompt' in schedule marker")?
        .to_string();

    // Validate the cron expression
    CronSchedule::from_str(&cron)
        .map_err(|e| format!("Invalid cron expression '{}': {}", cron, e))?;

    // Generate a stable id from the name
    let id = name
        .to_lowercase()
        .replace(|c: char| !c.is_alphanumeric(), "-")
        .trim_matches('-')
        .to_string();

    let output_routing = match value["output"].as_str() {
        Some("share") => OutputRouting::Share,
        Some("call") => OutputRouting::Call,
        _ => OutputRouting::Silent,
    };

    Ok(ScheduledTask {
        id,
        name,
        cron,
        channel: "system".to_string(),
        prompt,
        output_routing,
        enabled: true,
        created_by: TaskCreator::Entity,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_task_from_valid_marker() {
        let json = r#"{"name": "follow-up-foucault", "cron": "0 0 14 * * *", "prompt": "Continue researching Foucault."}"#;
        let task = create_task_from_marker(json).unwrap();
        assert_eq!(task.name, "follow-up-foucault");
        assert_eq!(task.cron, "0 0 14 * * *");
        assert!(task.prompt.contains("Foucault"));
        assert_eq!(task.created_by, TaskCreator::Entity);
        assert!(task.enabled);
    }

    #[test]
    fn reject_missing_name() {
        let json = r#"{"cron": "0 0 14 * * *", "prompt": "Do something."}"#;
        assert!(create_task_from_marker(json).is_err());
    }

    #[test]
    fn reject_invalid_cron() {
        let json = r#"{"name": "bad", "cron": "not a cron", "prompt": "Do something."}"#;
        assert!(create_task_from_marker(json).is_err());
    }
}
