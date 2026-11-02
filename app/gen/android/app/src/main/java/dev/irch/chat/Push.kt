package dev.irch.chat

import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import org.json.JSONArray
import org.unifiedpush.android.connector.FailedReason
import org.unifiedpush.android.connector.UnifiedPush
import org.unifiedpush.android.connector.PushService
import org.unifiedpush.android.connector.data.PushEndpoint
import org.unifiedpush.android.connector.data.PushMessage

/** Rust without Tauri: a push may come when no activity is running. */
object Wake {
  init { System.loadLibrary("irch_app") }

  @JvmStatic external fun run(
    ctx: Context, dir: String, push: String?, full: Boolean,
  ): String

  @JvmStatic external fun moved()

  /** Waits for the doorbell: `{show: notices, catch: whether to catch up}`. */
  @JvmStatic external fun rung(ctx: Context, dir: String): String

  /** Hand Android's context back before the activity's runtime takes it. */
  @JvmStatic external fun release()

  /** irpush's heartbeat now; seconds to the next, 0 with no irpush. */
  @JvmStatic external fun kick(): Long

  /** The network irpush keeps its socket on: an id for each, -1 for none. */
  @JvmStatic external fun net(id: Long)
}

/** irpush's heartbeat: native timers stop while the phone sleeps, alarms
 *  do not. Inexact (no exact-alarm permission); its socket learns slack. */
class Beat : android.content.BroadcastReceiver() {
  override fun onReceive(ctx: Context, i: Intent) {
    val pm = ctx.getSystemService(android.os.PowerManager::class.java)
    // Awake until the ping is answered.
    pm.newWakeLock(android.os.PowerManager.PARTIAL_WAKE_LOCK, "irch:beat")
      .acquire(10_000)
    val s = runCatching { Wake.kick() }.getOrDefault(0L)
    if (IrchLive.running.get()) beat(ctx, if (s > 0) s else 1800L)
  }
}

fun beat(ctx: Context, secs: Long) {
  val am = ctx.getSystemService(android.app.AlarmManager::class.java)
  val pi = android.app.PendingIntent.getBroadcast(
    ctx, 0, Intent(ctx, Beat::class.java),
    android.app.PendingIntent.FLAG_IMMUTABLE,
  )
  val at = android.os.SystemClock.elapsedRealtime() + secs * 1000
  am.setAndAllowWhileIdle(
    android.app.AlarmManager.ELAPSED_REALTIME_WAKEUP, at, pi)
}

/** A network by its interface and DNS servers: `wlan0` alone is every
 *  Wi-Fi, and each network's heartbeat is learned apart. */
fun netId(lp: android.net.LinkProperties): Long =
  "${lp.interfaceName} ${lp.dnsServers}".hashCode().toLong() and 0xffffffffL

/** UnifiedPush: the ping carries nothing; irch fetches from peers. */
class IrchPush : PushService() {
  override fun onNewEndpoint(endpoint: PushEndpoint, instance: String) =
    wake(this, endpoint.url)

  override fun onMessage(message: PushMessage, instance: String) =
    wake(this, null)

  override fun onUnregistered(instance: String) = wake(this, "")

  override fun onRegistrationFailed(reason: FailedReason, instance: String) {}
}

/** A push lends a few foreground seconds to start a service; refused
 *  (Android 12+, battery optimized), irch catches up in those seconds. */
fun wake(ctx: Context, push: String?) {
  val i = Intent(ctx, IrchWake::class.java).putExtra("push", push)
  runCatching { ctx.startForegroundService(i) }.onFailure {
    Thread { catchUp(ctx, push) }.start()
  }
}

/** A ring's notices shown; true if irch must catch up for it (a message
 *  too big to carry, a DM to take): most need no node at all. */
fun show(ctx: Context, json: String): Boolean = runCatching {
  val got = org.json.JSONObject(json)
  val show = got.getJSONArray("show")
  for (k in 0 until show.length()) alert(ctx, show.getJSONObject(k))
  got.getBoolean("catch")
}.getOrDefault(true)

/** CPU held awake: asleep, Android would freeze irch mid-sync. */
fun catchUp(ctx: Context, push: String?) {
  val pm = ctx.getSystemService(Context.POWER_SERVICE) as android.os.PowerManager
  val lock = pm.newWakeLock(android.os.PowerManager.PARTIAL_WAKE_LOCK, "irch:catch-up")
  lock.acquire(60_000)
  try {
    for (full in listOf(false, true)) runCatching {
      val dir = ctx.dataDir.absolutePath
      val got = JSONArray(Wake.run(ctx.applicationContext, dir, push, full))
      for (k in 0 until got.length()) alert(ctx, got.getJSONObject(k))
    }
  } finally {
    if (lock.isHeld) lock.release()
  }
}

class IrchWake : Service() {
  override fun onBind(intent: Intent?): IBinder? = null

  override fun onStartCommand(intent: Intent?, flags: Int, id: Int): Int {
    val quiet = NotificationManagerCompat.IMPORTANCE_MIN
    notificationChannel(this, SYNCING, "Checking for messages", quiet)
    val n = NotificationCompat.Builder(this, SYNCING)
      .setSmallIcon(R.drawable.ic_stat)
      .setContentTitle("Checking for messages")
      .build()
    if (Build.VERSION.SDK_INT >= 34) {
      startForeground(2, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_SHORT_SERVICE)
    } else {
      startForeground(2, n)
    }
    val push = intent?.getStringExtra("push")
    Thread {
      catchUp(this, push)
      stopSelf(id)
    }.start()
    return START_NOT_STICKY
  }

  // Android ends a short service after 3 minutes; ours ends sooner.
  override fun onTimeout(startId: Int, fgsType: Int) = stopSelf()
}

const val SYNCING = "syncing"

/** No push app: irpush through a storage node (one quiet socket), else a
 *  doorbell (src/android.rs), for a little more battery. */
fun live(ctx: Context) {
  runCatching { ctx.startForegroundService(Intent(ctx, IrchLive::class.java)) }
}

class IrchLive : Service() {
  override fun onBind(intent: Intent?): IBinder? = null

  override fun onStartCommand(intent: Intent?, flags: Int, id: Int): Int {
    val quiet = NotificationManagerCompat.IMPORTANCE_MIN
    notificationChannel(this, LIVE, "Connected", quiet)
    val n = NotificationCompat.Builder(this, LIVE)
      .setSmallIcon(R.drawable.ic_stat)
      .setContentTitle("Connected for messages and calls")
      .setContentIntent(openAppIntent(this))
      .build()
    if (Build.VERSION.SDK_INT >= 34) {
      val type = ServiceInfo.FOREGROUND_SERVICE_TYPE_REMOTE_MESSAGING
      startForeground(3, n, type)
    } else {
      startForeground(3, n)
    }
    if (running.compareAndSet(false, true)) {
      // No wake lock held: a ring's packets wake the phone themselves.
      // Native code cannot see the network change; the doorbell must.
      val cm = getSystemService(android.net.ConnectivityManager::class.java)
      cm.registerDefaultNetworkCallback(object :
        android.net.ConnectivityManager.NetworkCallback() {
        override fun onAvailable(n: android.net.Network) =
          Thread { runCatching { Wake.moved() } }.start()

        override fun onLinkPropertiesChanged(
          n: android.net.Network, lp: android.net.LinkProperties,
        ) = runCatching { Wake.net(netId(lp)) }.let {}

        override fun onLost(n: android.net.Network) =
          runCatching { Wake.net(-1L) }.let {}
      })
      beat(this, 120L)
      Thread {
        // Until a UnifiedPush app takes over.
        while (UnifiedPush.getAckDistributor(this) == null) {
          val dir = dataDir.absolutePath
          // Caught up beside it, if need be: the doorbell listens at once.
          if (show(this, Wake.rung(applicationContext, dir)) &&
            catching.compareAndSet(false, true)) Thread {
            try { catchUp(this, null) } finally { catching.set(false) }
          }.start()
        }
        stopSelf()
      }.start()
    }
    return START_STICKY
  }

  companion object {
    val running = java.util.concurrent.atomic.AtomicBoolean(false)
    val catching = java.util.concurrent.atomic.AtomicBoolean(false)
  }
}

class Boot : android.content.BroadcastReceiver() {
  override fun onReceive(ctx: Context, i: Intent) {
    if (i.action != Intent.ACTION_BOOT_COMPLETED) return
    if (UnifiedPush.getAckDistributor(ctx) == null) live(ctx)
  }
}

const val LIVE = "live"

