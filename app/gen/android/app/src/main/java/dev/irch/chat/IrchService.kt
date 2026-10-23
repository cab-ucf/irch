package dev.irch.chat

import android.app.Service
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat

/**
 * Keeps the process, and so the node, running while irch is in the
 * background, so messages and mention notifications keep arriving. There is
 * no push server to wake it otherwise. Opt-in from Settings.
 */
class IrchService : Service() {
  override fun onBind(intent: Intent?): IBinder? = null

  override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
    val quiet = NotificationManagerCompat.IMPORTANCE_MIN
    notificationChannel(this, CONNECTED, "Background connection", quiet)
    val n = NotificationCompat.Builder(this, CONNECTED)
      .setSmallIcon(R.drawable.ic_stat)
      .setContentTitle("Connected")
      .setContentText("Receiving messages from your communities")
      .setContentIntent(openAppIntent(this))
      .setOngoing(true)
      .build()
    if (Build.VERSION.SDK_INT >= 34) {
      startForeground(1, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE)
    } else {
      startForeground(1, n)
    }
    return START_NOT_STICKY
  }

  // Swiping irch away quits it: the activity's exit shuts the node down.
  override fun onTaskRemoved(rootIntent: Intent?) = stopSelf()
}
