use std::{collections::HashMap, fs, process::Command};

use anyhow::bail;
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

#[derive(Clone, Debug, Default, PartialEq)]
struct SSHConfig {
    host_name: String,
    port: u16,
    user: String,
}

fn parse_ssh_config(raw: &str) -> anyhow::Result<HashMap<String, SSHConfig>> {
    let mut m = HashMap::new();

    let raw = raw.as_bytes();
    let mut i = 0;

    macro_rules! consume_line {
        () => {{
            while i < raw.len() {
                if raw[i] == b'\n' {
                    break;
                }
                i += 1;
            }
            i += 1;
        }};
    }

    macro_rules! read_value {
        ($start:expr) => {{
            i = $start;

            while i < raw.len() {
                if raw[i] != b' ' {
                    break;
                }
                i += 1;
            }
            let start = i;

            consume_line!();

            &raw[start..i - 1]
        }};
    }

    let mut host = "";
    let mut config = SSHConfig::default();

    while i < raw.len() {
        match raw[i] as char {
            ' ' | '\t' | '\n' => i += 1,
            'H' => {
                if i + 8 <= raw.len() && &raw[i..i + 8] == b"HostName" {
                    if host.is_empty() {
                        bail!("missing `Host`");
                    }
                    config.host_name = String::from_utf8(read_value!(i + 8).to_vec())?;
                } else if i + 4 <= raw.len() && &raw[i..i + 4] == b"Host" {
                    if !host.is_empty() {
                        m.insert(host.to_owned(), config.clone());
                        config = SSHConfig::default();
                    }
                    host = str::from_utf8(read_value!(i + 4))?
                } else {
                    consume_line!();
                }
            }
            'P' => {
                if i + 4 <= raw.len() && &raw[i..i + 4] == b"Port" {
                    if host.is_empty() {
                        bail!("missing `Host`");
                    }
                    config.port = str::from_utf8(read_value!(i + 4))?.parse()?;
                } else {
                    consume_line!();
                }
            }
            'U' => {
                if i + 4 <= raw.len() && &raw[i..i + 4] == b"User" {
                    if host.is_empty() {
                        bail!("missing `Host`");
                    }
                    config.user = String::from_utf8(read_value!(i + 4).to_vec())?;
                } else {
                    consume_line!();
                }
            }
            _ => consume_line!(),
        }
    }

    if !host.is_empty() {
        m.insert(host.to_owned(), config.clone());
    }

    Ok(m)
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use super::*;

    #[test]
    fn test_parse_ssh_config() {
        let raw = r#"Host foo
    HostName example.com
    Port 2233
    User me"#;
        let m = parse_ssh_config(raw);
        assert_matches!(m, Ok(_));

        let m = m.unwrap();
        assert_eq!(
            m,
            HashMap::from([(
                "foo".to_owned(),
                SSHConfig {
                    host_name: "example.com".to_owned(),
                    port: 2233,
                    user: "me".to_owned()
                }
            )])
        );
    }
}
