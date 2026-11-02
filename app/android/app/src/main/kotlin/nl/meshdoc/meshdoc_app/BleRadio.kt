package nl.meshdoc.meshdoc_app

import android.Manifest
import android.annotation.SuppressLint
import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothManager
import android.bluetooth.le.AdvertiseCallback
import android.bluetooth.le.AdvertiseData
import android.bluetooth.le.AdvertiseSettings
import android.bluetooth.le.ScanCallback
import android.bluetooth.le.ScanFilter
import android.bluetooth.le.ScanResult
import android.bluetooth.le.ScanSettings
import android.content.Context
import android.content.pm.PackageManager
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.os.ParcelUuid
import java.security.MessageDigest
import java.util.UUID

/**
 * BLE advertising and scanning for MeshDoc discovery.
 *
 * The 128-bit service UUID is supplied by the Dart layer, which derives it from the
 * workspace epoch key and a 15-minute time bucket. This class never derives keys; it
 * only puts the given UUID on the air and filters scans on it.
 *
 * Everything degrades gracefully: unsupported hardware or a denied permission yields
 * an error string, never a crash.
 */
class BleRadio(
    private val context: Context,
    private val onPeer: (Map<String, Any?>) -> Unit,
) {
    private val manager = context.getSystemService(Context.BLUETOOTH_SERVICE) as? BluetoothManager
    private val adapter: BluetoothAdapter? get() = manager?.adapter

    private var advertiser: android.bluetooth.le.BluetoothLeAdvertiser? = null
    private var advertiseCallback: AdvertiseCallback? = null
    private var scanner: android.bluetooth.le.BluetoothLeScanner? = null
    private var scanning = false
    private var advertising = false
    private val lastSeen = HashMap<String, Long>()
    private val handler = Handler(Looper.getMainLooper())

    fun hasPermission(permission: String): Boolean =
        context.checkSelfPermission(permission) == PackageManager.PERMISSION_GRANTED

    private fun advertisePermission(): String =
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) Manifest.permission.BLUETOOTH_ADVERTISE
        else Manifest.permission.ACCESS_FINE_LOCATION

    private fun scanPermission(): String =
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) Manifest.permission.BLUETOOTH_SCAN
        else Manifest.permission.ACCESS_FINE_LOCATION

    fun capabilities(): Map<String, Any?> {
        val hasLe = context.packageManager.hasSystemFeature(PackageManager.FEATURE_BLUETOOTH_LE)
        val btOn = adapter?.isEnabled == true
        val advSupported = adapter?.isMultipleAdvertisementSupported == true
        return mapOf(
            "ble" to (hasLe && adapter != null),
            "bleAdvertise" to (hasLe && advSupported && btOn),
            "bleScan" to (hasLe && btOn),
            "advertising" to advertising,
            "scanning" to scanning,
            "reason" to when {
                adapter == null -> "no Bluetooth adapter on this device"
                !btOn -> "Bluetooth is off"
                !hasLe -> "device has no BLE support"
                !advSupported -> "device cannot advertise (common on emulators)"
                else -> null
            },
        )
    }

    @SuppressLint("MissingPermission")
    fun startAdvertising(uuidHex: String, payloadHex: String): String? {
        val uuid = parseUuid(uuidHex) ?: return "malformed discovery uuid: $uuidHex"
        val payload = parseHex(payloadHex) ?: return "malformed discovery payload: $payloadHex"
        val a = adapter ?: return "no Bluetooth adapter"
        if (!a.isEnabled) return "Bluetooth is off"
        if (!a.isMultipleAdvertisementSupported) return "this device cannot advertise over BLE"
        if (!hasPermission(advertisePermission())) return "permission required: ${advertisePermission()}"

        stopAdvertising()
        val adv = a.bluetoothLeAdvertiser ?: return "no BLE advertiser available"
        advertiser = adv

        val settings = AdvertiseSettings.Builder()
            .setAdvertiseMode(AdvertiseSettings.ADVERTISE_MODE_LOW_POWER)
            .setTxPowerLevel(AdvertiseSettings.ADVERTISE_TX_POWER_LOW)
            .setConnectable(false)
            .setTimeout(0)
            .build()

        val data = AdvertiseData.Builder()
            .setIncludeDeviceName(false)
            .addServiceUuid(ParcelUuid(uuid))
            .addServiceData(ParcelUuid(uuid), payload)
            .build()

        val cb = object : AdvertiseCallback() {
            override fun onStartSuccess(settingsInEffect: AdvertiseSettings) {
                advertising = true
            }

            override fun onStartFailure(errorCode: Int) {
                advertising = false
            }
        }
        advertiseCallback = cb
        adv.startAdvertising(settings, data, cb)
        advertising = true
        return null
    }

    @SuppressLint("MissingPermission")
    fun stopAdvertising() {
        val a = advertiser ?: return
        val cb = advertiseCallback
        try {
            if (cb != null) a.stopAdvertising(cb)
        } catch (_: Exception) {
        }
        advertiser = null
        advertiseCallback = null
        advertising = false
    }

    @SuppressLint("MissingPermission")
    fun startScanning(uuidHex: String): String? {
        val uuid = parseUuid(uuidHex) ?: return "malformed discovery uuid: $uuidHex"
        val a = adapter ?: return "no Bluetooth adapter"
        if (!a.isEnabled) return "Bluetooth is off"
        if (!hasPermission(scanPermission())) return "permission required: ${scanPermission()}"

        stopScanning()
        val leScanner = a.bluetoothLeScanner ?: return "no BLE scanner available"
        scanner = leScanner

        val filter = ScanFilter.Builder().setServiceUuid(ParcelUuid(uuid)).build()
        val settings = ScanSettings.Builder()
            .setScanMode(ScanSettings.SCAN_MODE_LOW_POWER)
            .setCallbackType(ScanSettings.CALLBACK_TYPE_ALL_MATCHES)
            .build()

        val cb = object : ScanCallback() {
            override fun onScanResult(callbackType: Int, result: ScanResult) {
                val address = result.device?.address ?: return
                val now = System.currentTimeMillis()
                // Deduplicate per address within 5 s so the UI is not flooded.
                val previous = lastSeen[address]
                if (previous != null && now - previous < 5_000) return
                lastSeen[address] = now

                val payload = result.scanRecord?.getServiceData(ParcelUuid(uuid))
                val fingerprint = payload?.let { fingerprintOf(it) } ?: ""
                onPeer(
                    mapOf(
                        "fingerprint" to fingerprint,
                        "rssi" to result.rssi,
                        "kind" to "ble",
                        "host" to null,
                        "address" to address,
                    )
                )
            }

            override fun onScanFailed(errorCode: Int) {
                scanning = false
            }
        }
        scanCallback = cb
        leScanner.startScan(listOf(filter), settings, cb)
        scanning = true
        return null
    }

    @SuppressLint("MissingPermission")
    fun stopScanning() {
        val s = scanner ?: return
        val cb = scanCallback
        try {
            if (cb != null) s.stopScan(cb)
        } catch (_: Exception) {
        }
        scanner = null
        scanCallback = null
        scanning = false
    }

    fun isScanning(): Boolean = scanning
    fun isAdvertising(): Boolean = advertising

    private var scanCallback: ScanCallback? = null

    /** A stable, non-secret handle for an advertisement payload. */
    private fun fingerprintOf(payload: ByteArray): String {
        val digest = MessageDigest.getInstance("SHA-256").digest(payload)
        return digest.take(16).joinToString("") { "%02x".format(it) }
    }

    private fun parseUuid(hex: String): UUID? = try {
        val raw = parseHex(hex) ?: return null
        if (raw.size != 16) return null
        var msb = 0L
        var lsb = 0L
        for (i in 0 until 8) msb = (msb shl 8) or (raw[i].toLong() and 0xff)
        for (i in 8 until 16) lsb = (lsb shl 8) or (raw[i].toLong() and 0xff)
        UUID(msb, lsb)
    } catch (_: Exception) {
        null
    }

    private fun parseHex(hex: String): ByteArray? {
        if (hex.length % 2 != 0) return null
        val out = ByteArray(hex.length / 2)
        for (i in out.indices) {
            val hi = Character.digit(hex[i * 2], 16)
            val lo = Character.digit(hex[i * 2 + 1], 16)
            if (hi < 0 || lo < 0) return null
            out[i] = ((hi shl 4) or lo).toByte()
        }
        return out
    }
}
