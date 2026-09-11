use std::{
    collections::HashMap,
    env, fs,
    path::Path,
    process::Command,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, ensure};
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
    let user = env::var("USER").expect("failed to get the current user from `$USER`");
    let home_dir = env::home_dir().expect("failed to get the home directory path");

    let ssh_configs = parse_ssh_config(
        &fs::read_to_string(home_dir.join(".ssh/config"))
            .expect("failed to read the SSH config file"),
    )
    .expect("failed to parse the SSH config");

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
                "`{}`:`{}` error {}:\n=== stdout ===\n{}\n=== stderr ===\n{}",
                site.working_dir,
                site.build_command,
                output.status,
                String::from_utf8_lossy(&output.stdout).trim(),
                String::from_utf8_lossy(&output.stderr).trim(),
            );
            break;
        }

        let (host, path) = site
            .target
            .split_once(':')
            .unwrap_or_else(|| panic!("bad target syntax: {}", site.target));

        let session = SSHSession::connect(
            ssh_configs
                .get(host)
                .unwrap_or_else(|| panic!("failed to find the host `{host}` in SSH config")),
            &user,
        )
        .await
        .unwrap_or_else(|e| panic!("SSH: failed to connect the host `{host}`: {e:?}"));

        let path = Path::new(path);
        let parent = path.parent().unwrap();
        let name = path.file_name().unwrap().to_str().unwrap();
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let name_ts = format!("{name}_{ts}");
        let archive = format!("{name_ts}.tar.zst");

        session
            .copy(
                parent.join(&archive),
                &compress(&site.build_output, &name_ts).unwrap_or_else(|e| {
                    panic!(
                        "failed to compress the directory `{}: {e:?}`",
                        site.build_output
                    )
                }),
            )
            .await
            .expect("failed to copy file via SSH");
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
                parent.to_str().unwrap()
            ))
            .await
            .expect("failed to execute remote command")
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
