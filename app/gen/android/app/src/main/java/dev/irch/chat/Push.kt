package dev.irch.chat

import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import com.google.firebase.messaging.FirebaseMessaging
import com.google.firebase.messaging.FirebaseMessagingService
import com.google.firebase.messaging.RemoteMessage
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

  /** Waits for the doorbell: `{show: notices, catch: whether to catch up}`. */
  @JvmStatic external fun rung(ctx: Context, dir: String): String

  /** Our Firebase token to irch's push servers; false if none took it. */
  @JvmStatic external fun fcm(ctx: Context, dir: String, token: String): Boolean

  /** Google woke us: what the push servers hold, as `rung` tells it. */
  @JvmStatic external fun rang(ctx: Context, dir: String): String

  /** Hand Android's context back before the activity's runtime takes it. */
  @JvmStatic external fun release()
}

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

/** Google's push, through irch's push servers (core/src/push.rs), where
 *  this build has both: no connection of irch's own held open. Google
 *  carries an empty "wake"; the sealed rings come from the push server. */
class IrchFcm : FirebaseMessagingService() {
  override fun onNewToken(token: String) {
    register(this, token)
  }

  // Google's message says nothing: it only wakes us to fetch.
  override fun onMessageReceived(m: RemoteMessage) {
    if (show(this, Wake.rang(applicationContext, dataDir.absolutePath))) {
      wake(this, null)
    }
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

/** Rung through Google: a push server took our Firebase token. */
fun google(ctx: Context) = ctx.getSharedPreferences("irch", 0).getBoolean("fcm", false)

fun register(ctx: Context, token: String): Boolean {
  val ok = runCatching {
    Wake.fcm(ctx.applicationContext, ctx.dataDir.absolutePath, token)
  }.getOrDefault(false)
  ctx.getSharedPreferences("irch", 0).edit().putBoolean("fcm", ok).apply()
  return ok
}

/** Firebase's token to the push servers; `done(false)` where this build
 *  or phone has no Firebase, or no push server took it. */
fun fcm(ctx: Context, done: (Boolean) -> Unit) {
  val main = android.os.Handler(android.os.Looper.getMainLooper())
  val t = runCatching { FirebaseMessaging.getInstance().token }.getOrNull()
    ?: return done(false)
  t.addOnCompleteListener { r ->
    val token = if (r.isSuccessful) r.result else null
    Thread {
      val ok = token != null && register(ctx, token)
      main.post { done(ok) }
    }.start()
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
      // No wake lock held: a ring's packets wake the phone themselves.
      // Native code cannot see the network change; the doorbell must.
      val cm = getSystemService(android.net.ConnectivityManager::class.java)
      cm.registerDefaultNetworkCallback(object :
        android.net.ConnectivityManager.NetworkCallback() {
        override fun onAvailable(n: android.net.Network) =
          Thread { runCatching { Wake.moved() } }.start()
      })
      Thread {
        while (true) {
          val dir = dataDir.absolutePath
          // Caught up beside it, if need be: the doorbell listens at once.
          if (show(this, Wake.rung(applicationContext, dir)) &&
            catching.compareAndSet(false, true)) Thread {
            try { catchUp(this, null) } finally { catching.set(false) }
          }.start()
        }
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
    val pushed = org.unifiedpush.android.connector.UnifiedPush
      .getAckDistributor(ctx) != null
    if (!pushed && !google(ctx)) live(ctx)
  }
}

const val LIVE = "live"

