package nl.meshdoc.meshdoc_app

import android.Manifest
import android.annotation.SuppressLint
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.PackageManager
import android.net.wifi.p2p.WifiP2pConfig
import android.net.wifi.p2p.WifiP2pDevice
import android.net.wifi.p2p.WifiP2pManager
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.os.Looper.getMainLooper

/**
 * Wi-Fi Direct group formation.
 *
 * Role election is NOT left to Android. Both peers decide from the same two
 * fingerprints: the lexicographically smaller one requests groupOwnerIntent 15 and
 * becomes the group owner; the other requests 0. That guarantees the two sides ask
 * for opposite roles, so negotiation cannot stall on equal intents.
 */
class WifiDirectRadio(
    private val context: Context,
    private val onEvent: (Map<String, Any?>) -> Unit,
) {
    private val manager: WifiP2pManager? =
        context.getSystemService(Context.WIFI_P2P_SERVICE) as? WifiP2pManager
    private var channel: WifiP2pManager.Channel? = null
    private var receiver: BroadcastReceiver? = null
    private val handler = Handler(getMainLooper())

    private var peers: List<WifiP2pDevice> = emptyList()
    private var group: Map<String, Any?>? = null
    private var lastRole: String = "client"

    init {
        if (manager != null) {
            channel = manager.initialize(context, getMainLooper()) { }
            registerReceiver()
        }
    }

    fun supported(): Boolean {
        val feature = context.packageManager.hasSystemFeature(PackageManager.FEATURE_WIFI_DIRECT)
        return manager != null && channel != null && feature
    }

    fun capabilities(): Map<String, Any?> = mapOf(
        "wifiDirect" to supported(),
        "wifiDirectSupported" to supported(),
        "reason" to if (supported()) null else "this device has no Wi-Fi Direct support",
    )

    /**
     * Discover and connect. [myFingerprint] and [peerFingerprint] decide the role.
     * If [preferredAddress] is given it is used, otherwise the first discovered peer.
     */
    @SuppressLint("MissingPermission")
    fun formGroup(
        myFingerprint: String,
        peerFingerprint: String,
        preferredAddress: String?,
        onResult: (Map<String, Any?>?, String?) -> Unit,
    ) {
        val m = manager ?: return onResult(null, "Wi-Fi Direct is not available")
        val ch = channel ?: return onResult(null, "Wi-Fi Direct channel not initialised")
        if (!hasNearbyPermission()) {
            return onResult(null, "permission required: ${nearbyPermission()}")
        }

        // Deterministic role: smaller fingerprint wins ownership.
        val weOwn = myFingerprint < peerFingerprint
        lastRole = if (weOwn) "owner" else "client"

        if (group != null) {
            // Already in a group: report it rather than negotiating again.
            return onResult(group, null)
        }

        m.discoverPeers(ch, object : WifiP2pManager.ActionListener {
            override fun onSuccess() {
                handler.postDelayed({
                    val target = peers.firstOrNull { preferredAddress == null || it.deviceAddress == preferredAddress }
                        ?: peers.firstOrNull()
                    if (target == null) {
                        onResult(null, "no Wi-Fi Direct peer found nearby")
                        return@postDelayed
                    }
                    connectTo(m, ch, target, weOwn, onResult)
                }, 2000)
            }

            override fun onFailure(reason: Int) {
                onResult(null, "peer discovery failed (code $reason); is Wi-Fi on?")
            }
        })
    }

    @SuppressLint("MissingPermission")
    private fun connectTo(
        m: WifiP2pManager,
        ch: WifiP2pManager.Channel,
        device: WifiP2pDevice,
        weOwn: Boolean,
        onResult: (Map<String, Any?>?, String?) -> Unit,
    ) {
        val config = WifiP2pConfig().apply {
            deviceAddress = device.deviceAddress
            groupOwnerIntent = if (weOwn) 15 else 0
        }
        m.connect(ch, config, object : WifiP2pManager.ActionListener {
            override fun onSuccess() {
                // The group-owner address arrives via requestGroupInfo below.
                handler.postDelayed({
                    m.requestGroupInfo(ch) { info ->
                        if (info == null) {
                            onResult(null, "group formed but no group info was available")
                            return@requestGroupInfo
                        }
                        val host = if (weOwn) "192.168.49.1" else (info.groupOwnerAddress?.hostAddress ?: "192.168.49.1")
                        group = mapOf(
                            "role" to lastRole,
                            "host" to host,
                            "available" to true,
                            "isGroupOwner" to info.isGroupOwner,
                        )
                        onResult(group, null)
                    }
                }, 3000)
            }

            override fun onFailure(reason: Int) {
                onResult(null, "group formation failed (code $reason)")
            }
        })
    }

    @SuppressLint("MissingPermission")
    fun leaveGroup(onDone: (String?) -> Unit) {
        val m = manager ?: return onDone("Wi-Fi Direct is not available")
        val ch = channel ?: return onDone("Wi-Fi Direct channel not initialised")
        m.removeGroup(ch, object : WifiP2pManager.ActionListener {
            override fun onSuccess() {
                group = null
                onDone(null)
            }

            override fun onFailure(reason: Int) {
                onDone("could not leave the group (code $reason)")
            }
        })
    }

    fun status(): Map<String, Any?> = mapOf(
        "supported" to supported(),
        "group" to group,
        "peerCount" to peers.size,
    )

    @SuppressLint("MissingPermission")
    private fun registerReceiver() {
        if (receiver != null) return
        val r = object : BroadcastReceiver() {
            override fun onReceive(ctx: Context, intent: Intent) {
                when (intent.action) {
                    WifiP2pManager.WIFI_P2P_PEERS_CHANGED_ACTION -> {
                        @Suppress("DEPRECATION")
                        val list = intent.getParcelableExtra<android.net.wifi.p2p.WifiP2pDeviceList>(WifiP2pManager.EXTRA_P2P_DEVICE_LIST)
                        if (list != null) {
                            peers = list.deviceList.toList()
                        } else {
                            manager?.requestPeers(channel) { l -> peers = l.deviceList.toList() }
                        }
                    }
                    WifiP2pManager.WIFI_P2P_CONNECTION_CHANGED_ACTION -> {
                        manager?.requestGroupInfo(channel) { info ->
                            group = if (info == null) null else mapOf(
                                "role" to if (info.isGroupOwner) "owner" else "client",
                                "host" to (info.groupOwnerAddress?.hostAddress ?: "192.168.49.1"),
                                "available" to true,
                                "isGroupOwner" to info.isGroupOwner,
                            )
                            onEvent(mapOf("kind" to "wifi_direct", "group" to group))
                        }
                    }
                }
            }
        }
        val filter = IntentFilter().apply {
            addAction(WifiP2pManager.WIFI_P2P_STATE_CHANGED_ACTION)
            addAction(WifiP2pManager.WIFI_P2P_PEERS_CHANGED_ACTION)
            addAction(WifiP2pManager.WIFI_P2P_CONNECTION_CHANGED_ACTION)
            addAction(WifiP2pManager.WIFI_P2P_THIS_DEVICE_CHANGED_ACTION)
        }
        context.registerReceiver(r, filter)
        receiver = r
    }

    fun close() {
        receiver?.let { runCatching { context.unregisterReceiver(it) } }
        receiver = null
    }

    private fun hasNearbyPermission(): Boolean {
        val perm = nearbyPermission()
        return context.checkSelfPermission(perm) == PackageManager.PERMISSION_GRANTED
    }

    private fun nearbyPermission(): String =
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) Manifest.permission.NEARBY_WIFI_DEVICES
        else Manifest.permission.ACCESS_FINE_LOCATION
}
