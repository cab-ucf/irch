package dev.irch.chat

import android.Manifest.permission.POST_NOTIFICATIONS
import android.app.Activity
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.net.ConnectivityManager
import android.net.LinkProperties
import android.net.Network
import android.os.Build
import android.webkit.WebView
import androidx.core.app.ActivityCompat
import androidx.core.app.NotificationChannelCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.content.ContextCompat
import androidx.core.content.FileProvider
import androidx.lifecycle.DefaultLifecycleObserver
import androidx.lifecycle.LifecycleOwner
import androidx.lifecycle.ProcessLifecycleOwner
import app.tauri.PermissionState
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.Permission
import app.tauri.annotation.PermissionCallback
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Channel
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import org.unifiedpush.android.connector.UnifiedPush

@InvokeArg
class UrlArgs {
  var url = ""
}

@InvokeArg
class WatchArgs {
  lateinit var channel: Channel
}

@InvokeArg
class AlertArgs {
  var space = ""
  var room = ""
  var title = ""
  var body = ""
  var ring = false
}

@InvokeArg
class ShareArgs {
  var text = ""
}

@InvokeArg
class FileArgs {
  var path = ""
}

@InvokeArg
class OnArgs {
  var on = false
}

@TauriPlugin(permissions = [
  Permission(strings = [android.Manifest.permission.RECORD_AUDIO],
    alias = "microphone"),
])
class IrchPlugin(private val activity: Activity) : Plugin(activity) {
  override fun load(webView: WebView) {
    if (!notificationsAllowed(activity) && Build.VERSION.SDK_INT >= 33) {
      ActivityCompat.requestPermissions(activity, arrayOf(POST_NOTIFICATIONS), 1)
    }
    // A push app costs almost no battery; else a service keeps a doorbell.
    UnifiedPush.tryUseCurrentOrDefaultDistributor(activity) { ok ->
      if (ok) UnifiedPush.register(activity) else live(activity)
    }
  }

  /** Optimized, Doze stops irch staying connected or waking on push. */
  private fun unrestrict() = runCatching {
    val uri = android.net.Uri.parse("package:" + activity.packageName)
    activity.startActivity(Intent(
      android.provider.Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS, uri
    ))
  }

  @Command
  fun alerts(invoke: Invoke) {
    val push = UnifiedPush.getAckDistributor(activity)
    val label = push?.let {
      runCatching {
        val pm = activity.packageManager
        pm.getApplicationLabel(pm.getApplicationInfo(it, 0)).toString()
      }.getOrDefault(it)
    }
    invoke.resolve(JSObject()
      .put("notify", notificationsAllowed(activity))
      .put("background", unrestricted(activity))
      .put("calls", fullScreen(activity))
      .put("push", label))
  }

  @Command
  fun allowAlerts(invoke: Invoke) {
    if (!notificationsAllowed(activity)) {
      val i = Intent(android.provider.Settings.ACTION_APP_NOTIFICATION_SETTINGS)
        .putExtra(android.provider.Settings.EXTRA_APP_PACKAGE,
          activity.packageName)
      activity.startActivity(i)
    } else if (!unrestricted(activity)) unrestrict()
    else if (!fullScreen(activity)) runCatching {
      val uri = android.net.Uri.parse("package:" + activity.packageName)
      activity.startActivity(Intent(
        android.provider.Settings.ACTION_MANAGE_APP_USE_FULL_SCREEN_INTENT, uri
      ))
    }
    invoke.resolve()
  }

  @Command
  fun testAlert(invoke: Invoke) {
    postMessage(activity, "test", "irch", "Notifications reach this phone.")
    invoke.resolve()
  }

  /** Native code sees neither network changes nor whether irch is in sight. */
  @Command
  fun watchNetwork(invoke: Invoke) {
    val channel = invoke.parseArgs(WatchArgs::class.java).channel
    val cm = activity.getSystemService(ConnectivityManager::class.java)
    val ping = object : ConnectivityManager.NetworkCallback() {
      override fun onAvailable(network: Network) = channel.send(JSObject())
      override fun onLost(network: Network) = channel.send(JSObject())
      override fun onLinkPropertiesChanged(n: Network, lp: LinkProperties) =
        channel.send(JSObject())
    }
    cm.registerDefaultNetworkCallback(ping)
    val seen = object : DefaultLifecycleObserver {
      override fun onStart(owner: LifecycleOwner) =
        channel.send(JSObject().put("fg", true))
      override fun onStop(owner: LifecycleOwner) =
        channel.send(JSObject().put("fg", false))
    }
    activity.runOnUiThread {
      ProcessLifecycleOwner.get().lifecycle.addObserver(seen)
    }
    invoke.resolve()
  }

  @Command
  fun alert(invoke: Invoke) {
    val a = invoke.parseArgs(AlertArgs::class.java)
    alert(activity, org.json.JSONObject().put("space", a.space)
      .put("room", a.room).put("title", a.title).put("body", a.body)
      .put("ring", a.ring))
    invoke.resolve()
  }

  @Command
  fun hush(invoke: Invoke) {
    hush(activity)
    invoke.resolve()
  }

  /** A call: the screen on, over the lock screen, and the microphone
   *  allowed before irch's own voice opens it. */
  @Command
  fun inCall(invoke: Invoke) {
    val on = invoke.parseArgs(OnArgs::class.java).on
    activity.runOnUiThread { (activity as? MainActivity)?.overLock(on) }
    if (on && getPermissionState("microphone") != PermissionState.GRANTED) {
      requestPermissionForAlias("microphone", invoke, "allowed")
    } else invoke.resolve()
  }

  @PermissionCallback
  private fun allowed(invoke: Invoke) = invoke.resolve()

  @Command
  fun share(invoke: Invoke) {
    val text = invoke.parseArgs(ShareArgs::class.java).text
    val send = Intent(Intent.ACTION_SEND).setType("text/plain")
      .putExtra(Intent.EXTRA_TEXT, text)
    activity.startActivity(Intent.createChooser(send, null))
    invoke.resolve()
  }

  @Command
  fun browse(invoke: Invoke) {
    val url = invoke.parseArgs(UrlArgs::class.java).url
    val i = Intent(Intent.ACTION_VIEW, android.net.Uri.parse(url))
    try {
      activity.startActivity(i.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
      invoke.resolve()
    } catch (e: android.content.ActivityNotFoundException) {
      invoke.reject("no app opens it")
    }
  }

  @Command
  fun shareFile(invoke: Invoke) {
    val file = java.io.File(invoke.parseArgs(FileArgs::class.java).path)
    val authority = activity.packageName + ".fileprovider"
    val uri = FileProvider.getUriForFile(activity, authority, file)
    val type =
      activity.contentResolver.getType(uri) ?: "application/octet-stream"
    val send = Intent(Intent.ACTION_SEND).setType(type)
      .putExtra(Intent.EXTRA_STREAM, uri)
      .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
    activity.startActivity(Intent.createChooser(send, null))
    invoke.resolve()
  }
}

fun unrestricted(ctx: Context) =
  ctx.getSystemService(android.os.PowerManager::class.java)
    .isIgnoringBatteryOptimizations(ctx.packageName)

/** Android 14 on: a call fills a locked screen only if allowed. */
fun fullScreen(ctx: Context) = Build.VERSION.SDK_INT < 34 ||
  ctx.getSystemService(android.app.NotificationManager::class.java)
    .canUseFullScreenIntent()

fun notificationsAllowed(ctx: Context) = Build.VERSION.SDK_INT < 33 ||
  ContextCompat.checkSelfPermission(ctx, POST_NOTIFICATIONS) ==
  PackageManager.PERMISSION_GRANTED

fun openAppIntent(ctx: Context): PendingIntent = PendingIntent.getActivity(
  ctx, 0, Intent(ctx, MainActivity::class.java),
  PendingIntent.FLAG_IMMUTABLE,
)

fun notificationChannel(ctx: Context, id: String, name: String, level: Int) =
  NotificationManagerCompat.from(ctx).createNotificationChannel(
    NotificationChannelCompat.Builder(id, level).setName(name).build()
  )
