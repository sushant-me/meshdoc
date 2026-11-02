package nl.meshdoc.meshdoc_app

import io.flutter.embedding.android.FlutterActivity
import io.flutter.embedding.engine.FlutterEngine

class MainActivity : FlutterActivity() {
    private var radio: RadioPlugin? = null

    override fun configureFlutterEngine(flutterEngine: FlutterEngine) {
        super.configureFlutterEngine(flutterEngine)
        // The MeshDoc radio channel: BLE discovery + Wi-Fi Direct group formation.
        radio = RadioPlugin(this).also { it.register(flutterEngine) }
    }

    override fun onDestroy() {
        radio?.dispose()
        radio = null
        super.onDestroy()
    }
}
