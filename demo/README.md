# Demo

`two-device-sync.mp4` (42 s) — a real screen capture of `scripts/two-device-demo.sh`.

What it shows: two **independent processes**, each with its own passphrase-encrypted
store, joining one workspace by invite token and syncing over a real TCP socket.
The field phone records a checklist and hands it to HQ; both then edit while
disconnected; on reconnect both reconcile and the run ends with
`CONVERGED — both devices hold byte-identical state`.

This is two clients, not two phones. The same binaries run on two Android devices
under Termux; the radio transports (BLE discovery, Wi-Fi Direct) are not implemented
yet, so the link here is TCP over loopback/LAN.

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
