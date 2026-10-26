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

/**
 * The Rust side (src/android.rs), without the app: a push can arrive after
 * Android stopped irch, when no activity (and so no Tauri) is running.
 */
object Wake {
  init { System.loadLibrary("irch_app") }

  /** Start or wake the node, catch up; what to notify, as JSON. */
  @JvmStatic external fun run(ctx: Context, dir: String, push: String?): String

  /** Keep the node running; the next things to notify, as JSON. */
  @JvmStatic external fun next(ctx: Context, dir: String): String

  /** Hand Android's context back before the activity's runtime takes it. */
  @JvmStatic external fun release()
}

/**
 * UnifiedPush (ntfy, or another distributor the user picked): someone in
 * a space wrote. The ping carries nothing; irch fetches from peers.
 */
class IrchPush : PushService() {
  override fun onNewEndpoint(endpoint: PushEndpoint, instance: String) =
    wake(endpoint.url)

  override fun onMessage(message: PushMessage, instance: String) = wake(null)

  override fun onUnregistered(instance: String) = wake("")

  override fun onRegistrationFailed(reason: FailedReason, instance: String) {}

  private fun wake(push: String?) {
    val i = Intent(this, IrchWake::class.java).putExtra("push", push)
    // The distributor holds us in the foreground for a few seconds: enough
    // to start a short foreground service from the background. Refused
    // (Android 12+, irch under battery optimization): catch up here, in
    // the seconds this service has.
    runCatching { startForegroundService(i) }.onFailure {
      Thread {
        runCatching {
          val got = JSONArray(Wake.run(applicationContext, dataDir.absolutePath, push))
          for (k in 0 until got.length()) alert(this, got.getJSONObject(k))
        }
      }.start()
    }
  }
}

/** A few seconds of catching up, then notifications, then sleep again. */
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
      runCatching {
        val dir = dataDir.absolutePath
        val got = JSONArray(Wake.run(applicationContext, dir, push))
        for (k in 0 until got.length()) alert(this, got.getJSONObject(k))
      }
      stopSelf(id)
    }.start()
    return START_NOT_STICKY
  }

  // Android ends a short service after 3 minutes; ours ends sooner.
  override fun onTimeout(startId: Int, fgsType: Int) = stopSelf()
}

const val SYNCING = "syncing"

/** Its owner's own servers wake it (src/android.rs): irch saved them. */
fun wakeServers(ctx: Context) = java.io.File(ctx.dataDir, "wake.json").exists()

/** No push app: irch stays connected, so messages and calls arrive. */
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
    if (running.compareAndSet(false, true)) Thread {
      val dir = dataDir.absolutePath
      while (true) {
        val got = runCatching { JSONArray(Wake.next(applicationContext, dir)) }
          .getOrDefault(JSONArray())
        for (k in 0 until got.length()) alert(this, got.getJSONObject(k))
        if (got.length() == 0) Thread.sleep(5000) // could not start: again
      }
    }.start()
    return START_STICKY
  }

  companion object {
    val running = java.util.concurrent.atomic.AtomicBoolean(false)
  }
}

/** After a restart, before irch is opened: connected again, unless a
 *  push app will wake it. */
class Boot : android.content.BroadcastReceiver() {
  override fun onReceive(ctx: Context, i: Intent) {
    if (i.action != Intent.ACTION_BOOT_COMPLETED) return
    val pushed = org.unifiedpush.android.connector.UnifiedPush
      .getAckDistributor(ctx) != null
    if (!pushed || wakeServers(ctx)) live(ctx)
  }
}

const val LIVE = "live"

