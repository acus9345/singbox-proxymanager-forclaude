package io.github.acus9345.singboxwatch

import android.content.Context
import android.net.ConnectivityManager
import android.net.NetworkCapabilities
import android.os.Handler
import android.os.Looper
import java.net.HttpURLConnection
import java.net.URL

enum class Level { OK, LEAK, VPN_DOWN, OFFLINE, NO_PIN, CHECKING, PAUSED }

data class Status(
    val level: Level,
    val vpn: Boolean,
    val exitIp: String?,
    val pinned: String,
    val checkedAt: Long,
) {
    val title: String
        get() = when (level) {
            Level.OK -> "Tunnel OK"
            Level.LEAK -> "LEAKING — wrong exit IP"
            Level.VPN_DOWN -> "sing-box VPN is down"
            Level.OFFLINE -> "No answer — proxy busy?"
            Level.NO_PIN -> "Pin your exit IP"
            Level.CHECKING -> "Checking…"
            Level.PAUSED -> "Not watching"
        }

    val detail: String
        get() = when (level) {
            Level.OK -> "Exit $exitIp matches your pinned IP"
            Level.LEAK -> "Exit $exitIp, expected $pinned — traffic is not using the tunnel"
            Level.VPN_DOWN -> "No VPN is active" + (exitIp?.let { " · raw exit $it" } ?: "")
            Level.OFFLINE -> "VPN is up but the exit-IP check got no reply"
            Level.NO_PIN -> "With sing-box on, tap \"Pin current IP\"" + (exitIp?.let { " (now $it)" } ?: "")
            Level.CHECKING -> "Asking the exit-IP services"
            Level.PAUSED -> "Turn on \"Watch the tunnel\" below"
        }

    val bad: Boolean get() = level == Level.LEAK || level == Level.VPN_DOWN
}

/** Latest status, shared by the service and the screen (same process). */
object StatusBus {
    private val main = Handler(Looper.getMainLooper())
    private val listeners = mutableSetOf<(Status) -> Unit>()

    @Volatile
    var current: Status = Status(Level.PAUSED, false, null, "", 0)
        private set

    fun publish(s: Status) = main.post {
        current = s
        listeners.toList().forEach { it(s) }
    }

    fun listen(l: (Status) -> Unit) = main.post { listeners += l; l(current) }
    fun unlisten(l: (Status) -> Unit) = main.post { listeners -= l }
}

object Checker {
    /** True when Android's current default network is a VPN (sing-box's TUN). */
    fun vpnActive(ctx: Context): Boolean {
        val cm = ctx.getSystemService(ConnectivityManager::class.java)
        val caps = cm.getNetworkCapabilities(cm.activeNetwork) ?: return false
        return caps.hasTransport(NetworkCapabilities.TRANSPORT_VPN)
    }

    private fun validIpv4(s: String): Boolean {
        val p = s.split('.')
        return p.size == 4 && p.all { it.isNotEmpty() && it.length <= 3 && it.all(Char::isDigit) && it.toInt() <= 255 }
    }

    // Same endpoints and order as the PC tray: plain HTTP works identically
    // inside and outside the tunnel, and several endpoints mean one refused
    // request (proxies cap concurrent connections) doesn't read as "offline".
    fun fetchExitIp(): String? {
        for (u in listOf(
            "http://ip-api.com/line?fields=query",
            "http://ifconfig.me/ip",
            "http://checkip.amazonaws.com/",
        )) {
            try {
                val c = URL(u).openConnection() as HttpURLConnection
                c.connectTimeout = 6000
                c.readTimeout = 6000
                c.useCaches = false
                c.setRequestProperty("User-Agent", "singbox-watch/1.0")
                try {
                    if (c.responseCode == 200) {
                        val ip = c.inputStream.bufferedReader().use { it.readText() }.trim()
                        if (validIpv4(ip)) return ip
                    }
                } finally {
                    c.disconnect()
                }
            } catch (_: Exception) {
            }
        }
        return null
    }

    /** Full check: VPN present + exit IP vs the pinned IP. Blocking. */
    fun check(ctx: Context): Status {
        val pinned = Prefs(ctx).pinnedIp
        val vpn = vpnActive(ctx)
        val ip = fetchExitIp()
        val level = when {
            !vpn -> Level.VPN_DOWN
            pinned.isEmpty() -> Level.NO_PIN
            ip == null -> Level.OFFLINE
            ip != pinned -> Level.LEAK
            else -> Level.OK
        }
        return Status(level, vpn, ip, pinned, System.currentTimeMillis())
    }
}
