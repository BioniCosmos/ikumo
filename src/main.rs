use std::{
    collections::HashMap,
    env, fs,
    path::Path,
    process::Command,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, bail, ensure};
use clap::{Parser, Subcommand};
use russh::{
    ChannelMsg,
    client::{self, Handle, Handler},
    keys::{PublicKeyOrCertificate, agent::client::AgentClient},
};
use russh_sftp::client::SftpSession;
use serde::Deserialize;
use tar::{Builder, HeaderMode};
use tokio::{io::AsyncWriteExt as _, main};

#[derive(Debug, Deserialize)]
struct Site {
    working_dir: String,
    build_command: String,
    build_output: String,
    target: String,
}

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// List all configured sites
    List,
    /// Build and publish sites
    Deploy {
        /// Site names
        #[arg(required = true, group = "site")]
        sites: Vec<String>,
        /// Deploy all configured sites
        #[arg(short, long, group = "site")]
        all: bool,
    },
}

#[main]
async fn main() {
    let cli = Cli::parse();
    let sites = toml::from_str::<HashMap<String, Site>>(
        &fs::read_to_string("config.toml").expect("failed to read the config file"),
    )
    .expect("failed to parse the config");

    match cli.command {
        Commands::List => println!("{}", sites.keys().cloned().collect::<Vec<_>>().join(" ")),
        Commands::Deploy { sites: names, all } => {
            deploy(&sites, (!all).then_some(&names)).await.unwrap();
        }
    }
}

async fn deploy(sites: &HashMap<String, Site>, names: Option<&Vec<String>>) -> anyhow::Result<()> {
    let user = env::var("USER")?;
    let home_dir = env::home_dir().context("failed to get the home directory path")?;

    let ssh_configs = parse_ssh_config(&fs::read_to_string(home_dir.join(".ssh/config"))?)?;

    let sites: Vec<_> = if let Some(names) = names {
        sites
            .iter()
            .filter(|(name, _)| names.contains(name))
            .map(|(_, site)| site)
            .collect()
    } else {
        sites.values().collect()
    };
    for site in sites {
        let output = Command::new("bash")
            .arg("-c")
            .arg(&site.build_command)
            .current_dir(&site.working_dir)
            .output()?;
        ensure!(
            output.status.success(),
            "`{}`:`{}` error {}:\n=== stdout ===\n{}\n=== stderr ===\n{}",
            site.working_dir,
            site.build_command,
            output.status,
            String::from_utf8_lossy(&output.stdout).trim(),
            String::from_utf8_lossy(&output.stderr).trim(),
        );

        let Some((host, path)) = site.target.split_once(':') else {
            bail!("bad target syntax: {}", site.target)
        };

        let session = SSHSession::connect(
            ssh_configs
                .get(host)
                .context("failed to find the host in SSH config")?,
            &user,
        )
        .await?;

        let path = Path::new(path);
        let parent = path.parent().context("invalid path")?;
        let name = path
            .file_name()
            .context("invalid path")?
            .to_str()
            .context("invalid path")?;
        let ts = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let name_ts = format!("{name}_{ts}");
        let archive = format!("{name_ts}.tar.zst");

        session
            .copy(
                parent.join(&archive),
                &compress(&site.build_output, &name_ts)?,
            )
            .await?;
        session
            .execute(&format!(
                r#"bash -c '
set -e

cleanup() {{
  if [ $? -ne 0 ]; then
    rm -rf {name_ts}
  fi
  if [ -f {archive} ]; then
    rm -rf {archive}
  fi
}}
trap cleanup EXIT

cd {}
tar -xf {archive}
ln -s {name_ts} {name}_t
mv -fT {name}_t {name}
'"#,
                parent.to_str().context("invalid path")?
            ))
            .await?
    }

    Ok(())
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
                    ensure!(!host.is_empty(), "missing `Host`");
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
                    ensure!(!host.is_empty(), "missing `Host`");
                    config.port = str::from_utf8(read_value!(i + 4))?.parse()?;
                } else {
                    consume_line!();
                }
            }
            'U' => {
                if i + 4 <= raw.len() && &raw[i..i + 4] == b"User" {
                    ensure!(!host.is_empty(), "missing `Host`");
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

struct SSHSession {
    session: Handle<SSHHandler>,
}

impl SSHSession {
    async fn connect(config: &SSHConfig, default_user: &str) -> anyhow::Result<Self> {
        let mut session = client::connect(
            Arc::new(client::Config::default()),
            (
                config.host_name.as_str(),
                if config.port == 0 { 22 } else { config.port },
            ),
            SSHHandler,
        )
        .await?;
        let mut agent = AgentClient::connect_env().await?;

        let mut auth_success = false;
        for id in agent.request_identities().await? {
            if session
                .authenticate_publickey_with(
                    if config.user.is_empty() {
                        default_user
                    } else {
                        &config.user
                    },
                    id.public_key().into_owned(),
                    None,
                    &mut agent,
                )
                .await?
                .success()
            {
                auth_success = true;
                break;
            }
        }

        ensure!(auth_success, "SSH authentication failed.");
        Ok(Self { session })
    }

    async fn execute(&self, command: &str) -> anyhow::Result<()> {
        let mut ch = self.session.channel_open_session().await?;
        ch.exec(true, command).await?;

        let mut stdout = String::new();
        let mut stderr = String::new();
        let mut status = 0;
        loop {
            match ch.wait().await {
                None => break,
                Some(msg) => match msg {
                    ChannelMsg::Data { data } => stdout = String::from_utf8(data.to_vec())?,
                    ChannelMsg::ExtendedData { data, ext: 1 } => {
                        stderr = String::from_utf8(data.to_vec())?
                    }
                    ChannelMsg::ExitStatus { exit_status } => status = exit_status,
                    _ => (),
                },
            }
        }

        anyhow::ensure!(
            status == 0,
            "SSH executed command `{}` with status {}:\n=== stdout ===\n{}\n=== stderr ===\n{}",
            command,
            status,
            stdout.trim(),
            stderr.trim(),
        );
        Ok(())
    }

    async fn copy<P: AsRef<Path>>(&self, path: P, data: &[u8]) -> anyhow::Result<()> {
        let ch = self.session.channel_open_session().await?;
        ch.request_subsystem(true, "sftp").await?;
        let sftp = SftpSession::new(ch.into_stream()).await?;

        let mut file = sftp
            .create(path.as_ref().to_str().context("invalid path")?)
            .await?;
        file.write_all(data).await?;
        file.close().await?;

        Ok(())
    }
}

struct SSHHandler;

impl Handler for SSHHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

fn compress<P: AsRef<Path>>(path: P, dir_name: &str) -> anyhow::Result<Vec<u8>> {
    let mut archive = Builder::new(vec![]);
    archive.mode(HeaderMode::Deterministic);
    archive.follow_symlinks(false);
    archive.append_dir_all(dir_name, &path)?;
    zstd::encode_all(archive.into_inner()?.as_slice(), 0).map_err(anyhow::Error::new)
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
