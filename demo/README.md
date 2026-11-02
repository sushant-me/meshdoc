# Demo

Two recordings, both real runs, both ending in a convergence check.

| File | Length | What it is |
|---|---|---|
| `two-device-sync.mp4` | 42 s | two independent processes with separate encrypted stores syncing over TCP |
| `android-sync.mp4` | 37 s | the CLI built for Android, installed on an Android 15 device, syncing with a Linux host in both directions |

### `two-device-sync.mp4`

What it shows: two **independent processes**, each with its own passphrase-encrypted
store, joining one workspace by invite token and syncing over a real TCP socket.
The field phone records a checklist and hands it to HQ; both then edit while
disconnected; on reconnect both reconcile and the run ends with
`CONVERGED — both devices hold byte-identical state`.

This is two clients, not two phones. The same binaries run on two Android devices
under Termux; the radio transports (BLE discovery, Wi-Fi Direct) are not implemented
yet, so the link here is TCP over loopback/LAN.

### `android-sync.mp4`

Captured from `scripts/android-sync-demo.sh`: cross-compiles for the device ABI, pushes
the binary, installs it, has Android create the workspace and invite the host, syncs
Android → host, then host → Android, and diffs the two `render` outputs.

Regenerate:

```bash
cargo build --release --bin meshdoc
./scripts/make-demo-video.sh          # needs ImageMagick + ffmpeg
```

Run the demo live instead of watching it:

```bash
./scripts/two-device-demo.sh
SYNC_HOST=192.168.1.42 ./scripts/two-device-demo.sh   # two machines on one Wi-Fi
```
