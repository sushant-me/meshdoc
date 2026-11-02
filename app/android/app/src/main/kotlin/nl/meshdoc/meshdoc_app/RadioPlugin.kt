package nl.meshdoc.meshdoc_app

import android.Manifest
import android.os.Build
import android.os.Handler
import android.os.Looper
import io.flutter.embedding.android.FlutterActivity
import io.flutter.plugin.common.EventChannel
import io.flutter.plugin.common.MethodCall
import io.flutter.plugin.common.MethodChannel

/**
 * The `nl.meshdoc/radio` platform channel.
 *
 * Contract is frozen in docs/INTERFACES.md section B. All errors are returned as
 * `result.error("radio", message, null)`, never as exceptions, and every method
 * works (or reports a clear reason) on hardware that lacks BLE or Wi-Fi Direct,
 * including emulators.
 */
class RadioPlugin(private val activity: FlutterActivity) :
    MethodChannel.MethodCallHandler, EventChannel.StreamHandler {

    companion object {
        const val METHOD_CHANNEL = "nl.meshdoc/radio"
        const val EVENT_CHANNEL = "nl.meshdoc/radio/peers"
        private const val PERMISSION_REQUEST = 4711
    }

    private val main = Handler(Looper.getMainLooper())
    private var sink: EventChannel.EventSink? = null

    private val ble by lazy { BleRadio(activity.applicationContext) { peer -> emit(peer) } }
    private val wifi by lazy { WifiDirectRadio(activity.applicationContext) { event -> emit(event) } }

    fun register(engine: io.flutter.embedding.engine.FlutterEngine) {
        MethodChannel(engine.dartExecutor.binaryMessenger, METHOD_CHANNEL)
            .setMethodCallHandler(this)
        EventChannel(engine.dartExecutor.binaryMessenger, EVENT_CHANNEL)
            .setStreamHandler(this)
    }

    private fun emit(event: Map<String, Any?>) {
        main.post { sink?.success(event) }
    }

    override fun onListen(arguments: Any?, events: EventChannel.EventSink?) {
        sink = events
    }

    override fun onCancel(arguments: Any?) {
        sink = null
    }

    override fun onMethodCall(call: MethodCall, result: MethodChannel.Result) {
        try {
            when (call.method) {
                "capabilities" -> {
                    val caps = HashMap<String, Any?>()
                    caps.putAll(ble.capabilities())
                    val w = wifi.capabilities()
                    caps["wifiDirect"] = w["wifiDirect"]
                    caps["wifiDirectSupported"] = w["wifiDirectSupported"]
                    caps["wifiDirectReason"] = w["reason"]
                    result.success(caps)
                }

                "startAdvertising" -> {
                    requestNearbyPermissionsIfNeeded()
                    val uuid = call.argument<String>("uuidHex")
                    val payload = call.argument<String>("payloadHex")
                    if (uuid == null || payload == null) {
                        return fail(result, "startAdvertising needs uuidHex and payloadHex")
                    }
                    val err = ble.startAdvertising(uuid, payload)
                    if (err == null) result.success(null) else fail(result, err)
                }

                "stopAdvertising" -> {
                    ble.stopAdvertising()
                    result.success(null)
                }

                "startScanning" -> {
                    requestNearbyPermissionsIfNeeded()
                    val uuid = call.argument<String>("uuidHex")
                    if (uuid == null) return fail(result, "startScanning needs uuidHex")
                    val err = ble.startScanning(uuid)
                    if (err == null) result.success(null) else fail(result, err)
                }

                "stopScanning" -> {
                    ble.stopScanning()
                    result.success(null)
                }

                "formGroup" -> {
                    val mine = call.argument<String>("myFingerprint") ?: ""
                    val peer = call.argument<String>("peerFingerprint")
                        ?: return fail(result, "formGroup needs peerFingerprint")
                    val address = call.argument<String>("deviceAddress")
                    requestNearbyPermissionsIfNeeded()
                    wifi.formGroup(mine, peer, address) { ok, err ->
                        main.post {
                            if (ok != null) result.success(ok)
                            else fail(result, err ?: "group formation failed")
                        }
                    }
                }

                "leaveGroup" -> {
                    wifi.leaveGroup { err ->
                        main.post { if (err == null) result.success(null) else fail(result, err) }
                    }
                }

                "status" -> result.success(
                    mapOf(
                        "advertising" to ble.isAdvertising(),
                        "scanning" to ble.isScanning(),
                        "group" to wifi.status()["group"],
                    )
                )

                else -> result.notImplemented()
            }
        } catch (t: Throwable) {
            fail(result, t.message ?: t.javaClass.simpleName)
        }
    }

    private fun fail(result: MethodChannel.Result, message: String) {
        result.error("radio", message, null)
    }

    /** Ask for the runtime permissions the radios need; the caller retries after granting. */
    private fun requestNearbyPermissionsIfNeeded() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.M) return
        val needed = ArrayList<String>()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            if (!granted(Manifest.permission.BLUETOOTH_SCAN)) needed.add(Manifest.permission.BLUETOOTH_SCAN)
            if (!granted(Manifest.permission.BLUETOOTH_ADVERTISE)) needed.add(Manifest.permission.BLUETOOTH_ADVERTISE)
            if (!granted(Manifest.permission.BLUETOOTH_CONNECT)) needed.add(Manifest.permission.BLUETOOTH_CONNECT)
        } else {
            if (!granted(Manifest.permission.ACCESS_FINE_LOCATION)) needed.add(Manifest.permission.ACCESS_FINE_LOCATION)
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            if (!granted(Manifest.permission.NEARBY_WIFI_DEVICES)) needed.add(Manifest.permission.NEARBY_WIFI_DEVICES)
        }
        if (needed.isNotEmpty()) {
            activity.requestPermissions(needed.toTypedArray(), PERMISSION_REQUEST)
        }
    }

    private fun granted(permission: String): Boolean =
        activity.checkSelfPermission(permission) == android.content.pm.PackageManager.PERMISSION_GRANTED

    fun dispose() {
        ble.stopAdvertising()
        ble.stopScanning()
        wifi.close()
        sink = null
    }
}
