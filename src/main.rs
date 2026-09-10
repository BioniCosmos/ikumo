use std::{fs, process::Command};

use serde::Deserialize;
use tokio::main;

#[derive(Debug, Deserialize)]
#[allow(unused)]
struct Site {
    working_dir: String,
    build_command: String,
    build_output: String,
    target: String,
    reload_command: String,
}

#[derive(Deserialize)]
struct Config {
    #[serde(rename = "site")]
    sites: Vec<Site>,
}

#[main]
async fn main() {
    let Config { sites } =
        toml::from_str(&fs::read_to_string("config.toml").expect("failed to read the config file"))
            .expect("failed to parse the config");
    for site in sites {
        let output = Command::new("bash")
            .arg("-c")
            .arg(&site.build_command)
            .current_dir(&site.working_dir)
            .output()
            .expect("fail to spawn the build command");
        if !output.status.success() {
            eprintln!(
                "`{}`:`{}` error {}:\n\n===stdout===\n{}\n===stderr===\n{}",
                site.working_dir,
                site.build_command,
                output.status,
                String::from_utf8_lossy(&output.stdout).trim(),
                String::from_utf8_lossy(&output.stderr).trim(),
            );
            break;
        }
    }
}
