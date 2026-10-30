package dev.irch.chat

import android.content.Intent
import android.graphics.Color
import android.os.Build
import android.os.Bundle
import android.view.View
import android.view.WindowManager
import android.webkit.WebView
import androidx.activity.OnBackPressedCallback
import androidx.activity.SystemBarStyle
import androidx.activity.enableEdgeToEdge
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat

class MainActivity : TauriActivity() {
  override fun onCreate(savedInstanceState: Bundle?) {
    // The UI is dark: light system-bar icons whatever the system theme.
    val bars = SystemBarStyle.dark(Color.TRANSPARENT)
    enableEdgeToEdge(bars, bars)
    Wake.release() // a push may have started irch first
    answerable(intent)
    overLock(ringing(intent))
    super.onCreate(savedInstanceState)
    // Edge to edge: inset the page by the bars, cutouts and keyboard.
    val root = findViewById<View>(android.R.id.content)
    ViewCompat.setOnApplyWindowInsetsListener(root) { view, insets ->
      val i = insets.getInsets(
        WindowInsetsCompat.Type.systemBars() or
          WindowInsetsCompat.Type.displayCutout() or
          WindowInsetsCompat.Type.ime()
      )
      view.setPadding(i.left, i.top, i.right, i.bottom)
      WindowInsetsCompat.CONSUMED
    }
  }

  override fun onNewIntent(intent: Intent) {
    answerable(intent)
    if (ringing(intent)) overLock(true)
    super.onNewIntent(intent)
  }

  private fun ringing(i: Intent?) = i?.data?.let {
    it.scheme == "irch" && (it.host == "ring" || it.host == "call")
  } == true

  /** A ringing call, or one under way: shown over the lock screen, the
   *  screen lit and kept on. Only then, never for the rest of irch. */
  fun overLock(on: Boolean) {
    if (Build.VERSION.SDK_INT >= 27) {
      setShowWhenLocked(on)
      setTurnScreenOn(on)
    } else {
      @Suppress("DEPRECATION")
      val f = WindowManager.LayoutParams.FLAG_SHOW_WHEN_LOCKED or
        WindowManager.LayoutParams.FLAG_TURN_SCREEN_ON
      if (on) window.addFlags(f) else window.clearFlags(f)
    }
    val keep = WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON
    if (on) window.addFlags(keep) else window.clearFlags(keep)
  }

  // Back closes what the page has open; else irch goes to the background,
  // as finishing would stop the node.
  override fun onWebViewCreate(webView: WebView) {
    val back = object : OnBackPressedCallback(true) {
      override fun handleOnBackPressed() {
        webView.evaluateJavascript("window.irchBack?.() === true") { handled ->
          if (handled != "true") moveTaskToBack(true)
        }
      }
    }
    onBackPressedDispatcher.addCallback(this, back)
  }
}
