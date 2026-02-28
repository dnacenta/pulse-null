use std::path::PathBuf;

use crate::init::wizard;

pub async fn run(dir: Option<String>) -> Result<(), Box<dyn std::error::Error>> {
    let target_dir = match dir {
        Some(d) => PathBuf::from(d),
        None => std::env::current_dir()?,
    };

    wizard::run(&target_dir).await
}
