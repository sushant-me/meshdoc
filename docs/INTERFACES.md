# Frozen interfaces

Two contracts. Do not change them without telling the lead — three people are building
against them in parallel.

---

## A. Rust FFI (C ABI) — consumed by Dart `dart:ffi`

Crate: `ffi/` (package `meshdoc-ffi`, `crate-type = ["cdylib", "staticlib"]`).
Library name on Android: `libmeshdoc.so`.

**Stateless by design.** Every call opens the encrypted store, does one thing, closes.
That mirrors the CLI and means no handle lifetime bugs across the JNI/FFI boundary.

All functions return a `*mut c_char` holding a JSON object, or `NULL` on error.
The caller owns the string and must release it with `meshdoc_free`.
On error, `meshdoc_last_error()` returns the message for the **most recent failing call
on the current thread**.

```c
const char* meshdoc_version(void);
void        meshdoc_free(char* s);
const char* meshdoc_last_error(void);

/* Identity + workspace lifecycle. `store` is a directory, `pass` a passphrase. */
char* meshdoc_identity(const char* store, const char* pass);
/* -> {"fingerprint":"<32 hex>","public":"<128 hex>"} */

char* meshdoc_init(const char* store, const char* pass);
/* -> {"fingerprint":"...","public":"...","created":true|false} */

char* meshdoc_workspace_create(const char* store, const char* pass, const char* name);
/* -> {"workspace_id":"<32 hex>","name":"...","epoch":0} */

char* meshdoc_workspace_list(const char* store, const char* pass);
/* -> {"workspaces":[{"id":"...","name":"...","epoch":0,"docs":3,"members":2}]} */

char* meshdoc_workspace_invite(const char* store, const char* pass,
                               const char* ws, const char* joiner_public_hex);
/* -> {"token":"<base64>","bytes":4610} */

char* meshdoc_workspace_join(const char* store, const char* pass, const char* token);
/* -> {"workspace_id":"...","name":"...","members":2} */

/* State for the UI. `ws` may be "" to mean "the only workspace". */
char* meshdoc_state(const char* store, const char* pass, const char* ws);
/* -> {"workspace_id":"...","name":"...","epoch":0,"members":2,
       "documents":[{"id":"<32hex>","type":"checklist","title":"...","summary":"2/5 done"}],
       "changes":4,"pending":0} */

char* meshdoc_document(const char* store, const char* pass, const char* ws,
                       const char* doc_id);
/* -> {"id":"...","type":"checklist","title":"...",
       "items":[{"id":"<12hex>","text":"...","done":false}]}   (checklist)
    | {"id":"...","type":"note","title":"...","text":"..."}     (note)
    | {"id":"...","type":"geo_point",...,"lat":27.7,"lon":85.3,"label":"...","kind":"..."}
    | {"id":"...","type":"supply_form","fields":[{"id":"...","value":"...","unit":"..."}]} */

char* meshdoc_doc_create(const char* store, const char* pass, const char* ws,
                         const char* type, const char* title);
/* -> {"doc_id":"<32 hex>"} */

char* meshdoc_checklist_add(const char* store, const char* pass, const char* ws,
                            const char* doc_id, const char* text);
/* -> {"item_id":"<12 hex>"} */

char* meshdoc_checklist_toggle(const char* store, const char* pass, const char* ws,
                               const char* doc_id, const char* item_id, int32_t done);
/* -> {"ok":true} */

/* Sync. Rust opens the socket itself; Dart never touches a file descriptor. */
char* meshdoc_sync_pull(const char* store, const char* pass, const char* ws,
                        const char* host, uint16_t port);
/* -> {"received":2,"applied":2,"pages":1} */

char* meshdoc_sync_serve(const char* store, const char* pass, const char* ws,
                         uint16_t port, int32_t max_sessions);
/* -> {"served":1,"sent":2} */

/* Radio helpers the Dart layer uses to build the discovery tag. */
char* meshdoc_discovery_key(const char* store, const char* pass, const char* ws,
                            uint64_t unix_seconds);
/* -> {"uuid":"<32 hex>","bucket":123456,"payload":"<8 hex>"} */
```

Rules:
- `ws` = `""` means "the only workspace in the store"; more than one is an error.
- Every `char*` out is heap-allocated by Rust; `meshdoc_free` frees it.
- Never return a pointer to thread-local or static data.
- All inputs are NUL-terminated UTF-8. Never trust them; validate.

---

## B. Flutter ⇄ Android platform channel — implemented in Kotlin, called from Dart

Channel names are fixed:

- `MethodChannel('nl.meshdoc/radio')`
- `EventChannel('nl.meshdoc/radio/peers')` — discovery events

Discovery key: the Dart layer obtains `uuid` (32 hex = 16 bytes) and `payload` (8 hex)
from `meshdoc_discovery_key` and passes them down. Kotlin never derives keys.

### Methods

| Method | Arguments | Returns |
|---|---|---|
| `capabilities` | — | `{"ble":true,"bleAdvertise":true,"bleScan":true,"wifiDirect":true,"wifiDirectSupported":true}` |
| `startAdvertising` | `uuidHex` (32 hex), `payloadHex` (8 hex) | `null` on success |
| `stopAdvertising` | — | `null` |
| `startScanning` | `uuidHex` (32 hex) | `null` — results arrive on the EventChannel |
| `stopScanning` | — | `null` |
| `formGroup` | `peerFingerprint` (32 hex) | `{"role":"owner"\|"client","host":"192.168.49.1","available":true}` |
| `leaveGroup` | — | `null` |
| `status` | — | `{"advertising":bool,"scanning":bool,"group":null\|{"role":..,"host":..}}` |

### Events on `nl.meshdoc/radio/peers`

```json
{"fingerprint":"<32 hex>","rssi":-62,"kind":"ble","host":null}
```
`host` is filled in once a Wi-Fi Direct group exists, so the Dart layer knows where to
call `meshdoc_sync_pull`. `kind` is `"ble"` or `"wifi_direct"`.

### Kotlin implementation requirements

- Target API 35, min API 24.
- **BLE advertising**: `BluetoothLeAdvertiser`, `ADVERTISE_MODE_LOW_POWER`,
  `ADVERTISE_TX_POWER_LOW`, 128-bit service UUID = the rotating `uuidHex` provided,
  service data = the 8-byte `payloadHex`. Return an error if
  `isMultipleAdvertisementSupported` is false.
- **BLE scanning**: `BluetoothLeScanner`, `SCAN_MODE_LOW_POWER`, filter on that service
  UUID, report `device.address` + `rssi` on the EventChannel. Deduplicate per address
  within a 5 s window so the UI is not flooded.
- **Wi-Fi Direct**: `WifiP2pManager` — `discoverPeers`, `connect` with
  `WifiP2pConfig(groupOwnerIntent = …)` decided by the **fingerprint rule**: the peer
  with the lexicographically smaller fingerprint requests `groupOwnerIntent = 15`
  (owner), the other uses `0` (client). `requestGroupInfo` supplies the owner address.
  Default owner address is `192.168.49.1` when `groupOwnerAddress` is unavailable.
- **Permissions**: `BLUETOOTH_ADVERTISE`, `BLUETOOTH_SCAN`, `BLUETOOTH_CONNECT`
  (API 31+), `ACCESS_FINE_LOCATION`, `NEARBY_WIFI_DEVICES` (API 33+),
  `INTERNET`, `CHANGE_WIFI_STATE`, `ACCESS_WIFI_STATE`.
- **Graceful degradation**: on an emulator, Wi-Fi Direct and BLE advertising are
  typically unsupported. Detect that and return
  `{"ble":false,...,"wifiDirectSupported":false}` from `capabilities` instead of
  throwing. The Dart layer shows a clear banner.
- Never crash on missing hardware or denied permission — return an error string.

### Dart side rules

- Own `app/lib/**` and `app/pubspec.yaml` only.
- Load `libmeshdoc.so` with `DynamicLibrary.open('libmeshdoc.so')`; fall back to
  `DynamicLibrary.process()` on desktop if needed.
- All FFI strings freed with `meshdocFree`.
- Every FFI call that returns NULL must surface `meshdocLastError()` to the user.
