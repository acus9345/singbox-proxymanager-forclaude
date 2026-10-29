package io.github.acus9345.singboxwatch

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent

/** Restarts the watcher after a reboot or an app update. */
class BootReceiver : BroadcastReceiver() {
    override fun onReceive(ctx: Context, intent: Intent) {
        val p = Prefs(ctx)
        val boot = intent.action == Intent.ACTION_BOOT_COMPLETED
        if (p.get(Prefs.WATCHING) && (!boot || p.get(Prefs.START_ON_BOOT))) {
            WatchService.start(ctx)
        }
    }
}
