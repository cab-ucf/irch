package dev.irch.chat

import android.app.Notification
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.media.AudioAttributes
import android.net.Uri
import android.provider.Settings
import androidx.core.app.NotificationChannelCompat
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.app.Person
import org.json.JSONObject

const val CHAT = "chat"
const val CALLS = "calls"
const val CALL = 1

/** What irch tells of: a message, or a call in a DM, which rings. */
fun alert(ctx: Context, a: JSONObject) {
  if (!notificationsAllowed(ctx)) return
  val (space, room) = a.getString("space") to a.getString("room")
  val (title, body) = a.getString("title") to a.getString("body")
  if (a.optBoolean("ring")) postCall(ctx, space, room, title)
  else postMessage(ctx, space + room, title, body)
}

private fun channel(ctx: Context, id: String, name: String, sound: Uri,
                    use: Int, buzz: LongArray) {
  val audio = AudioAttributes.Builder().setUsage(use).build()
  val high = NotificationManagerCompat.IMPORTANCE_HIGH
  NotificationManagerCompat.from(ctx).createNotificationChannel(
    NotificationChannelCompat.Builder(id, high)
      .setName(name).setSound(sound, audio)
      .setVibrationEnabled(true).setVibrationPattern(buzz).build()
  )
}

/** Like a text: a sound and a buzz. Locked, only "New message" shows. */
fun postMessage(ctx: Context, tag: String, title: String, body: String) {
  // Its first channel left sound and vibration to the system: replaced.
  NotificationManagerCompat.from(ctx).deleteNotificationChannel("messages")
  channel(ctx, CHAT, "Messages", Settings.System.DEFAULT_NOTIFICATION_URI,
    AudioAttributes.USAGE_NOTIFICATION,
    longArrayOf(0, 250, 150, 250))
  val hidden = NotificationCompat.Builder(ctx, CHAT)
    .setSmallIcon(R.drawable.ic_stat)
    .setContentTitle("New message")
    .build()
  val n = NotificationCompat.Builder(ctx, CHAT)
    .setSmallIcon(R.drawable.ic_stat)
    .setContentTitle(title)
    .setContentText(body)
    .setStyle(NotificationCompat.BigTextStyle().bigText(body))
    .setCategory(NotificationCompat.CATEGORY_MESSAGE)
    .setVisibility(NotificationCompat.VISIBILITY_PRIVATE)
    .setPublicVersion(hidden)
    .setContentIntent(openAppIntent(ctx))
    .setAutoCancel(true)
    .build()
  NotificationManagerCompat.from(ctx).notify(tag, 0, n)
}

/** Tokens of the Answer buttons on show. */
val answers: MutableSet<String> = java.util.Collections.synchronizedSet(HashSet())

/** An irch://call link without one of them only rings, asking first. */
fun answerable(i: Intent?) {
  if (i == null) return
  val u = i.data?.takeIf { it.scheme == "irch" } ?: return
  if (u.host != "call" && u.host != "ring") return
  val ok = u.host == "call" && answers.remove(u.getQueryParameter("k") ?: "")
  val verb = if (ok) "call" else u.host
  i.setData(Uri.parse("irch://$verb${u.path}"))
}

/**
 * Like a phone call: the ringtone and a long buzz, again and again, full
 * screen when locked, until answered, declined or 45 s pass. Answer joins
 * the call; the screen itself (or a tap) asks first.
 */
fun postCall(ctx: Context, space: String, room: String, who: String) {
  channel(ctx, CALLS, "Calls", Settings.System.DEFAULT_RINGTONE_URI,
    AudioAttributes.USAGE_NOTIFICATION_RINGTONE, longArrayOf(0, 1000, 1000))
  val tag = space + room
  val flags = PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
  val open = { verb: String, code: Int ->
    // Answering needs this notification's own token: a link from anywhere
    // else only rings (MainActivity), never opens the microphone.
    val k = java.util.UUID.randomUUID().toString()
    if (verb == "call") answers.add(k)
    val uri = Uri.parse("irch://$verb/$space/$room?k=$k")
    val i = Intent(Intent.ACTION_VIEW, uri, ctx, MainActivity::class.java)
      .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
    PendingIntent.getActivity(ctx, tag.hashCode() + code, i, flags)
  }
  val decline = PendingIntent.getBroadcast(ctx, tag.hashCode(),
    Intent(ctx, Decline::class.java).putExtra("tag", tag), flags)
  val caller = Person.Builder().setName(who).setImportant(true).build()
  val n = NotificationCompat.Builder(ctx, CALLS)
    .setSmallIcon(R.drawable.ic_stat)
    .setStyle(NotificationCompat.CallStyle
      .forIncomingCall(caller, decline, open("call", 1)))
    .setCategory(NotificationCompat.CATEGORY_CALL)
    .setFullScreenIntent(open("ring", 2), true)
    .setContentIntent(open("ring", 2))
    .setVisibility(NotificationCompat.VISIBILITY_PUBLIC)
    .setOngoing(true)
    .setTimeoutAfter(45_000)
    .build()
  n.flags = n.flags or Notification.FLAG_INSISTENT
  NotificationManagerCompat.from(ctx).notify(tag, CALL, n)
}

/** Every ringing call stops: answered or declined. */
fun hush(ctx: Context) {
  val nm = NotificationManagerCompat.from(ctx)
  nm.activeNotifications.filter { it.id == CALL }
    .forEach { nm.cancel(it.tag, CALL) }
}

class Decline : BroadcastReceiver() {
  override fun onReceive(ctx: Context, i: Intent) =
    NotificationManagerCompat.from(ctx).cancel(i.getStringExtra("tag"), CALL)
}
