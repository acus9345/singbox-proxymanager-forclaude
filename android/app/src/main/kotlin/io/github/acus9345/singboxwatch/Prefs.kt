package io.github.acus9345.singboxwatch

import android.content.Context
import android.content.SharedPreferences

/** Feature switches and pinned values, mirroring the PC tray's settings.txt. */
class Prefs(ctx: Context) {
    private val sp: SharedPreferences =
        ctx.applicationContext.getSharedPreferences("singbox-watch", Context.MODE_PRIVATE)

    /** (key, default, label) for every on/off switch shown in the app */
    data class Switch(val key: String, val default: Boolean, val label: String, val hint: String)

    companion object {
        const val WATCHING = "watching"
        const val ALERT_VPN_DOWN = "alert_vpn_down"
        const val ALERT_LEAK = "alert_leak"
        const val ALERT_OFFLINE = "alert_offline"
        const val VIBRATE = "vibrate"
        const val FAST_RECHECK = "fast_recheck"
        const val START_ON_BOOT = "start_on_boot"

        val SWITCHES = listOf(
            Switch(WATCHING, true, "Watch the tunnel",
                "Runs in the background with a status notification"),
            Switch(ALERT_VPN_DOWN, true, "Alert when sing-box's VPN drops",
                "Instant: Android reports the VPN going away, no probe needed"),
            Switch(ALERT_LEAK, true, "Alert on a wrong exit IP",
                "Traffic leaves from an IP other than your pinned one"),
            Switch(ALERT_OFFLINE, false, "Alert when the check gets no answer",
                "Often just a busy proxy or no signal, so off by default"),
            Switch(VIBRATE, true, "Vibrate on alerts", ""),
            Switch(FAST_RECHECK, true, "Re-check every 15 s while not OK",
                "Shows recovery quickly; uses a little more data"),
            Switch(START_ON_BOOT, true, "Start after the phone reboots", ""),
        )

        val INTERVALS = listOf(30, 60, 120, 300)
    }

    fun get(key: String): Boolean =
        sp.getBoolean(key, SWITCHES.firstOrNull { it.key == key }?.default ?: false)

    fun set(key: String, on: Boolean) = sp.edit().putBoolean(key, on).apply()

    var pinnedIp: String
        get() = sp.getString("pinned_ip", "") ?: ""
        set(v) = sp.edit().putString("pinned_ip", v.trim()).apply()

    /** seconds between exit-IP checks while everything is OK */
    var intervalSecs: Int
        get() = sp.getInt("interval_secs", 60)
        set(v) = sp.edit().putInt("interval_secs", v).apply()

    /** optional proxy renewal deadline, YYYY-MM-DD */
    var renewDeadline: String
        get() = sp.getString("renew_deadline", "") ?: ""
        set(v) = sp.edit().putString("renew_deadline", v.trim()).apply()

    /** "renew in 12d" / "RENEW NOW (3d left)" / "" when no deadline is set */
    fun renewText(): String {
        val d = runCatching { java.time.LocalDate.parse(renewDeadline) }.getOrNull() ?: return ""
        val days = java.time.temporal.ChronoUnit.DAYS.between(java.time.LocalDate.now(), d)
        return when {
            days < 0 -> "IP EXPIRED — renew your proxy"
            days <= 5 -> "RENEW NOW (${days}d left)"
            else -> "renew in ${days}d"
        }
    }
}
