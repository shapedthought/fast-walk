# Running fast-walk with Docker

If you would rather not install a toolchain, the `Dockerfile` builds it for you:

    docker build -t fast-walk .

Scanning needs two mounts: the tree to look at, and somewhere for the results to land. The scan target can be read-only, since nothing is ever written to it:

    docker run --rm -v /srv/share:/scan:ro -v "$PWD":/out fast-walk -p /scan

Every option works as it does outside a container, so `-p /scan --skip-hidden -o monday` behaves the same way. The results go to the working directory, which is why `/out` is mounted; without it the CSVs are written inside the container and disappear with it.

If the binary is what you actually want, take it and drop the image. It is a normal dynamically linked glibc binary, so it runs on any comparable Linux, not only inside a container:

    docker create --name fw fast-walk
    docker cp fw:/usr/local/bin/fast-walk .
    docker rm fw

**A container runs as root by default, and root bypasses permission bits.** For this tool that is not a detail: a scan as root reports files that the account running your backup may not be able to read, so the totals can come out higher than what will actually be protected. Pass `--user` to scan as yourself, which also leaves the results files owned by you rather than by root:

    docker run --rm --user "$(id -u):$(id -g)" \
        -v /srv/share:/scan:ro -v "$PWD":/out fast-walk -p /scan

## Mounting a share inside the container

The image carries `cifs-utils` and `nfs-common`, so a share can be mounted in the container rather than on the host. That means you do not need mount helpers on the machine you are running from, and the mount disappears when the container exits rather than being left behind.

Mounting needs two Linux capabilities that a container does not get by default, and they are granted with `--cap-add` on the `docker run` line itself:

    docker run --cap-add SYS_ADMIN --cap-add DAC_READ_SEARCH ...

One `--cap-add` per capability; there is no combined form. The names are case-insensitive and the `CAP_` prefix is optional, so `--cap-add CAP_SYS_ADMIN` and `--cap-add sys_admin` do the same thing. Under Docker Compose the same two go in a `cap_add:` list on the service and produce an identical result:

    services:
      scan:
        image: fast-walk
        cap_add:
          - SYS_ADMIN
          - DAC_READ_SEARCH

The Kubernetes equivalent is `securityContext.capabilities.add`, which has not been tried here.

`CAP_SYS_ADMIN` is for `mount` itself. `CAP_DAC_READ_SEARCH` is for `mount.cifs`, which is setuid and gives up with `Unable to apply new capability set` without it — `SYS_ADMIN` on its own is not enough, and adding `SETPCAP` instead does not help. `--privileged` is not required, which is worth keeping that way given what these containers get pointed at.

**On a Debian or Ubuntu host you also need `--security-opt apparmor=unconfined`.** Docker applies its `docker-default` AppArmor profile there, and that profile denies `mount` regardless of what capabilities the container holds. The denial happens at the system call, so the kernel's CIFS code never runs and never logs anything, and all you get is a bare `mount error(13): Permission denied` — which looks exactly like a rejected password and sends you to check your credentials, where there is nothing wrong. If you see error 13 and `sudo dmesg` shows no `CIFS: VFS:` lines at all, this is why; `sudo dmesg | grep -i apparmor` should show `apparmor="DENIED" operation="mount"`.

The flag is in the examples below unconditionally because it is accepted and does nothing on hosts with no AppArmor, such as Docker Desktop. Be aware that it drops the whole profile for that container rather than just the mount rule. A tighter option is a custom profile permitting `mount fstype=cifs` and nothing else, which is more work and has not been tried here. Hosts running SELinux instead have not been tried either, and may well need their own equivalent.

To check what a container actually ended up with rather than what you meant to ask for, run `grep CapEff /proc/self/status` inside it. On Docker 29.6 the default came out as `00000000a80425fb` and those two capabilities took it to `00000000a82425ff`; the exact value depends on the daemon's default set, so compare a run with and without the flags rather than trusting the number.

SMB needs credentials. Passing them as `-o user=...,password=...` puts the password in `ps` output and in your shell history, so `mount.cifs` reads them from a file instead. It is two lines, `username` and `password`, plus a `domain` line only if the server is domain joined — a wrong domain on a workgroup machine is rejected as an authentication failure, which sends you looking in the wrong place:

    username=scanner
    password=hunter2

Write it without the password reaching your history, and lock it down. `chmod 600` is not optional: `mount.cifs` is being handed the file and anyone who can read it can read the password:

    read -rp 'username: ' u && read -rsp 'password: ' p && echo
    printf 'username=%s\npassword=%s\n' "$u" "$p" > .smbcred
    chmod 600 .smbcred
    unset u p

That writes to the working directory, which is convenient and is also wherever you happen to be — quite possibly a checkout of something. `.smbcred` is in this repository's `.gitignore` and `.dockerignore`, but if you are working somewhere else, put it outside the tree or add it there too. Delete it when you are done.

Then mount with `credentials=` pointing at where the file is mounted in the container, not at where it lives on the host:

    docker run --rm --cap-add SYS_ADMIN --cap-add DAC_READ_SEARCH \
        --security-opt apparmor=unconfined \
        -v "$PWD":/out -v "$PWD/.smbcred":/creds:ro \
        --entrypoint bash fast-walk -c '
            set -euo pipefail
            mkdir -p /scan
            mount -t cifs //fileserver/data /scan -o ro,vers=3.0,credentials=/creds
            fast-walk -p /scan'

**Keep the `set -euo pipefail`.** Without it a failed mount does not stop the script, so `fast-walk` scans the empty directory that was going to be the mountpoint, reports zero files and exits successfully. To anything reading the exit status that is indistinguishable from an empty share, which is the exact confusion this tool exists to prevent. With it, the run stops at the mount and exits non-zero.

Three things produce a confusing `mount error(13): Permission denied` before you have even reached a real authentication problem. In likelihood order on a Linux host:

- **AppArmor denied the mount**, as above. This is the one to rule out first on Debian or Ubuntu, and the giveaway is that `dmesg` has no `CIFS: VFS:` lines at all, because the kernel never got as far as talking to the server.
- **The credentials file does not exist on the host.** Docker creates a *directory* at a bind-mount source that is missing, rather than failing, so `credentials=` is handed a directory, `mount.cifs` falls back to prompting for a password, and a container with no terminal answers with nothing. Check with `ls -ld .smbcred` on the host: if it is a directory, delete it and write the file. `docker run --rm -v "$PWD/.smbcred":/creds:ro --entrypoint bash fast-walk -c 'cat /creds'` should print your two lines.
- **Stray whitespace or CRLF line endings.** `mount.cifs` takes the value literally, so a trailing space or a `\r` becomes part of the password.

On a Linux host `sudo dmesg | tail` then gives the real reason for anything that is left: `Send error in SessSetup = -13` is genuinely the credentials or the domain.

If the mount fails, the number in the message is the errno and is usually enough on its own: `mount error(13): Permission denied` is authentication, and `mount error(2): No such file or directory` is a share name that does not exist. `mount.cifs` will tell you to check `dmesg`, and the kernel's `CIFS: VFS:` lines do carry more, but the kernel log is not readable with the two capabilities above — add `--cap-add SYSLOG` when you need it. That is a difference from running on a host, where `dmesg` is simply there.

NFS is the same shape, with the mount options the docs recommend — `soft` in particular, so an unresponsive server fails visibly instead of hanging the scan forever:

    docker run --rm --cap-add SYS_ADMIN --cap-add DAC_READ_SEARCH \
        --security-opt apparmor=unconfined \
        -v "$PWD":/out \
        --entrypoint bash fast-walk -c '
            set -euo pipefail
            mkdir -p /scan
            mount -t nfs -o ro,soft,timeo=100,retrans=3 nas.example.com:/vol/data /scan
            fast-walk -p /scan'

The mount commands are the ones in [docs/snapshot-scanning.md](docs/snapshot-scanning.md), not a separate interface: everything that document says about credentials files, `vers=`, `soft` and confirming that `ro` actually took applies unchanged here. Read it before pointing this at a share you care about.

## What has been checked

Dependencies are built in their own layer, so editing the source and rebuilding does not recompile them: a cold build took 22.6 seconds here and a rebuild after a source change took 2.7, with only `fast-walk` itself recompiling. The Rust version is pinned in the `Dockerfile` rather than tracking `latest`, so it needs bumping deliberately.

On Docker 29.6 with Docker Desktop on macOS and an arm64 image: the container scans the standard fixture to 20,232 files and 1,435,762,672 bytes, matching the documented totals exactly; all six CSVs land in the mounted directory; `--cpus=2` is picked up correctly, so the thread default respects a container CPU limit rather than seeing the whole host; and a `--user` run produces the same totals with the output owned by the caller.

The same fixture was then scanned three ways from inside this image — locally, over SMB from a Samba server, and over NFSv4.2 from a Linux `nfsd` — and all six CSVs came out byte identical every time. That exercises the client side only. A Samba container is not a NAS, the NFS run was v4.2 with no v3 path tested, and snapshot directory traversal was not exercised at all; see [TESTING.md](TESTING.md) for where the line sits. The image has not been built or run on a Linux host, nor on amd64.
