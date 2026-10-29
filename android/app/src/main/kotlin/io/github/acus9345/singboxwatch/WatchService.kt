package io.github.acus9345.singboxwatch

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.os.VibrationEffect
import android.os.Vibrator
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicBoolean

/**
 * Background watchdog for the phone's sing-box tunnel — the phone's version
 * of the PC tray. Two paths, like the tray:
 *  - instant: a network callback flips to "VPN down" the moment Android
 *    reports the VPN gone (no probe, the way the tray waits on sing-box.exe);
 *  - periodic: the exit IP is checked against the pinned IP every interval.
 */
class WatchService : Service() {

    companion object {
        const val ACTION_CHECK = "check"
        private const val CH_STATUS = "status"
        private const val CH_ALERT = "alert"
        private const val ID_STATUS = 1
        private const val ID_ALERT = 2
        private const val FAST_RECHECK_MS = 15_000L
        const val SINGBOX_PACKAGE = "io.nekohasekai.sfa"

        fun start(ctx: Context, action: String? = null) {
            val i = Intent(ctx, WatchService::class.java).setAction(action)
            try {
                ctx.startForegroundService(i)
            } catch (_: Exception) {
                // background-start restrictions: the next app open restarts it
            }
        }

        fun stop(ctx: Context) {
            ctx.stopService(Intent(ctx, WatchService::class.java))
            StatusBus.publish(Status(Level.PAUSED, Checker.vpnActive(ctx), null, Prefs(ctx).pinnedIp, 0))
        }

        fun openSingBox(ctx: Context): Boolean {
            val i = ctx.packageManager.getLaunchIntentForPackage(SINGBOX_PACKAGE) ?: return false
            ctx.startActivity(i.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
            return true
        }
    }

    private val main = Handler(Looper.getMainLooper())
    private val worker = Executors.newSingleThreadExecutor()
    private val checking = AtomicBoolean(false)
    private lateinit var prefs: Prefs
    private lateinit var nm: NotificationManager
    private var lastLevel: Level? = null
    private val tick = Runnable { runCheck() }
    private val settled = Runnable { runCheck() }

    // capability updates also arrive for signal-strength changes: react only
    // when the default network itself or its VPN-ness changes
    @Volatile private var seenNet: Network? = null
    @Volatile private var seenVpn: Boolean? = null

    private val netCallback = object : ConnectivityManager.NetworkCallback() {
        override fun onLost(network: Network) {
            seenNet = null
            onNetworkChange()
        }

        override fun onCapabilitiesChanged(network: Network, caps: NetworkCapabilities) {
            val vpn = caps.hasTransport(NetworkCapabilities.TRANSPORT_VPN)
            if (network == seenNet && vpn == seenVpn) return
            seenNet = network
            seenVpn = vpn
            onNetworkChange()
        }
    }

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onCreate() {
        super.onCreate()
        prefs = Prefs(this)
        nm = getSystemService(NotificationManager::class.java)
        nm.createNotificationChannel(
            NotificationChannel(CH_STATUS, "Tunnel status", NotificationManager.IMPORTANCE_LOW).apply {
                description = "Ongoing tunnel status while watching"
                setShowBadge(false)
            }
        )
        nm.createNotificationChannel(
            NotificationChannel(CH_ALERT, "Tunnel alerts", NotificationManager.IMPORTANCE_HIGH).apply {
                description = "The VPN dropped or traffic is leaving from the wrong IP"
                enableVibration(false) // vibration follows the in-app switch
            }
        )
        val first = Status(Level.CHECKING, Checker.vpnActive(this), null, prefs.pinnedIp, 0)
        val n = statusNotification(first)
        if (Build.VERSION.SDK_INT >= 34) {
            startForeground(ID_STATUS, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE)
        } else {
            startForeground(ID_STATUS, n)
        }
        StatusBus.publish(first)
        getSystemService(ConnectivityManager::class.java).registerDefaultNetworkCallback(netCallback)
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (!prefs.get(Prefs.WATCHING)) {
            stopSelf()
            return START_NOT_STICKY
        }
        runCheck()
        return START_STICKY
    }

    override fun onDestroy() {
        main.removeCallbacksAndMessages(null)
        runCatching { getSystemService(ConnectivityManager::class.java).unregisterNetworkCallback(netCallback) }
        worker.shutdownNow()
        nm.cancel(ID_ALERT)
        super.onDestroy()
    }

    // called on a ConnectivityManager thread
    private fun onNetworkChange() {
        val vpn = Checker.vpnActive(this)
        main.post {
            val cur = StatusBus.current
            if (!vpn && cur.level != Level.VPN_DOWN) {
                // instant: no VPN = no tunnel, whatever the last probe said
                apply(Status(Level.VPN_DOWN, false, cur.exitIp, prefs.pinnedIp, System.currentTimeMillis()))
            }
            // probe once the network has settled (fills in the raw / new exit IP)
            main.removeCallbacks(settled)
            main.postDelayed(settled, 1500)
        }
    }

    private fun runCheck() {
        main.removeCallbacks(tick)
        if (!checking.compareAndSet(false, true)) return
        worker.execute {
            val s = try {
                Checker.check(this)
            } finally {
                checking.set(false)
            }
            main.post { apply(s) }
        }
    }

    private fun apply(s: Status) {
        StatusBus.publish(s)
        nm.notify(ID_STATUS, statusNotification(s))
        alertIfNeeded(s)
        lastLevel = s.level
        main.removeCallbacks(tick)
        val ms = if (s.level != Level.OK && prefs.get(Prefs.FAST_RECHECK)) {
            FAST_RECHECK_MS
        } else {
            prefs.intervalSecs * 1000L
        }
        main.postDelayed(tick, ms)
    }

    private fun alertIfNeeded(s: Status) {
        val wanted = when (s.level) {
            Level.VPN_DOWN -> prefs.get(Prefs.ALERT_VPN_DOWN)
            Level.LEAK -> prefs.get(Prefs.ALERT_LEAK)
            Level.OFFLINE -> prefs.get(Prefs.ALERT_OFFLINE)
            Level.OK -> {
                nm.cancel(ID_ALERT)
                return
            }
            else -> false
        }
        if (!wanted || s.level == lastLevel) return
        val open = PendingIntent.getActivity(
            this, 1,
            packageManager.getLaunchIntentForPackage(SINGBOX_PACKAGE) ?: Intent(this, MainActivity::class.java),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val n = Notification.Builder(this, CH_ALERT)
            .setSmallIcon(R.drawable.ic_stat_alert)
            .setColor(colorFor(s.level))
            .setContentTitle(s.title)
            .setContentText(s.detail)
            .setStyle(Notification.BigTextStyle().bigText(s.detail))
            .setContentIntent(mainIntent())
            .addAction(Notification.Action.Builder(null, "Open sing-box", open).build())
            .setAutoCancel(true)
            .setCategory(Notification.CATEGORY_ALARM)
            .build()
        nm.notify(ID_ALERT, n)
        if (prefs.get(Prefs.VIBRATE)) {
            getSystemService(Vibrator::class.java)?.vibrate(
                VibrationEffect.createWaveform(longArrayOf(0, 250, 120, 250), -1)
            )
        }
    }

    private fun mainIntent(): PendingIntent = PendingIntent.getActivity(
        this, 0,
        Intent(this, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP),
        PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
    )

    private fun statusNotification(s: Status): Notification {
        val renew = prefs.renewText()
        val text = if (renew.isEmpty()) s.detail else "${s.detail} · $renew"
        return Notification.Builder(this, CH_STATUS)
            .setSmallIcon(if (s.level == Level.OK) R.drawable.ic_stat_ok else R.drawable.ic_stat_alert)
            .setColor(colorFor(s.level))
            .setColorized(false)
            .setContentTitle(s.title)
            .setContentText(text)
            .setContentIntent(mainIntent())
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .setShowWhen(false)
            .build()
    }
}

fun colorFor(level: Level): Int = when (level) {
    Level.OK -> 0xFF0A9F3E.toInt()
    Level.LEAK, Level.VPN_DOWN -> 0xFFC62D2D.toInt()
    Level.OFFLINE, Level.NO_PIN, Level.CHECKING -> 0xFFD98A0A.toInt()
    Level.PAUSED -> 0xFF6B7280.toInt()
}
