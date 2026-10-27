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

  /** Waits for the doorbell, letting the CPU sleep. */
  @JvmStatic external fun rung(ctx: Context, dir: String): String

  /** Hand Android's context back before the activity's runtime takes it. */
  @JvmStatic external fun release()
}

/** UnifiedPush: the ping carries nothing; irch fetches from peers. */
class IrchPush : PushService() {
  override fun onNewEndpoint(endpoint: PushEndpoint, instance: String) =
    wake(endpoint.url)

  override fun onMessage(message: PushMessage, instance: String) = wake(null)

  override fun onUnregistered(instance: String) = wake("")

  override fun onRegistrationFailed(reason: FailedReason, instance: String) {}

  private fun wake(push: String?) {
    val i = Intent(this, IrchWake::class.java).putExtra("push", push)
    // The distributor lends a few foreground seconds to start a service;
    // refused (Android 12+, battery optimized), catch up in those seconds.
    runCatching { startForegroundService(i) }.onFailure {
      Thread {
        catchUp(this, push)
      }.start()
    }
  }
}

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

/** No push app: a doorbell (src/android.rs) rings, for little battery. */
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
      // Relays drop clients that miss a ping (~18 s) by 5 s; asleep, a
      // phone often does, silencing the doorbell for minutes. So the CPU
      // idles awake, which Android honors while irch runs unrestricted.
      val pm = getSystemService(android.os.PowerManager::class.java)
      awake = pm.newWakeLock(android.os.PowerManager.PARTIAL_WAKE_LOCK, "irch:doorbell")
        .apply { setReferenceCounted(false) }
      // Native code cannot see the network change; the doorbell must.
      val cm = getSystemService(android.net.ConnectivityManager::class.java)
      cm.registerDefaultNetworkCallback(object :
        android.net.ConnectivityManager.NetworkCallback() {
        override fun onAvailable(n: android.net.Network) =
          Thread { runCatching { Wake.moved() } }.start()
      })
      Thread {
        while (true) {
          awake?.acquire(40 * 60_000L) // the doorbell waits at most 30 min
          runCatching { Wake.rung(applicationContext, dataDir.absolutePath) }
          catchUp(this, null)
        }
      }.start()
    }
    return START_STICKY
  }

  override fun onDestroy() {
    awake?.release()
    super.onDestroy()
  }

  companion object {
    val running = java.util.concurrent.atomic.AtomicBoolean(false)
    var awake: android.os.PowerManager.WakeLock? = null
  }
}

class Boot : android.content.BroadcastReceiver() {
  override fun onReceive(ctx: Context, i: Intent) {
    if (i.action != Intent.ACTION_BOOT_COMPLETED) return
    val pushed = org.unifiedpush.android.connector.UnifiedPush
      .getAckDistributor(ctx) != null
    if (!pushed) live(ctx)
  }
}

const val LIVE = "live"

