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
import androidx.core.app.ActivityCompat
import androidx.core.app.NotificationChannelCompat
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.content.ContextCompat
import androidx.core.content.FileProvider
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.ProcessLifecycleOwner
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Channel
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin

@InvokeArg
class WatchArgs {
  lateinit var channel: Channel
}

@InvokeArg
class MentionArgs {
  var tag = ""
  var title = ""
  var body = ""
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
class BackgroundArgs {
  var on = false
}

/** What the Rust side (src-tauri/src/android.rs) needs from Android. */
@TauriPlugin
class IrchPlugin(private val activity: Activity) : Plugin(activity) {
  /** Native code cannot observe network changes on Android; iroh needs them. */
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
    invoke.resolve()
  }

  /** A mention, shown only while irch is in the background. */
  @Command
  fun mention(invoke: Invoke) {
    val args = invoke.parseArgs(MentionArgs::class.java)
    val state = ProcessLifecycleOwner.get().lifecycle.currentState
    if (!state.isAtLeast(Lifecycle.State.STARTED)) postMention(activity, args)
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

  /** Keep the node running in the background: a foreground service. */
  @Command
  fun background(invoke: Invoke) {
    val on = invoke.parseArgs(BackgroundArgs::class.java).on
    val service = Intent(activity, IrchService::class.java)
    try {
      if (on) {
        if (!notificationsAllowed(activity)) {
          val perm = arrayOf(POST_NOTIFICATIONS)
          ActivityCompat.requestPermissions(activity, perm, 1)
        }
        ContextCompat.startForegroundService(activity, service)
      } else {
        activity.stopService(service)
      }
      invoke.resolve()
    } catch (e: Exception) {
      invoke.reject(e.message)
    }
  }
}

const val MENTIONS = "mentions"
const val CONNECTED = "connected"

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

/** Locked, the screen shows only "New message"; the text needs unlocking. */
private fun postMention(ctx: Context, args: MentionArgs) {
  if (!notificationsAllowed(ctx)) return
  val high = NotificationManagerCompat.IMPORTANCE_HIGH
  notificationChannel(ctx, MENTIONS, "Mentions", high)
  val hidden = NotificationCompat.Builder(ctx, MENTIONS)
    .setSmallIcon(R.drawable.ic_stat)
    .setContentTitle("New message")
    .build()
  val n = NotificationCompat.Builder(ctx, MENTIONS)
    .setSmallIcon(R.drawable.ic_stat)
    .setContentTitle(args.title)
    .setContentText(args.body)
    .setStyle(NotificationCompat.BigTextStyle().bigText(args.body))
    .setCategory(NotificationCompat.CATEGORY_MESSAGE)
    .setVisibility(NotificationCompat.VISIBILITY_PRIVATE)
    .setPublicVersion(hidden)
    .setContentIntent(openAppIntent(ctx))
    .setAutoCancel(true)
    .build()
  NotificationManagerCompat.from(ctx).notify(args.tag, 0, n)
}
