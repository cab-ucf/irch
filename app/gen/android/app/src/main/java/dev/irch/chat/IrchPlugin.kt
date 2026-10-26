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
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Channel
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import org.unifiedpush.android.connector.UnifiedPush

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

/** What the Rust side (src/android.rs) needs from Android. */
@TauriPlugin
class IrchPlugin(private val activity: Activity) : Plugin(activity) {
  /** Ask for notifications once, and be woken through UnifiedPush. */
  override fun load(webView: WebView) {
    if (!notificationsAllowed(activity) && Build.VERSION.SDK_INT >= 33) {
      ActivityCompat.requestPermissions(activity, arrayOf(POST_NOTIFICATIONS), 1)
    }
    // A push app wakes irch for almost no battery; without one, a service
    // keeps it running, or messages and calls would wait for it.
    UnifiedPush.tryUseCurrentOrDefaultDistributor(activity) { ok ->
      if (ok) UnifiedPush.register(activity) else live(activity)
    }
  }

  /**
   * Native code cannot observe network changes on Android; iroh needs them.
   * Nor whether irch is in sight: out of it, the node sleeps (battery).
   */
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

  /** The Rust side left out what is on screen; the rest is told. */
  @Command
  fun alert(invoke: Invoke) {
    val a = invoke.parseArgs(AlertArgs::class.java)
    alert(activity, org.json.JSONObject().put("space", a.space)
      .put("room", a.room).put("title", a.title).put("body", a.body)
      .put("ring", a.ring))
    invoke.resolve()
  }

  /** A call answered or declined in the page: the ringing stops. */
  @Command
  fun hush(invoke: Invoke) {
    hush(activity)
    invoke.resolve()
  }

  @Command
  fun share(invoke: Invoke) {
    val text = invoke.parseArgs(ShareArgs::class.java).text
    val send = Intent(Intent.ACTION_SEND).setType("text/plain")
      .putExtra(Intent.EXTRA_TEXT, text)
    activity.startActivity(Intent.createChooser(send, null))
    invoke.resolve()
  }

  /**
   * A newer irch (downloaded by the Rust side) to Android's installer,
   * which takes it only if signed with the same key as this one.
   */
  @Command
  fun install(invoke: Invoke) {
    val ok = Build.VERSION.SDK_INT < 26 ||
      activity.packageManager.canRequestPackageInstalls()
    if (!ok) {
      val uri = android.net.Uri.parse("package:" + activity.packageName)
      val allow = Intent(
        android.provider.Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES, uri
      )
      activity.startActivity(allow)
      return invoke.reject("allow irch to install updates, then try again")
    }
    val file = java.io.File(invoke.parseArgs(FileArgs::class.java).path)
    val authority = activity.packageName + ".fileprovider"
    val uri = FileProvider.getUriForFile(activity, authority, file)
    val apk = "application/vnd.android.package-archive"
    val i = Intent(Intent.ACTION_VIEW).setDataAndType(uri, apk)
      .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
    activity.startActivity(i)
    invoke.resolve()
  }

  /** Hand a saved attachment to another app: Files, Photos, a messenger. */
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
