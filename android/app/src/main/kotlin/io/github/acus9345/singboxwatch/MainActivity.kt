package io.github.acus9345.singboxwatch

import android.Manifest
import android.app.Activity
import android.app.AlertDialog
import android.content.Intent
import android.content.pm.PackageManager
import android.graphics.Typeface
import android.graphics.drawable.GradientDrawable
import android.os.Build
import android.os.Bundle
import android.provider.Settings
import android.text.InputType
import android.util.TypedValue
import android.view.Gravity
import android.view.View
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.Switch
import android.widget.TextView
import android.widget.Toast
import java.text.DateFormat
import java.util.Date

/** Status + every switch. All work happens in WatchService; this only shows it. */
class MainActivity : Activity() {

    private lateinit var prefs: Prefs
    private lateinit var dot: View
    private lateinit var title: TextView
    private lateinit var detail: TextView
    private lateinit var rows: Map<String, TextView>
    private lateinit var intervalBtn: Button
    private val listener: (Status) -> Unit = { render(it) }

    private val textColor by lazy { getColor(R.color.text) }
    private val mutedColor by lazy { getColor(R.color.muted) }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        prefs = Prefs(this)
        setContentView(buildUi())
        if (Build.VERSION.SDK_INT >= 33 &&
            checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED
        ) {
            requestPermissions(arrayOf(Manifest.permission.POST_NOTIFICATIONS), 1)
        }
    }

    override fun onStart() {
        super.onStart()
        StatusBus.listen(listener)
        if (prefs.get(Prefs.WATCHING)) WatchService.start(this, WatchService.ACTION_CHECK)
        else WatchService.stop(this)
    }

    override fun onStop() {
        StatusBus.unlisten(listener)
        super.onStop()
    }

    // ---------- rendering ----------

    private fun render(s: Status) {
        (dot.background as GradientDrawable).setColor(colorFor(s.level))
        title.text = s.title
        detail.text = s.detail
        rows.getValue("vpn").text = if (s.vpn) "active" else "none"
        rows.getValue("exit").text = s.exitIp ?: "–"
        rows.getValue("pinned").text = prefs.pinnedIp.ifEmpty { "(not pinned)" }
        rows.getValue("checked").text =
            if (s.checkedAt == 0L) "–" else DateFormat.getTimeInstance(DateFormat.MEDIUM).format(Date(s.checkedAt))
        rows.getValue("renew").text = prefs.renewText().ifEmpty { "(no date set)" }
    }

    // ---------- actions ----------

    private fun checkNow() {
        if (!prefs.get(Prefs.WATCHING)) {
            Toast.makeText(this, "Turn on \"Watch the tunnel\" first", Toast.LENGTH_SHORT).show()
            return
        }
        WatchService.start(this, WatchService.ACTION_CHECK)
    }

    private fun pinCurrentIp() {
        if (!Checker.vpnActive(this)) {
            AlertDialog.Builder(this)
                .setTitle("sing-box is not on")
                .setMessage("Turn sing-box on first: the pinned IP must be the tunnel's exit IP, not your mobile/Wi-Fi IP.")
                .setPositiveButton("Open sing-box") { _, _ -> openSingBox() }
                .setNegativeButton("Cancel", null)
                .show()
            return
        }
        Toast.makeText(this, "Checking exit IP…", Toast.LENGTH_SHORT).show()
        Thread {
            val ip = Checker.fetchExitIp()
            runOnUiThread {
                if (ip == null) {
                    Toast.makeText(this, "No answer from the exit-IP services", Toast.LENGTH_LONG).show()
                } else {
                    AlertDialog.Builder(this)
                        .setTitle("Pin $ip?")
                        .setMessage("Every check will compare the phone's exit IP against $ip.")
                        .setPositiveButton("Pin") { _, _ ->
                            prefs.pinnedIp = ip
                            checkNow()
                        }
                        .setNegativeButton("Cancel", null)
                        .show()
                }
            }
        }.start()
    }

    private fun editPinned() {
        val input = EditText(this).apply {
            inputType = InputType.TYPE_CLASS_TEXT
            hint = "e.g. 178.93.182.244"
            setText(prefs.pinnedIp)
        }
        AlertDialog.Builder(this)
            .setTitle("Pinned exit IP")
            .setView(padded(input))
            .setPositiveButton("Save") { _, _ ->
                val v = input.text.toString().trim()
                if (v.isEmpty() || Regex("""^(\d{1,3}\.){3}\d{1,3}$""").matches(v)) {
                    prefs.pinnedIp = v
                    checkNow()
                    render(StatusBus.current)
                } else {
                    Toast.makeText(this, "Not an IPv4 address", Toast.LENGTH_SHORT).show()
                }
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    private fun editRenew() {
        val input = EditText(this).apply {
            inputType = InputType.TYPE_CLASS_DATETIME
            hint = "YYYY-MM-DD (empty = off)"
            setText(prefs.renewDeadline)
        }
        AlertDialog.Builder(this)
            .setTitle("Proxy renewal date")
            .setView(padded(input))
            .setPositiveButton("Save") { _, _ ->
                val v = input.text.toString().trim()
                if (v.isEmpty() || runCatching { java.time.LocalDate.parse(v) }.isSuccess) {
                    prefs.renewDeadline = v
                    checkNow()
                    render(StatusBus.current)
                } else {
                    Toast.makeText(this, "Use YYYY-MM-DD", Toast.LENGTH_SHORT).show()
                }
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    private fun pickInterval() {
        val opts = Prefs.INTERVALS
        AlertDialog.Builder(this)
            .setTitle("Check exit IP every")
            .setSingleChoiceItems(opts.map(::intervalLabel).toTypedArray(), opts.indexOf(prefs.intervalSecs)) { d, i ->
                prefs.intervalSecs = opts[i]
                intervalBtn.text = "Check every ${intervalLabel(opts[i])}"
                checkNow()
                d.dismiss()
            }
            .show()
    }

    private fun openSingBox() {
        if (!WatchService.openSingBox(this)) {
            Toast.makeText(this, "sing-box app (io.nekohasekai.sfa) not found", Toast.LENGTH_LONG).show()
        }
    }

    private fun intervalLabel(s: Int) = if (s < 60) "$s s" else "${s / 60} min"

    // ---------- layout (plain views, no extra libraries) ----------

    private fun dp(v: Int) = TypedValue.applyDimension(TypedValue.COMPLEX_UNIT_DIP, v.toFloat(), resources.displayMetrics).toInt()

    private fun padded(v: View) = LinearLayout(this).apply {
        setPadding(dp(20), dp(8), dp(20), 0)
        addView(v, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
    }

    private fun card(vararg children: View) = LinearLayout(this).apply {
        orientation = LinearLayout.VERTICAL
        setPadding(dp(16), dp(14), dp(16), dp(14))
        background = GradientDrawable().apply {
            setColor(getColor(R.color.card))
            cornerRadius = dp(16).toFloat()
            setStroke(dp(1), getColor(R.color.line))
        }
        children.forEach { addView(it) }
        layoutParams = LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT).apply { bottomMargin = dp(12) }
    }

    private fun text(s: String, size: Float = 15f, color: Int = textColor, bold: Boolean = false) =
        TextView(this).apply {
            text = s
            setTextSize(TypedValue.COMPLEX_UNIT_SP, size)
            setTextColor(color)
            if (bold) typeface = Typeface.DEFAULT_BOLD
        }

    private fun heading(s: String) = text(s.uppercase(), 12f, mutedColor, bold = true).apply {
        letterSpacing = 0.06f
        setPadding(0, 0, 0, dp(6))
    }

    private fun infoRow(label: String): Pair<View, TextView> {
        val value = text("–", 14f).apply { gravity = Gravity.END }
        val row = LinearLayout(this).apply {
            setPadding(0, dp(3), 0, dp(3))
            addView(text(label, 14f, mutedColor), LinearLayout.LayoutParams(0, WRAP_CONTENT, 1f))
            addView(value, LinearLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT))
        }
        return row to value
    }

    private fun button(label: String, onClick: () -> Unit) = Button(this).apply {
        text = label
        isAllCaps = false
        setOnClickListener { onClick() }
        layoutParams = LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT).apply { topMargin = dp(4) }
    }

    private fun switchRow(sw: Prefs.Switch, first: Boolean): View {
        val toggle = Switch(this).apply { isChecked = prefs.get(sw.key) }
        val labels = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            addView(text(sw.label))
            if (sw.hint.isNotEmpty()) addView(text(sw.hint, 12.5f, mutedColor))
        }
        val row = LinearLayout(this).apply {
            gravity = Gravity.CENTER_VERTICAL
            setPadding(0, if (first) 0 else dp(10), 0, dp(10))
            addView(labels, LinearLayout.LayoutParams(0, WRAP_CONTENT, 1f).apply { marginEnd = dp(12) })
            addView(toggle)
            setOnClickListener { toggle.toggle() }
        }
        toggle.setOnCheckedChangeListener { _, on ->
            prefs.set(sw.key, on)
            when (sw.key) {
                Prefs.WATCHING -> if (on) WatchService.start(this, WatchService.ACTION_CHECK) else WatchService.stop(this)
                else -> if (prefs.get(Prefs.WATCHING)) checkNow()
            }
        }
        return row
    }

    private fun buildUi(): View {
        val col = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(16), dp(20), dp(16), dp(24))
        }
        col.addView(text("singbox-watch", 15f, mutedColor, bold = true).apply { setPadding(dp(2), 0, 0, dp(12)) })

        // status hero
        dot = View(this).apply {
            background = GradientDrawable().apply { shape = GradientDrawable.OVAL; setColor(colorFor(Level.PAUSED)) }
            layoutParams = LinearLayout.LayoutParams(dp(96), dp(96)).apply { gravity = Gravity.CENTER_HORIZONTAL; bottomMargin = dp(12) }
        }
        title = text("…", 22f, bold = true).apply { gravity = Gravity.CENTER }
        detail = text("", 14f, mutedColor).apply { gravity = Gravity.CENTER; setPadding(0, dp(4), 0, dp(8)) }
        col.addView(card(dot, title, detail,
            button("Check now") { checkNow() },
            button("Open sing-box") { openSingBox() }).apply { setPadding(dp(16), dp(22), dp(16), dp(14)) })

        // details
        val r = listOf("vpn" to "VPN", "exit" to "Exit IP", "pinned" to "Pinned IP", "checked" to "Last check", "renew" to "Renewal")
            .associate { (k, label) -> k to infoRow(label) }
        rows = r.mapValues { it.value.second }
        col.addView(card(*r.values.map { it.first }.toTypedArray()))

        // pinned values
        intervalBtn = button("Check every ${intervalLabel(prefs.intervalSecs)}") { pickInterval() }
        col.addView(card(heading("Pinned"),
            button("Pin current IP (sing-box on)") { pinCurrentIp() },
            button("Edit pinned IP") { editPinned() },
            button("Renewal date") { editRenew() },
            intervalBtn))

        // switches
        val sw = card(heading("Settings"))
        Prefs.SWITCHES.forEachIndexed { i, s -> sw.addView(switchRow(s, i == 0)) }
        col.addView(sw)

        // Android's own kill switch
        col.addView(card(heading("Block leaks"),
            text("Android can block all traffic whenever sing-box's VPN is down. In VPN settings, open sing-box's gear and turn on \"Always-on VPN\" and \"Block connections without VPN\".", 14f, mutedColor),
            button("Open Android VPN settings") {
                runCatching { startActivity(Intent(Settings.ACTION_VPN_SETTINGS)) }
            }))

        return ScrollView(this).apply {
            isFillViewport = true
            fitsSystemWindows = true
            addView(col)
        }
    }
}
