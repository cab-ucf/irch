package dev.irch.chat

import android.content.Intent
import android.graphics.Color
import android.os.Bundle
import android.view.View
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
    Wake.release() // a push may have started irch without us: our turn
    answerable(intent)
    super.onCreate(savedInstanceState)
    // Edge to edge, the page would sit under the status and navigation bars,
    // cutouts and the keyboard: inset the web view's frame by all of them.
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
    super.onNewIntent(intent)
  }

  // Back closes what the page has open (dialog, menu, drawer). With nothing
  // open, irch moves to the background instead of finishing, which would
  // stop the node.
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
