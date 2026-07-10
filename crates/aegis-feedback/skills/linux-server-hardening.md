---
id: linux-server-hardening
trigger: "(?i)(harden|hardening|secure a( linux)? server|server security|sshd?_config|firewall|ufw|fail2ban|iptables|crowdsec|lynis|new (vps|server) setup|加固|服务器安全|防火墙|服务器初始化)"
version: 4
status: active
origin: builtin
enabled: true
description: "Playbook for hardening a Linux server (SSH, firewall, intrusion detection, auditing). Use when setting up, securing, or auditing a server / VPS."
keywords: ["harden", "hardening", "server security", "ssh", "sshd_config", "firewall", "ufw", "fail2ban", "psad", "crowdsec", "lynis", "aide", "clamav", "rkhunter", "2fa", "mfa", "vps", "sudo", "unattended-upgrades", "hidepid", "加固", "服务器", "服务器安全", "防火墙", "安全加固", "入侵检测", "初始化"]
---
# Linux Server Hardening

Aegis's operational skill for hardening a Linux server. Commands below are standard,
non-copyrightable practice expressed in Aegis's own words, grounded in the **primary
authoritative sources** listed at the end — go fetch those (e.g. via `web_extract`)
when you need the full detail. Ordered by **attack surface and blast radius**.
Debian/Ubuntu shown; adapt the package manager for other distros.

## Operating discipline (before touching anything)
- **Least privilege, deny by default.** Only allow/open what a service actually needs.
- **Keep a second root/SSH session open** while changing SSH or the firewall, so a
  mistake can't lock you out.
- **Back up every file before editing:** `sudo cp --archive FILE FILE.bak-$(date +%s)`.
- **Verify after each change**; have out-of-band console/rescue access before you
  disable root, lock accounts, or password-protect the bootloader.

## Priority 1 — Remote access (SSH: the largest exposed surface)
- Prefer key auth: on the *client* `ssh-keygen -t ed25519`, then `ssh-copy-id user@host`.
- In `/etc/ssh/sshd_config` (align with the Mozilla OpenSSH baseline):
  `PermitRootLogin no`, `PasswordAuthentication no`, `PermitEmptyPasswords no`,
  `MaxAuthTries 3`, `LoginGraceTime 30`, `X11Forwarding no`, `AllowTcpForwarding no`,
  `LogLevel VERBOSE`, and restrict users via `AllowGroups <ssh-group>`.
- Use only strong `KexAlgorithms`/`Ciphers`/`MACs` and drop DH moduli < 3072 bits.
- **Watch for duplicate contradicting directives** — sshd honours the *first*
  occurrence and silently ignores later ones. This check should print nothing:
  `awk 'NF && $1!~/^(#|HostKey)/{print $1}' /etc/ssh/sshd_config | sort | uniq -c | grep -v ' 1 '`.
- On OpenSSH 9.1+ you can add `RequiredRSASize 3072` (rejects weak RSA; harmless to
  Ed25519/ECDSA). Omit it on older builds or sshd won't start.
- Apply + validate: `sudo sshd -t && sudo systemctl restart ssh && sudo sshd -T`.
- Optional MFA: `libpam-google-authenticator` wired into `/etc/pam.d/sshd` +
  `KbdInteractiveAuthentication yes`.

## Priority 2 — Identity & privilege
- Grant `sudo` only to an explicit admin group (edit via `visudo`). Optionally restrict
  `su` to a group too: `sudo dpkg-statoverride --update --add root <su-group> 4750 /bin/su`.
- Enforce password quality with `libpam-pwquality` in `/etc/pam.d/common-password`.
- Keep the clock correct — many controls depend on it. On modern Debian/Ubuntu the
  client is `systemd-timesyncd`: `sudo timedatectl set-ntp true` (the standalone `ntp`
  package was dropped in Debian 13).

## Priority 3 — Network exposure (host firewall)
- Default-deny, allow by exception, rate-limit SSH. Note `ufw limit` auto-blocks a
  source that opens 6+ connections within 30s — ideal for the SSH port:
  ```
  sudo apt install ufw
  sudo ufw default deny incoming
  sudo ufw limit in ssh
  sudo ufw allow out 53; sudo ufw allow out 123      # DNS, NTP
  sudo ufw allow out http; sudo ufw allow out https  # updates
  sudo ufw enable && sudo ufw status verbose
  ```
- Audit what's actually listening and remove anything unexpected: `sudo ss -lntup`.

## Priority 4 — Detection & response
- `fail2ban` to ban brute-force sources (SSH jail: `maxretry = 5`, `banaction = ufw`).
- Optionally add `psad` (iptables scan/DoS detection) — it reads iptables logs, so
  first make UFW log by adding `-A INPUT -j LOG --log-prefix "[IPTABLES] "` in
  `/etc/ufw/before.rules` (before the `COMMIT` line). Or use `crowdsec` for
  crowd-sourced blocklists. `firejail` sandboxes risky apps.

## Priority 5 — Integrity & audit
- Run an auditor and work the findings: `sudo lynis audit system`.
- File-integrity baseline with `aide`; rootkit scans with `rkhunter`/`chkrootkit`;
  on-demand malware scans with `clamav`; daily log summaries with `logwatch`.
  For a fuller host IDS, consider `ossec`.

## Priority 6 — Patch hygiene
- `unattended-upgrades` (+ `apt-listchanges`, `apticron`) for automatic security
  patches and pending-update mail. Constrain it to security + stable origins,
  blacklist packages you don't want auto-upgraded, and keep recovery access — an
  automatic update/reboot can occasionally break a service. Ensure the host can send
  mail (e.g. `msmtp`/`exim4`) so alerts actually arrive.

## Priority 7 — Deeper, higher-risk (do last, understand first)
- Hide other users' processes: mount `/proc` with `hidepid=2` (add
  `proc /proc proc defaults,hidepid=2 0 0` to `/etc/fstab`) so unprivileged users
  can't inspect others' processes — but test it, as some systemd setups dislike it.
- Kernel `sysctl` hardening (test with `sysctl -w` before persisting).
- Password-protect GRUB; disable direct root login (`sudo passwd -l root`).
- Tighten default `umask` (e.g. 0027 users / 0077 root); prune orphaned packages;
  enable disk encryption at install; consider a MAC layer (AppArmor/SELinux).

## Before declaring "done"
- Confirm you can still log in from a **second** session with the new SSH config.
- Cross-check against an independent standard (CIS Benchmark for your OS) and let it
  override this list where they differ. Re-run `lynis` and `ufw status verbose`.

## Authoritative sources (fetch for full detail)
These are the primary references this skill is grounded in. When a task needs depth,
open the relevant one with `web_extract` rather than guessing:
- SSH hardening: Mozilla OpenSSH Guidelines — https://infosec.mozilla.org/guidelines/openssh
- SSH keys (Ed25519): Linux Audit — https://linux-audit.com/using-ed25519-openssh-keys-instead-of-dsa-rsa-ecdsa/
- Comprehensive OS baselines: CIS Benchmarks — https://www.cisecurity.org/cis-benchmarks/
- Firewall: Ubuntu UFW — https://help.ubuntu.com/community/UFW
- Brute-force defense: Fail2ban — https://www.fail2ban.org/ ; CrowdSec — https://docs.crowdsec.net/
- App sandboxing: Firejail — https://firejail.wordpress.com/
- Security auditing: Lynis — https://cisofy.com/lynis/
- File integrity: AIDE — https://aide.github.io/
- Host IDS: OSSEC — https://www.ossec.net/docs/
- Auto-updates: Debian UnattendedUpgrades — https://wiki.debian.org/UnattendedUpgrades
- General reference: Arch Wiki Security — https://wiki.archlinux.org/title/Security
