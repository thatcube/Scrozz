# Native platform lab

This is the reproducible fourth layer of Scrozz's platform strategy. It exists
for behavior that compilation, CI runners and headless golden images cannot
prove: desktop portals, real captured pixels, compositor integration, focus,
DPI, clipboard transfer, drag behavior and recording teardown.

## Evidence is not an exit code

Every retained run must be classified as exactly one of:

- `pass`: the stated native behavior was observed on the stated platform.
- `fail`: the behavior was exercised and violated its contract.
- `skip`: a prerequisite was absent or the behavior was not exercised.

A command that prints `skipped` is a skip even when it exits zero. A build or
cross-target check is never native runtime evidence. Expected rejection can be
useful evidence - for example, refusing H.264 when no hardware encoder exists -
but it does not prove the unavailable feature works.

Use the evidence wrappers to retain a command before interpreting it:

```bash
export SCROZZ_LAB_ROOT="$HOME/ScrozzPlatformLab"
tools/native-evidence.sh \
  --output "$SCROZZ_LAB_ROOT/runs/gnome-wayland-capture" \
  --label gnome-wayland-capture \
  --source-sha "$EXPECTED_SHA" \
  -- tools/wayland-smoke.sh --require
```

```powershell
$LabRoot = Join-Path $HOME 'ScrozzPlatformLab'
.\tools\native-evidence.ps1 `
  -Output (Join-Path $LabRoot 'runs\windows-capture') `
  -Label windows-capture `
  -SourceSha $ExpectedSha `
  -- pwsh -NoLogo -NoProfile -File .\tools\windows-smoke.ps1
```

Both wrappers create a new, private directory outside the repository, retain
stdout and stderr separately, hash an optional artifact, and record only an
allowlist of session variables. They deliberately write
`classification=unreviewed`. They never promote exit zero to `pass`, and warn
when a skip marker appears.

Do not put passwords, tokens or VM encryption keys in command arguments. Store
guest credentials in the host keychain or another machine-local credential
store, and keep VM disks, installers and evidence outside the repository.

## Required matrix fields

Each reviewed row records:

| Field | Meaning |
|---|---|
| OS and version | Guest edition, architecture and build or kernel |
| Desktop | GNOME/KDE/X11/Weston plus compositor and session type |
| Source SHA | Exact commit used to produce the tested binary |
| Artifact SHA-256 | Exact executable, package, image or video when applicable |
| Result | `pass`, `fail` or `skip` |
| Scope | The precise behavior observed, including exclusions |
| Evidence path | Machine-local retained logs and media |

Do not overwrite run directories. A rerun gets a new directory and its own
digests.

## Guest layout

Use official installation or evaluation media only:

- Windows 11 media from Microsoft's Windows download or Evaluation Center.
- Ubuntu Desktop or Server media from Ubuntu's official download service.

On Apple silicon, use ARM64 guests. An x86_64 binary under QEMU user emulation
can prove that exact binary boots, but the matrix must say `qemu-user`; it is not
a native x86_64 guest row.

Keep active VM bundles on a directly attached local filesystem with enough room
for the virtual disk, RAM-sized suspend state and snapshots. A network-backed
sparse image can stall during suspend and can grow host-side metadata or swap at
the worst time; do not use one as the default lab disk. Check free space before
start and suspend, and shut down the guest instead of suspending when the host
cannot safely absorb a RAM checkpoint.

## Linux desktop guest

One Ubuntu guest can cover GNOME, KDE and X11 by installing both desktop
sessions and selecting the required session at the display manager. Run the
repository dependency installer after checkout:

```bash
tools/ci-linux-deps.sh
sudo apt-get install openssh-server xdg-desktop-portal-gnome \
  xdg-desktop-portal-kde wireplumber pipewire
sudo systemctl enable --now ssh
```

Run portal tests from a terminal opened inside the interactively logged-in
desktop. Before every GNOME and KDE run, retain:

```bash
env | grep -E \
  '^(XDG_CURRENT_DESKTOP|XDG_SESSION_TYPE|WAYLAND_DISPLAY|XDG_RUNTIME_DIR|DBUS_SESSION_BUS_ADDRESS)='
busctl --user status org.freedesktop.portal.Desktop
```

The session must report `XDG_SESSION_TYPE=wayland`, a nonempty
`WAYLAND_DISPLAY`, a live user D-Bus, PipeWire/WirePlumber and the matching
portal backend. A tty, Xvfb, nested generic compositor or headless Weston run is
an honest skip for GNOME/KDE portal behavior. Keep isolated `XDG_STATE_HOME`
directories when testing persisted portal restore tokens.

Select an Xorg session at login for physical X11 behavior. Xvfb is useful for
protocol and media automation, but it must be labeled Xvfb and cannot prove
physical display, window-manager or GPU behavior.

## Windows guest

Use an interactive Windows 11 desktop with VMware Tools or the equivalent guest
integration installed. Install the MSVC Rust toolchain, Visual Studio C++ Build
Tools and a current Windows SDK. Enable OpenSSH once from an elevated
PowerShell so subsequent test execution and artifact transfer do not depend on
console typing:

```powershell
Add-WindowsCapability -Online -Name OpenSSH.Server~~~~0.0.1.0
Set-Service -Name sshd -StartupType Automatic
Start-Service sshd
if (-not (Get-NetFirewallRule -Name OpenSSH-Server-In-TCP -ErrorAction SilentlyContinue)) {
    New-NetFirewallRule -Name OpenSSH-Server-In-TCP `
      -DisplayName 'OpenSSH Server (sshd)' -Enabled True `
      -Direction Inbound -Protocol TCP -Action Allow -LocalPort 22
}
```

Use a dedicated lab key rather than putting a password in automation. Generate
it outside the repository on the host, copy only the public key into the guest,
and restrict the administrators key file to Administrators and SYSTEM:

```bash
install -d -m 700 "$HOME/.local/share/scrozz-platform-lab/credentials"
ssh-keygen -t ed25519 \
  -f "$HOME/.local/share/scrozz-platform-lab/credentials/windows_ed25519"
```

```powershell
$key = Get-Content "$HOME\Downloads\windows_ed25519.pub" -Raw
$path = "$env:ProgramData\ssh\administrators_authorized_keys"
if (-not (Test-Path $path)) {
    New-Item -ItemType File -Path $path -Force | Out-Null
}
if (-not (Select-String -Path $path -SimpleMatch $key.Trim() -Quiet)) {
    Add-Content -Path $path -Value $key.Trim() -Encoding ascii
}
icacls.exe $path /inheritance:r `
  /grant '*S-1-5-32-544:F' /grant 'SYSTEM:F'
Restart-Service sshd
```

Prefer SSH/SCP over simulated clipboard or bulk keyboard injection. Bind any
temporary artifact server only to the VM-only host interface, verify every
download against a retained SHA-256 file, and stop the server after the run.

An SSH process is not proof that a desktop-sensitive test ran in the logged-in
console session. Confirm the console is active with `quser`, then launch WGC,
clipboard, DPI, overlay and recording probes from a terminal in that session or
an `Interactive` Task Scheduler principal. Retain the task result and the probe's
own exit status; they are separate values. A typical non-secret principal is:

```powershell
$principal = New-ScheduledTaskPrincipal `
  -UserId "$env:USERDOMAIN\$env:USERNAME" `
  -LogonType Interactive `
  -RunLevel Highest
```

Windows.Graphics.Capture, D3D11, Media Foundation, WASAPI, DPI, focus,
click-through and drag behavior all require the interactive guest. A Windows
build on another host or a portable executable that only prints `--help` does
not cover them. Hardware H.264 is a skip when the VM exposes no hardware Media
Foundation encoder; software fallback is not equivalent evidence.

Record both the guest ISA and the artifact ISA. For example, an x86_64 portable
executable booting on Windows 11 ARM is a Windows runtime pass through x64
emulation, not an ARM64-native binary pass.

## Teardown

After each slice, verify that:

- the process exits without a leaked portal, PipeWire, COM or media worker;
- output files can be renamed or deleted immediately;
- repeated runs do not materially accumulate handles, threads or GPU memory;
- temporary HTTP/VNC exposure is disabled; and
- guest disks are cleanly shut down before moving or detaching storage.
