# ikumo

Build and deploy sites to your servers via SSH.

```console
$ ikumo -h
Usage: ikumo <COMMAND>

Commands:
  list    List all configured sites
  deploy  Build and publish sites
  help    Print this message or the help of the given subcommand(s)

Options:
  -h, --help  Print help
```

## Config and example

```toml
# config.toml

[foo]
working_dir = "~/repo/foo"
build_command = "pnpm i && pnpm build"
build_output = "~/repo/foo/dist"
target = "node-LHR:/srv/kumo/foo"
```

```nginx
server {
    listen 443 ssl;
    server_name foo.example.com;
    root /srv/kumo/foo;
}
```

## What does it do?

1. Build the project.
2. Archive and compress the output directory to `.tar.zst`.
3. Connect to the server according to `$HOME/.ssh/config` and got authenticated via `ssh-agent`.
4. Transfer the compressed file to the server.
5. Uncompress and switch to the new version via symbolic link safely.

## Purpose

> “What’s **mine** should be controlled by **me**, not Vercel or Cloudflare.”
