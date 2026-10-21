# runns

Run a command inside an existing Linux network namespace (minimal `ip netns exec`).

The daemon that creates the namespace (TUN, nftables, tun2socks) is the separate [`netns-singbox`](../netns-singbox/) crate.

## Usage

Requires [go-task](https://taskfile.dev/installation/) (`task` on PATH).

```bash
cd runns
task build
sudo task install

runns agent -- curl -4 https://ifconfig.me
```

From repo root: `task runns:build` and `sudo task runns:install`.

`sudo task install` copies the release binary to `/usr/local/bin/runns` and sets `cap_sys_admin`, so a normal user can run it without sudo. `sudo runns` also works: it joins the namespace as root, then runs the command as the user who invoked sudo. Host `http_proxy` / `https_proxy` / `all_proxy` values are removed so curl does not try the host's `127.0.0.1` proxy (that address is an empty loopback inside the namespace).

A root shell that did not come from sudo stays root.

```
Usage: runns <netns> [--] <command> [args...]
```

- **`RUNNS_NETNS`** — default netns name if the first argument is omitted (optional).

Requires a persistent netns (e.g. from the running **`netns-singbox.service`**). Does not create namespaces itself.

Joining a root-owned netns needs `CAP_SYS_ADMIN`. After `sudo task install`, the binary has `cap_sys_admin` (`setcap cap_sys_admin+ep`). With `sudo runns`, the join happens as root; `runns` then switches to `SUDO_UID` and drops capabilities, so the command cannot change nftables or routes. As a normal user, `runns` only joins `/run/netns/<name>` when **netns-singbox** has published `/run/netns-singbox/<name>.user` for that uid (`NETNS_USER`).

If `/etc/netns/<name>/` exists (e.g. `resolv.conf` from **netns-singbox**), **runns** bind-mounts those files over `/etc/` like `ip netns exec` — so the host `127.0.0.53` stub is not used inside the namespace.

## Development

```bash
task build
task test
```

This package is both the `runns` binary and a small library: the owner-stamp protocol (`/run/netns-singbox/<name>.user`). `netns-singbox` depends on that library.

## Requirements

- Linux with `setns(2)`, `iproute2`-style netns at `/run/netns`
- `cap_sys_admin` on the installed binary for non-root use (`sudo task install` runs `setcap`); root otherwise. `libcap2-bin` provides `setcap`.
